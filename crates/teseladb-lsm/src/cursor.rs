//! Cursors over the sources of a view, shared by scans and time scans: a memtable cursor
//! walks by key; a file cursor seeks through the index and loads its blocks through the
//! [`Reader`], waiting when one is not cached.

use std::sync::Arc;

use teseladb_keys::VERSION_LEN;
use teseladb_types::Bytes;

use crate::entry::{self, Kind};
use crate::error::LsmError;
use crate::memtable::Memtable;
use crate::read::{Blocks, Fetch, ReadToken, corrupt};
use crate::sst::block::BlockCursor;
use crate::sst::reader::Table;

/// Where a file cursor is.
pub(crate) enum Pos {
    /// Must seek to an internal key.
    Seek(Vec<u8>),
    /// Must load a block and stand at its first entry.
    Block(usize),
    /// At an entry of block `.0`.
    Ready(usize, BlockCursor),
    /// Past the end.
    Done,
}

/// A cursor over a memtable: it walks by key, so the active memtable may take writes or be
/// frozen while it is open.
pub(crate) struct MemCursor {
    mem: Arc<Memtable>,
    pos: usize,
    key: Vec<u8>,
    kind: Kind,
    value: Bytes,
    valid: bool,
}

type Found = Option<(usize, Vec<u8>, Kind, Bytes)>;

impl MemCursor {
    /// A cursor over `mem`, at no entry until it seeks.
    pub(crate) fn new(mem: Arc<Memtable>) -> MemCursor {
        MemCursor {
            mem,
            pos: 0,
            key: Vec::new(),
            kind: Kind::Put,
            value: Bytes::new(),
            valid: false,
        }
    }

    fn set(&mut self, found: Found) {
        match found {
            Some((p, k, kd, v)) => {
                (self.pos, self.key, self.kind, self.value, self.valid) = (p, k, kd, v, true)
            }
            None => self.valid = false,
        }
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

/// A cursor over one source.
pub(crate) enum Cursor {
    Mem(MemCursor),
    /// A file; `data` is the data file of a history file, where the bases of its deltas may
    /// be (FR-035).
    Table {
        table: Arc<Table>,
        data: Option<Arc<Table>>,
        pos: Pos,
    },
}

impl Cursor {
    pub(crate) fn seek(&mut self, target: Vec<u8>) {
        match self {
            Cursor::Mem(m) => {
                let found =
                    m.mem.seek(&target).map(|(p, k, s)| (p, k.to_vec(), s.kind, s.value.clone()));
                m.set(found);
            }
            Cursor::Table { pos, .. } => *pos = Pos::Seek(target),
        }
    }

    /// Moves past the current entry (only a cursor at an entry advances).
    pub(crate) fn advance(&mut self) -> Result<(), LsmError> {
        match self {
            Cursor::Mem(m) => {
                let found = m
                    .mem
                    .next_after(m.pos, &m.key)
                    .map(|(p, k, s)| (p, k.to_vec(), s.kind, s.value.clone()));
                m.set(found);
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

    /// Moves past the versions of `logical` from the current entry on, which is one of them. A
    /// file's cursor steps over up to `stepped` of them while they are in its block. At the
    /// block's end it goes on to the next block, unless the file's index says that block holds
    /// this key alone: then the index takes it to the block of the next key without reading
    /// the blocks that hold only older versions (a document updated often fills blocks with
    /// them). Past the versions it steps over, it searches its own block when the index says
    /// the next key is there, and the index otherwise. A memtable's cursor seeks at once, since
    /// each of its steps copies a key.
    pub(crate) fn skip_versions(&mut self, logical: &[u8], stepped: usize) -> Result<(), LsmError> {
        let Cursor::Table { table, pos, .. } = self else {
            self.seek(entry::after_all_versions(logical));
            return Ok(());
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
        self.ready_skipping(r, &mut |_, _| false)
    }

    /// [`Cursor::ready`] passing over, without reading it, each data block that `skip` names
    /// (FR-039): the cursor goes on at the next block.
    pub(crate) fn ready_skipping<B: Blocks>(
        &mut self,
        r: &mut B,
        skip: &mut dyn FnMut(&Table, usize) -> bool,
    ) -> Result<Option<ReadToken>, LsmError> {
        let Cursor::Table { table, pos, .. } = self else {
            return Ok(None);
        };
        loop {
            match pos {
                Pos::Ready(..) | Pos::Done => return Ok(None),
                Pos::Seek(target) => {
                    let Some(i) = table.find_block(target) else {
                        *pos = Pos::Done;
                        continue;
                    };
                    if skip(table, i) {
                        *pos = Pos::Block(i + 1);
                        continue;
                    }
                    let block = match r.block(table, i)? {
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
                    if skip(table, i) {
                        *pos = Pos::Block(i + 1);
                        continue;
                    }
                    let block = match r.block(table, i)? {
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
            Cursor::Mem(m) => m.valid.then_some(m.key.as_slice()),
            Cursor::Table { pos: Pos::Ready(_, c), .. } => Some(c.key()),
            Cursor::Table { .. } => None,
        }
    }

    pub(crate) fn kind(&self) -> Kind {
        match self {
            Cursor::Mem(m) => m.kind,
            Cursor::Table { pos: Pos::Ready(_, c), .. } => c.kind(),
            Cursor::Table { .. } => unreachable!("a cursor without an entry has no kind"),
        }
    }

    /// The entry's value, or where it is in its value file (FR-034).
    pub(crate) fn stored(&self) -> Result<(Bytes, Option<Box<crate::read::Deferred>>), LsmError> {
        match self {
            Cursor::Mem(m) => Ok((m.value.clone(), None)),
            Cursor::Table { table, data, pos: Pos::Ready(i, c) } => {
                crate::read::stored(table, data.as_ref(), *i, c)
            }
            Cursor::Table { .. } => unreachable!("a cursor without an entry has no value"),
        }
    }
}
