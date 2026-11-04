//! What a change stream reads of a range (014 FR-003, FR-004): a time scan of `(from, to]` of the
//! range's keys with every version and deletion, and the mark of the oldest intent still pending
//! there. A write goes above the range's closed mark, so with `to` at the closed mark no version
//! appears later in `(from, to]` but a pending intent's commit at its own mark: the resolved mark
//! stays below the oldest one (`ClosedTimestamp.tla` with `Streams`).

use std::cell::RefCell;
use std::collections::BTreeMap;

use teseladb_hlc::Hlc;
use teseladb_lsm::{Kind, Lsm, Step, TimeScan};

use crate::intents::pred;
use crate::push::ReadFault;
use crate::sm::split_value;

/// A version a time scan gave: its key, its mark and its `(value type, value)`, or `None` for a
/// deletion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Versioned {
    /// Logical key.
    pub key: Vec<u8>,
    /// Mark.
    pub ts: Hlc,
    /// Value type and value, or `None` for a deletion.
    pub value: Option<(u8, Vec<u8>)>,
}

/// What a range gave a change stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeRows {
    /// The versions and deletions, in key order and, within a key, from the newest.
    pub rows: Vec<Versioned>,
    /// The mark of the oldest intent pending in the interval.
    pub oldest_intent: Option<Hlc>,
}

/// The versions of `[start, end)` in `(from, to]` and its oldest pending intent; `None` while a
/// block is being read (the LSM's I/O brings it and the caller asks again).
pub fn change_rows(
    lsm: &RefCell<Lsm>,
    interval: (&[u8], &[u8]),
    marks: (Hlc, Hlc),
) -> Option<Result<ChangeRows, ReadFault>> {
    ChangeScan::new(lsm, interval, marks).step(lsm)
}

/// Bytes of versions a read answers at most ([`ChangeScan::with_budget`]): far below the 16 MiB
/// of a frame between nodes, so that the answer of a range led elsewhere reaches its reader
/// whatever the size of the history.
pub const ANSWER_BYTES: usize = 4 << 20;
/// Bytes a version counts besides its key and value: its mark and its framing.
const ROW_OVERHEAD: usize = 24;

/// A change stream's read of a range in progress: the time scan and what it gave so far. A block
/// that is not in the cache stops it; the next [`ChangeScan::step`] goes on from there, so a
/// history larger than the cache is read to the end (scanning again from the first key, each
/// block read would push an earlier one out of the cache and the read would never end).
///
/// The answer keeps to a budget of bytes: once the versions passed it, the read gives up the
/// highest mark it holds, and every version above it, and answers up to the mark below
/// ([`ChangeScan::to`]); the reader asks again from there. A mark's versions go together (a
/// transaction's writes of the range share it), so the lowest mark goes whole even past the
/// budget.
pub struct ChangeScan {
    cursor: TimeScan,
    out: ChangeRows,
    to: Hlc,
    budget: usize,
    /// Bytes of the versions at or below `to`, by mark.
    by_mark: BTreeMap<Hlc, usize>,
    bytes: usize,
    /// Bytes of the versions above `to` still in `out`.
    stale: usize,
}

impl std::fmt::Debug for ChangeScan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChangeScan")
            .field("rows", &self.out.rows.len())
            .field("to", &self.to)
            .finish()
    }
}

impl ChangeScan {
    /// A read of `[start, end)` in `(from, to]`, on the LSM as it is now, within [`ANSWER_BYTES`].
    pub fn new(
        lsm: &RefCell<Lsm>,
        (start, end): (&[u8], &[u8]),
        (from, to): (Hlc, Hlc),
    ) -> ChangeScan {
        let cursor = lsm.borrow().view().scan_time(start, end, from, to);
        ChangeScan {
            cursor,
            out: ChangeRows { rows: Vec::new(), oldest_intent: None },
            to,
            budget: ANSWER_BYTES,
            by_mark: BTreeMap::new(),
            bytes: 0,
            stale: 0,
        }
    }

    /// The read with a budget of `bytes` instead of [`ANSWER_BYTES`].
    pub fn with_budget(self, bytes: usize) -> ChangeScan {
        ChangeScan { budget: bytes, ..self }
    }

    /// The mark the answer is complete up to: the one asked, or below it once the versions
    /// passed the budget.
    pub fn to(&self) -> Hlc {
        self.to
    }

    /// Keeps version `row` if its mark is still in the answer, then gives up the highest marks
    /// while the answer passes the budget.
    fn keep(&mut self, row: Versioned) {
        if row.ts > self.to {
            return;
        }
        let size = row.key.len() + row.value.as_ref().map_or(0, |(_, v)| v.len()) + ROW_OVERHEAD;
        *self.by_mark.entry(row.ts).or_insert(0) += size;
        self.bytes += size;
        self.out.rows.push(row);
        while self.bytes > self.budget && self.by_mark.len() > 1 {
            let Some((mark, size)) = self.by_mark.pop_last() else { break };
            self.bytes -= size;
            self.stale += size;
            self.to = pred(mark);
        }
        // The versions given up leave once they weigh as much as the answer.
        if self.stale > self.budget {
            let to = self.to;
            self.out.rows.retain(|r| r.ts <= to);
            self.stale = 0;
        }
    }

    /// The versions of the interval and its oldest pending intent; `None` while a block is being
    /// read (the LSM's I/O brings it and the caller steps again).
    pub fn step(&mut self, lsm: &RefCell<Lsm>) -> Option<Result<ChangeRows, ReadFault>> {
        loop {
            match self.cursor.next(&mut lsm.borrow_mut()) {
                Step::Item(i) => match i.kind {
                    Kind::Intent => {
                        let o = self.out.oldest_intent;
                        self.out.oldest_intent = Some(o.map_or(i.ts, |o| o.min(i.ts)));
                    }
                    Kind::Delete => self.keep(Versioned { key: i.key, ts: i.ts, value: None }),
                    Kind::Put => {
                        let Some((t, v)) = split_value(&i.value) else {
                            return Some(Err(ReadFault::Unavailable));
                        };
                        let value = Some((t, v.to_vec()));
                        self.keep(Versioned { key: i.key, ts: i.ts, value });
                    }
                    Kind::Retract => {}
                },
                Step::Done => {
                    let empty = ChangeRows { rows: Vec::new(), oldest_intent: None };
                    let mut out = std::mem::replace(&mut self.out, empty);
                    let to = self.to;
                    out.rows.retain(|r| r.ts <= to);
                    return Some(Ok(out));
                }
                Step::Miss(_) => return None,
                Step::Failed(_) => return Some(Err(ReadFault::Unavailable)),
            }
        }
    }
}
