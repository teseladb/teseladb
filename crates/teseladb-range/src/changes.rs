//! What a change stream reads of a range (014 FR-003, FR-004): a time scan of `(from, to]` of the
//! range's keys with every version and deletion, and the mark of the oldest intent still pending
//! there. A write goes above the range's closed mark, so with `to` at the closed mark no version
//! appears later in `(from, to]` but a pending intent's commit at its own mark: the resolved mark
//! stays below the oldest one (`ClosedTimestamp.tla` with `Streams`).

use std::cell::RefCell;

use teseladb_hlc::Hlc;
use teseladb_lsm::{Kind, Lsm, Step, TimeScan};

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

/// A change stream's read of a range in progress: the time scan and what it gave so far. A block
/// that is not in the cache stops it; the next [`ChangeScan::step`] goes on from there, so a
/// history larger than the cache is read to the end (scanning again from the first key, each
/// block read would push an earlier one out of the cache and the read would never end).
pub struct ChangeScan {
    cursor: TimeScan,
    out: ChangeRows,
}

impl std::fmt::Debug for ChangeScan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChangeScan").field("rows", &self.out.rows.len()).finish()
    }
}

impl ChangeScan {
    /// A read of `[start, end)` in `(from, to]`, on the LSM as it is now.
    pub fn new(
        lsm: &RefCell<Lsm>,
        (start, end): (&[u8], &[u8]),
        (from, to): (Hlc, Hlc),
    ) -> ChangeScan {
        let cursor = lsm.borrow().view().scan_time(start, end, from, to);
        ChangeScan { cursor, out: ChangeRows { rows: Vec::new(), oldest_intent: None } }
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
                    Kind::Delete => {
                        self.out.rows.push(Versioned { key: i.key, ts: i.ts, value: None });
                    }
                    Kind::Put => {
                        let Some((t, v)) = split_value(&i.value) else {
                            return Some(Err(ReadFault::Unavailable));
                        };
                        let value = Some((t, v.to_vec()));
                        self.out.rows.push(Versioned { key: i.key, ts: i.ts, value });
                    }
                    Kind::Retract => {}
                },
                Step::Done => {
                    let empty = ChangeRows { rows: Vec::new(), oldest_intent: None };
                    return Some(Ok(std::mem::replace(&mut self.out, empty)));
                }
                Step::Miss(_) => return None,
                Step::Failed(_) => return Some(Err(ReadFault::Unavailable)),
            }
        }
    }
}
