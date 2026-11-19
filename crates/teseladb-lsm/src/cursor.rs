//! Cursors over the sources of a view, shared by scans and time scans: a memtable cursor
//! walks by key; a file cursor seeks through the index and loads its blocks through the
//! [`Reader`], waiting when one is not cached.

use std::sync::Arc;

use teseladb_keys::VERSION_LEN;
use teseladb_types::Bytes;

use crate::entry::{self, Kind};
use crate::error::LsmError;
use crate::memtable::{MemPlace, Memtable, Slot};
use crate::read::{Blocks, Fetch, ReadToken, corrupt};
use crate::sst::block::BlockCursor;
use crate::sst::reader::{COLUMN_SLOT, Table};

/// Where a file cursor is.
pub(crate) enum Pos {
    /// Must seek to an internal key.
    Seek(Vec<u8>),
    /// Must load a block and stand at its first entry.
    Block(usize),
    /// At an entry of block `.0`.
    Ready(usize, BlockCursor),
    /// At data block `block`'s column block (010 FR-033), whose `docs` documents a scan hands
    /// over whole; `at` orders it among the other sources, below every key of the block and
    /// above every key before it.
    Columns { block: usize, at: Vec<u8>, payload: Bytes, docs: usize },
    /// Past the end.
    Done,
}

/// A cursor over a memtable: it keeps its place in the memtable's tree and goes from a version
/// to the next without searching it from the root, reading the key where it is (FR-038). It
/// sees the versions the memtable had when it last sought, so the active memtable may take
/// writes or be frozen while it is open: a version written after that has a mark above the
/// read's, but for the one a resolution writes at its commit's mark, and the scan seeks again
/// when it meets the retraction the resolution leaves (008 FR-005).
pub(crate) struct MemCursor {
    mem: Arc<Memtable>,
    place: MemPlace,
    /// The kind and the value of the version it stands at, taken together when it got there: a
    /// version written again at its mark (an intent that is resolved) changes both at once, and
    /// a reader on another thread that took each on its own could pair the old kind with the
    /// new value.
    slot: Option<Slot>,
}

impl MemCursor {
    /// A cursor over `mem`, at no entry until it seeks.
    pub(crate) fn new(mem: Arc<Memtable>) -> MemCursor {
        MemCursor { mem, place: MemPlace::default(), slot: None }
    }

    /// Takes the kind and the value of the version its place stands at.
    fn settle(&mut self) {
        self.slot = self.mem.at(&self.place).map(|(_, slot)| slot.clone());
    }

    /// The internal key of the version it stands at.
    fn key(&self) -> Option<&[u8]> {
        self.slot.as_ref().and_then(|_| self.mem.at(&self.place)).map(|(key, _)| key)
    }
}

/// Whether the entry `c` moved to is another version of `logical`, a version of which the entry
/// before it was. With the whole logical key among the bytes it shares with that entry the keys
/// are not compared: an internal key is its logical key and a version of a fixed length.
fn another_version(c: &BlockCursor, logical: &[u8]) -> bool {
    let key = c.key();
    key.len() == logical.len() + VERSION_LEN
        && (c.shared() >= logical.len() || entry::logical(key) == logical)
}

/// What a cursor does with the data block it is about to read (FR-039, 010 FR-033).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Choice {
    /// Reads it.
    Read,
    /// Passes over it unread.
    Skip,
    /// Takes its column block instead, when the block has one that `fits`.
    Columns,
}

/// A cursor over one source.
pub(crate) enum Cursor {
    Mem(MemCursor),
    /// A file; `data` is the data file of a history file, where the bases of its deltas may
    /// be (FR-035), and `ahead` how the cursor reads the file ahead (FR-044).
    Table {
        table: Arc<Table>,
        data: Option<Arc<Table>>,
        pos: Pos,
        ahead: Readahead,
    },
}

/// Disk reads a file's cursor makes in order before it reads ahead (FR-044).
const READS_BEFORE_AHEAD: u32 = 2;
/// The first read ahead; each next one doubles it.
const FIRST_AHEAD: u64 = 8 << 10;
/// The largest read ahead.
const MOST_AHEAD: u64 = 256 << 10;

/// How a file's cursor reads ahead (FR-044): the disk reads it made in order, the bytes its next
/// one takes and the slot where that read must start to count as in order.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Readahead {
    reads: u32,
    bytes: u64,
    next: usize,
}

impl Default for Readahead {
    fn default() -> Readahead {
        Readahead { reads: 0, bytes: FIRST_AHEAD, next: usize::MAX }
    }
}

impl Readahead {
    /// The bytes a read of slot `slot` takes when it misses: none until the cursor has read the
    /// file in order [`READS_BEFORE_AHEAD`] times, or when `slot` does not follow its last read.
    fn size(&self, slot: usize) -> u64 {
        match self.reads >= READS_BEFORE_AHEAD && slot == self.next {
            true => self.bytes,
            false => 0,
        }
    }

    /// The cursor asked the disk for `blocks` slots from `slot` (none: it asked for nothing).
    /// A read that does not follow the last one starts the count again; one that read ahead
    /// doubles the next one, up to [`MOST_AHEAD`].
    fn asked(&mut self, slot: usize, blocks: usize) {
        if blocks == 0 {
            return;
        }
        if slot != self.next {
            *self = Readahead::default();
        } else if self.reads >= READS_BEFORE_AHEAD {
            self.bytes = (self.bytes * 2).min(MOST_AHEAD);
        }
        self.reads += 1;
        self.next = slot + blocks;
    }
}

impl Cursor {
    pub(crate) fn seek(&mut self, target: Vec<u8>) {
        match self {
            Cursor::Mem(m) => {
                m.mem.place(&target, &mut m.place);
                m.settle();
            }
            Cursor::Table { pos, .. } => *pos = Pos::Seek(target),
        }
    }

    /// Moves past the current entry (only a cursor at an entry advances).
    pub(crate) fn advance(&mut self) -> Result<(), LsmError> {
        match self {
            Cursor::Mem(m) => {
                m.mem.step(&mut m.place);
                m.settle();
            }
            Cursor::Table { table, pos, .. } => {
                let Pos::Ready(i, c) = pos else {
                    unreachable!("only a cursor at an entry advances")
                };
                c.advance().map_err(|_| corrupt(table, *i))?;
                if !c.valid() {
                    *pos = Pos::Block(*i + 1);
                }
            }
        }
        Ok(())
    }

    /// Whether the source may hold a version of a logical key in `[lo, hi]`: `false` is certain.
    pub(crate) fn may_hold(&self, lo: &[u8], hi: &[u8]) -> bool {
        match self {
            Cursor::Mem(m) => m
                .mem
                .seek(&entry::first_version(lo))
                .is_some_and(|(_, k, _)| entry::logical(k) <= hi),
            Cursor::Table { pos: Pos::Done, .. } => false,
            Cursor::Table { table, .. } => table.may_hold(lo, hi),
        }
    }

    /// Whether the cursor may still give a key of `[lo, hi]`, for a scan that stands before `lo`
    /// (010 FR-033): a cursor at an entry gives keys from it on, so one past `hi` gives none,
    /// and one about to load a block gives keys from that block's first on. A history file left
    /// closed (FR-031) counts when its file, another than `block_file`, sent it a deletion above
    /// the lowest mark of `block_file`: that deletion may hide a version of the block. Those of
    /// `block_file` itself are of keys its versions do not have.
    pub(crate) fn may_give(&self, lo: &[u8], hi: &[u8], block_file: &Table) -> bool {
        match self {
            Cursor::Mem(_) => self.may_hold(lo, hi),
            Cursor::Table { pos: Pos::Ready(_, c), .. } => entry::logical(c.key()) <= hi,
            Cursor::Table { table, pos: Pos::Block(i), .. } => {
                *i < table.blocks() && table.block_span(*i).0 <= hi
            }
            Cursor::Table { table, pos: Pos::Done, data: Some(file), .. } => {
                !std::ptr::eq(file.as_ref(), block_file)
                    && file.props.apart.is_some_and(|a| a > block_file.props.min_ts)
                    && table.may_hold(lo, hi)
            }
            Cursor::Table { pos: Pos::Done, .. } => false,
            Cursor::Table { table, .. } => table.may_hold(lo, hi),
        }
    }

    /// The column block the cursor stands at, which it leaves for the next data block.
    pub(crate) fn take_columns(&mut self) -> Option<(Arc<Table>, usize, Bytes, usize)> {
        let Cursor::Table { table, pos, .. } = self else { return None };
        let Pos::Columns { block, .. } = pos else { return None };
        let block = *block;
        let Pos::Columns { payload, docs, .. } = std::mem::replace(pos, Pos::Block(block + 1))
        else {
            unreachable!("checked")
        };
        Some((table.clone(), block, payload, docs))
    }

    /// Whether the cursor stands at a column block.
    pub(crate) fn at_columns(&self) -> bool {
        matches!(self, Cursor::Table { pos: Pos::Columns { .. }, .. })
    }

    /// Moves past the versions of `logical` from the current entry on, which is one of them. A
    /// file's cursor steps over up to `stepped` of them while they are in its block. At the
    /// block's end it goes on to the next block, unless the file's index says that block holds
    /// this key alone: then the index takes it to the block of the next key without reading
    /// the blocks that hold only older versions (a document updated often fills blocks with
    /// them). Past the versions it steps over, it searches its own block when the index says
    /// the next key is there, and the index otherwise. A memtable's cursor steps over as many
    /// and then seeks.
    pub(crate) fn skip_versions(&mut self, logical: &[u8], stepped: usize) -> Result<(), LsmError> {
        if let Cursor::Mem(m) = self {
            let mut past = false;
            for _ in 0..stepped {
                m.mem.step(&mut m.place);
                past = m.mem.at(&m.place).is_none_or(|(k, _)| entry::logical(k) != logical);
                if past {
                    break;
                }
            }
            if !past {
                m.mem.place(&entry::after_all_versions(logical), &mut m.place);
            }
            m.settle();
            return Ok(());
        }
        let Cursor::Table { table, pos, .. } = self else {
            unreachable!("a memtable's cursor returned above")
        };
        let Pos::Ready(i, c) = pos else {
            unreachable!("only a cursor at an entry passes versions")
        };
        let i = *i;
        for _ in 0..stepped {
            c.advance().map_err(|_| corrupt(table, i))?;
            if !c.valid() {
                let next = i + 1;
                let alone =
                    next < table.blocks() && entry::logical(table.last_key(next)) == logical;
                *pos = match alone {
                    true => Pos::Seek(entry::after_all_versions(logical)),
                    false => Pos::Block(next),
                };
                return Ok(());
            }
            if !another_version(c, logical) {
                return Ok(());
            }
        }
        let target = entry::after_all_versions(logical);
        if entry::compare(table.last_key(i), &target).is_lt() {
            // What is left of the block is this key's.
            *pos = Pos::Seek(target);
            return Ok(());
        }
        c.seek_forward(&target, entry::compare).map_err(|_| corrupt(table, i))?;
        if !c.valid() {
            // The index named a last key the block does not hold: the next block has it.
            *pos = Pos::Block(i + 1);
        }
        Ok(())
    }

    /// Loads what the cursor needs; `Some(token)` if it waits for a read.
    pub(crate) fn ready<B: Blocks>(&mut self, r: &mut B) -> Result<Option<ReadToken>, LsmError> {
        self.ready_choosing(r, &mut |_, _, _| Choice::Read, &|_| None)
    }

    /// [`Cursor::ready`] doing with each data block it is about to read what `choose` says,
    /// given the block and, when the cursor seeks into it, its target: passing over it unread
    /// (FR-039), or taking its column block when it has one and `fits` gives the number of
    /// documents of its payload (010 FR-033). Either way the cursor goes on at the next block.
    pub(crate) fn ready_choosing<B: Blocks>(
        &mut self,
        r: &mut B,
        choose: &mut dyn FnMut(&Table, usize, Option<&[u8]>) -> Choice,
        fits: &dyn Fn(&Bytes) -> Option<usize>,
    ) -> Result<Option<ReadToken>, LsmError> {
        let Cursor::Table { table, pos, ahead, .. } = self else {
            return Ok(None);
        };
        loop {
            match pos {
                Pos::Ready(..) | Pos::Columns { .. } | Pos::Done => return Ok(None),
                Pos::Seek(target) => {
                    let Some(i) = table.find_block(target) else {
                        *pos = Pos::Done;
                        continue;
                    };
                    match choose(table, i, Some(target)) {
                        Choice::Skip => {
                            *pos = Pos::Block(i + 1);
                            continue;
                        }
                        Choice::Columns => match columns_of(r, (table, ahead), i, fits)? {
                            Taken::Wait(t) => return Ok(Some(t)),
                            Taken::Columns(p) => {
                                *pos = p;
                                continue;
                            }
                            Taken::No => {}
                        },
                        Choice::Read => {}
                    }
                    let block = match block_of(r, (table, ahead), i)? {
                        Fetch::Ready(b) => b,
                        Fetch::Wait(t) => return Ok(Some(t)),
                    };
                    let c = BlockCursor::seek(block, target, entry::compare)
                        .map_err(|_| corrupt(table, i))?;
                    *pos = if c.valid() { Pos::Ready(i, c) } else { Pos::Block(i + 1) };
                }
                Pos::Block(i) => {
                    let i = *i;
                    if i >= table.blocks() {
                        *pos = Pos::Done;
                        continue;
                    }
                    match choose(table, i, None) {
                        Choice::Skip => {
                            *pos = Pos::Block(i + 1);
                            continue;
                        }
                        Choice::Columns => match columns_of(r, (table, ahead), i, fits)? {
                            Taken::Wait(t) => return Ok(Some(t)),
                            Taken::Columns(p) => {
                                *pos = p;
                                continue;
                            }
                            Taken::No => {}
                        },
                        Choice::Read => {}
                    }
                    let block = match block_of(r, (table, ahead), i)? {
                        Fetch::Ready(b) => b,
                        Fetch::Wait(t) => return Ok(Some(t)),
                    };
                    let c = BlockCursor::first(block).map_err(|_| corrupt(table, i))?;
                    *pos = if c.valid() { Pos::Ready(i, c) } else { Pos::Block(i + 1) };
                }
            }
        }
    }

    /// The current internal key, if any (after [`Cursor::ready`]).
    pub(crate) fn key(&self) -> Option<&[u8]> {
        match self {
            Cursor::Mem(m) => m.key(),
            Cursor::Table { pos: Pos::Ready(_, c), .. } => Some(c.key()),
            Cursor::Table { pos: Pos::Columns { at, .. }, .. } => Some(at),
            Cursor::Table { .. } => None,
        }
    }

    pub(crate) fn kind(&self) -> Kind {
        match self {
            Cursor::Mem(m) => match &m.slot {
                Some(slot) => slot.kind,
                None => unreachable!("a cursor without an entry has no kind"),
            },
            Cursor::Table { pos: Pos::Ready(_, c), .. } => c.kind(),
            Cursor::Table { .. } => unreachable!("a cursor without an entry has no kind"),
        }
    }

    /// The entry's value, or where it is in its value file (FR-034).
    pub(crate) fn stored(&self) -> Result<(Bytes, Option<Box<crate::read::Deferred>>), LsmError> {
        match self {
            Cursor::Mem(m) => match &m.slot {
                Some(slot) => Ok((slot.value.clone(), None)),
                None => unreachable!("a cursor without an entry has no value"),
            },
            Cursor::Table { table, data, pos: Pos::Ready(i, c), .. } => {
                crate::read::stored(table, data.as_ref(), *i, c)
            }
            Cursor::Table { .. } => unreachable!("a cursor without an entry has no value"),
        }
    }
}

/// What [`columns_of`] found.
enum Taken {
    /// The read of the column block to wait for.
    Wait(ReadToken),
    /// The cursor's place at the column block.
    Columns(Pos),
    /// No column block that fits: the data block is read.
    No,
}

/// Data block `i` of `table`, read ahead as `ahead` says (FR-044).
fn block_of<B: Blocks>(
    r: &mut B,
    (table, ahead): (&Arc<Table>, &mut Readahead),
    i: usize,
) -> Result<Fetch, LsmError> {
    let (fetch, asked) = r.block_ahead(table, i, ahead.size(i))?;
    ahead.asked(i, asked);
    Ok(fetch)
}

/// Data block `i`'s column block in `table`, when the reads give column blocks and it `fits`,
/// read ahead as `ahead` says (FR-044).
fn columns_of<B: Blocks>(
    r: &mut B,
    (table, ahead): (&Arc<Table>, &mut Readahead),
    i: usize,
    fits: &dyn Fn(&Bytes) -> Option<usize>,
) -> Result<Taken, LsmError> {
    let slot = COLUMN_SLOT + i;
    let (columns, asked) = r.columns_ahead(table, i, ahead.size(slot))?;
    ahead.asked(slot, asked);
    let payload = match columns {
        None => return Ok(Taken::No),
        Some(Err(t)) => return Ok(Taken::Wait(t)),
        Some(Ok(p)) => p,
    };
    let Some(docs) = fits(&payload) else { return Ok(Taken::No) };
    let at = match i {
        0 => entry::first_version(&table.props.lo),
        _ => entry::after_all_versions(entry::logical(table.last_key(i - 1))),
    };
    Ok(Taken::Columns(Pos::Columns { block: i, at, payload, docs }))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use teseladb_keys::Hlc;
    use teseladb_types::GroupId;

    use super::*;

    fn ts(p: u64) -> Hlc {
        Hlc { physical: p, logical: 0 }
    }

    /// The keys, marks and kinds a cursor gives from where it stands to the end.
    fn rest(c: &mut Cursor) -> Vec<(Vec<u8>, u64, Kind)> {
        let mut out = Vec::new();
        while let Some(k) = c.key() {
            let (logical, at) = entry::split(k);
            out.push((logical.to_vec(), at.physical, c.kind()));
            c.advance().expect("a memtable's cursor does not fail");
        }
        out
    }

    #[test]
    fn a_memtable_s_cursor_walks_versions_and_passes_over_those_of_a_key() {
        // FR-038: from a version to the next without a search from the root, the versions of a
        // key from the newest, and past all of them whether they are few or many.
        let m = Arc::new(Memtable::default());
        for p in 1..=12 {
            m.insert(GroupId(1), p, b"b", ts(p), Kind::Put, &p.to_be_bytes());
        }
        m.insert(GroupId(1), 13, b"a", ts(1), Kind::Put, b"x");
        m.insert(GroupId(1), 14, b"c", ts(2), Kind::Delete, b"");
        m.insert(GroupId(1), 15, b"c", ts(1), Kind::Put, b"y");
        let mut c = Cursor::Mem(MemCursor::new(m));
        assert!(c.key().is_none(), "at no entry until it seeks");
        c.seek(entry::first_version(b""));
        let all = rest(&mut c);
        assert_eq!(all.len(), 15);
        assert_eq!(all[0], (b"a".to_vec(), 1, Kind::Put));
        assert_eq!(all[1], (b"b".to_vec(), 12, Kind::Put), "the newest version first");
        assert_eq!(all[13..], [(b"c".to_vec(), 2, Kind::Delete), (b"c".to_vec(), 1, Kind::Put)]);
        // Twelve versions of `b`: more than it steps over, so it seeks past them.
        c.seek(entry::first_version(b"b"));
        c.skip_versions(b"b", 8).expect("a memtable's cursor does not fail");
        assert_eq!(c.key().map(|k| entry::split(k).0.to_vec()), Some(b"c".to_vec()));
        assert_eq!(c.stored().expect("a value").0.as_ref(), b"");
        // Two versions of `c`: it steps over them, and past the last key it stands nowhere.
        c.skip_versions(b"c", 8).expect("a memtable's cursor does not fail");
        assert!(c.key().is_none());
        c.seek(entry::first_version(b"a"));
        c.skip_versions(b"a", 8).expect("a memtable's cursor does not fail");
        assert_eq!(c.key().map(entry::split), Some((&b"b"[..], ts(12))));
    }

    #[test]
    fn a_memtable_s_cursor_keeps_the_kind_and_value_it_found_together() {
        // An intent resolved while a reader on another thread stands at it is written again at
        // its mark as a `Retract`: the reader gives the kind and the value of one of the two,
        // the one it found, and never the kind of one with the value of the other.
        let m = Arc::new(Memtable::default());
        m.insert(GroupId(1), 1, b"k", ts(5), Kind::Intent, b"intent");
        let mut c = Cursor::Mem(MemCursor::new(m.clone()));
        c.seek(entry::first_version(b"k"));
        m.insert(GroupId(1), 2, b"k", ts(5), Kind::Retract, b"");
        assert_eq!(c.kind(), Kind::Intent);
        assert_eq!(c.stored().expect("a value").0.as_ref(), b"intent");
        // A cursor that gets there afterwards finds the new pair.
        c.seek(entry::first_version(b"k"));
        assert_eq!((c.kind(), c.stored().expect("a value").0.len()), (Kind::Retract, 0));
    }
}
