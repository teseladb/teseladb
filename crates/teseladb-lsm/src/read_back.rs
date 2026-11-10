//! Backward range reads (H-003): the keys of `[start, end)` from the last one down, one visible
//! version per key, as a forward [`Scan`](crate::Scan) gives them reversed.
//!
//! Each step finds the largest logical key below the current bound in every source (AnotherProduct's
//! `SeekForPrev`: a memtable answers from its ordered keys; a file from its block index, which
//! holds each block's last key, reading at most the one block the bound falls in) and then reads
//! that key as a point read does, which decides its versions, retractions, intents and range
//! tombstones. A read of the last `n` keys costs `n` such steps, not a read of the whole range.

use std::sync::Arc;

use teseladb_keys::Hlc;

use crate::entry;
use crate::lsm::Lsm;
use crate::read::{Blocks, Fetch, Item, Lookup, ReadToken, Step, corrupt, get_once};
use crate::sst::block::BlockCursor;
use crate::sst::reader::Table;
use crate::version::{Version, View};

/// A backward range read in progress: call [`BackScan::next`] until [`Step::Done`].
pub struct BackScan {
    version: Arc<Version>,
    start: Vec<u8>,
    /// Every key at or above it is behind the scan (excluded).
    bound: Vec<u8>,
    read: Hlc,
    /// The key found below the bound, waiting for its point read.
    pending: Option<Vec<u8>>,
    finished: bool,
    /// For each file of the version, in its order, the keys of the block the bound last fell in:
    /// the bound only goes down, so each block is read once per scan and then searched by
    /// bisection (H-003 of the native tests; AnotherProduct's `BlockIter::Prev` keeps the entries it
    /// decoded for the same reason).
    blocks: Vec<Option<Decoded>>,
    /// The largest logical key a step found so far, kept between steps so that each step
    /// copies into it instead of allocating one per source.
    best: Vec<u8>,
}

/// The internal keys of block `index` of a file, in order: one buffer and where each key ends,
/// two allocations per block instead of one per key, as a block iterator keeps its entries.
struct Decoded {
    index: usize,
    bytes: Vec<u8>,
    ends: Vec<usize>,
}

impl Decoded {
    fn len(&self) -> usize {
        self.ends.len()
    }

    fn key(&self, n: usize) -> &[u8] {
        let start = n.checked_sub(1).map_or(0, |p| self.ends[p]);
        &self.bytes[start..self.ends[n]]
    }
}

impl std::fmt::Debug for BackScan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackScan")
            .field("start", &self.start)
            .field("bound", &self.bound)
            .field("read", &self.read)
            .finish()
    }
}

impl View {
    /// A backward scan of `[start, end)` at `ts`: one visible version per key, from the last key
    /// down (H-003).
    pub fn scan_back(&self, start: &[u8], end: &[u8], ts: Hlc) -> BackScan {
        BackScan {
            version: self.version.clone(),
            start: start.to_vec(),
            bound: end.to_vec(),
            read: ts,
            pending: None,
            finished: start >= end,
            blocks: Vec::new(),
            best: Vec::new(),
        }
    }
}

/// What one file has below a bound.
enum InTable {
    /// Its last internal key below the bound, if any.
    Ready(Option<Last>),
    /// The block it needs is being read.
    Wait(ReadToken),
}

/// Where a file's last internal key below a bound is, read without copying it.
#[derive(Clone, Copy)]
enum Last {
    /// Key `n` of the block the file keeps decoded.
    Decoded(usize),
    /// The last key of block `n`, which the file's index holds.
    OfBlock(usize),
}

/// What the sources have below a bound.
enum Below {
    /// The largest logical key below the bound.
    Key(Vec<u8>),
    /// Nothing at or above the scan's start.
    Nothing,
    /// A block is being read, or the read failed.
    Wait(Step),
}

impl BackScan {
    /// The previous key's visible version.
    pub fn next(&mut self, lsm: &mut Lsm) -> Step {
        self.next_with(&mut lsm.reader)
    }

    /// [`BackScan::next`] reading its blocks through `r`, as a reader on another thread does.
    pub(crate) fn next_with<B: Blocks>(&mut self, r: &mut B) -> Step {
        r.begin();
        let step = self.step(r);
        if !matches!(step, Step::Miss(_)) {
            r.finish();
        }
        step
    }

    fn step<B: Blocks>(&mut self, r: &mut B) -> Step {
        loop {
            if let Some(e) = r.failure() {
                return Step::Failed(e.clone());
            }
            if self.finished {
                return Step::Done;
            }
            let key = match self.pending.take() {
                Some(k) => k,
                None => match self.below(r) {
                    Below::Key(k) => k,
                    Below::Nothing => {
                        self.finished = true;
                        return Step::Done;
                    }
                    Below::Wait(step) => return step,
                },
            };
            match get_once(r, &self.version, &key, self.read) {
                Lookup::Hit(f) => {
                    self.bound.clone_from(&key);
                    return Step::Item(Item { key, ts: f.ts, kind: f.kind, value: f.value });
                }
                // Deleted, retracted or newer than the read: the scan goes on below it.
                Lookup::NotFound => self.bound = key,
                Lookup::Miss(t) => {
                    self.pending = Some(key);
                    return Step::Miss(t);
                }
                Lookup::Failed(e) => return Step::Failed(e),
            }
        }
    }

    /// The largest logical key below the bound and at or above the start, over every source.
    fn below<B: Blocks>(&mut self, r: &mut B) -> Below {
        let target = entry::first_version(&self.bound);
        let mut found = false;
        let (start, best) = (&self.start, &mut self.best);
        let mut keep = |ikey: &[u8]| {
            let logical = entry::logical(ikey);
            if logical >= start.as_slice() && (!found || logical > best.as_slice()) {
                best.clear();
                best.extend_from_slice(logical);
                found = true;
            }
        };
        for m in &self.version.memtables {
            if let Some(ikey) = m.before(&target) {
                keep(ikey);
            }
        }
        let mut waiting = None;
        let tables = self.version.tables().count();
        if self.blocks.len() < tables {
            self.blocks.resize_with(tables, || None);
        }
        for (n, t) in self.version.tables().enumerate() {
            if t.props.lo.as_slice() >= self.bound.as_slice()
                || t.props.hi.as_slice() < self.start.as_slice()
                || t.props.min_ts > self.read
                || t.props.versions == 0
            {
                continue;
            }
            match table_before(r, t, &target, &mut self.blocks[n]) {
                Ok(InTable::Ready(Some(Last::Decoded(k)))) => {
                    keep(self.blocks[n].as_ref().expect("decoded just now").key(k));
                }
                Ok(InTable::Ready(Some(Last::OfBlock(b)))) => keep(t.last_key(b)),
                Ok(InTable::Ready(None)) => {}
                Ok(InTable::Wait(token)) => {
                    waiting.get_or_insert(token);
                }
                Err(e) => return Below::Wait(Step::Failed(e)),
            }
        }
        if let Some(token) = waiting {
            return Below::Wait(Step::Miss(token));
        }
        match found {
            true => Below::Key(self.best.clone()),
            false => Below::Nothing,
        }
    }
}

/// The last internal key of table `t` before `target`: from the block `target` falls in, or the
/// last key of the block before it, which the index holds. The block's keys stay in `cached` for
/// the next, lower, target.
fn table_before<B: Blocks>(
    r: &mut B,
    t: &Arc<Table>,
    target: &[u8],
    cached: &mut Option<Decoded>,
) -> Result<InTable, crate::LsmError> {
    let Some(i) = t.find_block(target) else {
        // Every key of the file is below the target: its last one.
        return Ok(InTable::Ready(t.blocks().checked_sub(1).map(Last::OfBlock)));
    };
    if cached.as_ref().is_none_or(|d| d.index != i) {
        let block = match r.block(t, i)? {
            Fetch::Ready(b) => b,
            Fetch::Wait(token) => return Ok(InTable::Wait(token)),
        };
        let mut c = BlockCursor::first(block).map_err(|_| corrupt(t, i))?;
        // The buffers of the block decoded before are reused.
        let (mut bytes, mut ends) =
            cached.take().map_or_else(|| (Vec::new(), Vec::new()), |d| (d.bytes, d.ends));
        bytes.clear();
        ends.clear();
        while c.valid() {
            bytes.extend_from_slice(c.key());
            ends.push(bytes.len());
            c.advance().map_err(|_| corrupt(t, i))?;
        }
        *cached = Some(Decoded { index: i, bytes, ends });
    }
    let Some(d) = cached.as_ref() else { return Ok(InTable::Ready(None)) };
    // The first key at or above the target, by bisection over the decoded keys.
    let (mut lo, mut hi) = (0, d.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if entry::compare(d.key(mid), target).is_lt() {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let last = match lo {
        0 => i.checked_sub(1).map(Last::OfBlock),
        n => Some(Last::Decoded(n - 1)),
    };
    Ok(InTable::Ready(last))
}
