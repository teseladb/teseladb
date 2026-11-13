//! The local keys of a range that a split hands to the new range (007 FR-022): its GC threshold,
//! its last mark and the whole idempotence table go to both halves, because a retry can reach
//! either and its record carries no data key to say which. A merge joins them again: the range
//! that absorbs takes the other's table and the greater threshold and mark. The new range's end is a local key as
//! well, so that a replica that restarts after the split opens each half with its own bounds
//! whatever Group 0 said when it opened.

use teseladb_hlc::Hlc;
use teseladb_types::GroupId;

use crate::command::{CopiedRecord, DedupKey};
use crate::sm::{
    dedup_key, encode_hlc, end_key, gc_threshold_key, last_mark_key, parse_record_value,
    record_value, record_value_reaching,
};

/// The local keys and values with which `new_range`, ending at `new_end`, starts: its parent's
/// GC threshold `gc`, last mark `last` and idempotence records `records`.
pub fn split_writes(
    new_range: GroupId,
    new_end: &[u8],
    gc: Hlc,
    last: Hlc,
    records: &[CopiedRecord],
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = vec![
        (gc_threshold_key(new_range), encode_hlc(gc).to_vec()),
        (last_mark_key(new_range), encode_hlc(last).to_vec()),
        (end_key(new_range), new_end.to_vec()),
    ];
    out.extend(records.iter().map(|r| {
        (
            dedup_key(new_range, r.key.lsid, r.key.txn_number, r.key.stmt_id),
            match &r.reach {
                Some(k) => record_value_reaching(r.mark, k, &r.result),
                None => record_value(r.mark, &r.result),
            },
        )
    }));
    out
}

/// The local keys and values a range takes when it absorbs its right neighbour (007 FR-022): its
/// new end `end`, the greater GC threshold `gc` and last mark `last` of the two, and the right
/// range's idempotence `records`, which join its own table: the same keys a split gives a new
/// range.
pub fn merge_writes(
    range: GroupId,
    end: &[u8],
    gc: Hlc,
    last: Hlc,
    records: &[CopiedRecord],
) -> Vec<(Vec<u8>, Vec<u8>)> {
    split_writes(range, end, gc, last, records)
}

/// Bytes of a record's local key before its statement: the local prefix of the range.
const RECORD_PREFIX: usize = 10;

/// The record of an idempotence record's local key and stored value; the key is the inverse of
/// [`dedup_key`].
pub fn record_of(key: &[u8], stored: &[u8]) -> Option<CopiedRecord> {
    let s = key.get(RECORD_PREFIX..)?;
    if s.len() != 28 {
        return None;
    }
    let lsid = u128::from_be_bytes(s[..16].try_into().ok()?);
    let txn = u64::from_be_bytes(s[16..24].try_into().ok()?) ^ (1 << 63);
    let stmt_id = u32::from_be_bytes(s[24..].try_into().ok()?);
    let (mark, result) = parse_record_value(stored)?;
    let reach = crate::sm::record_reach(stored).map(<[u8]>::to_vec);
    let key = DedupKey { lsid, txn_number: txn as i64, stmt_id };
    Some(CopiedRecord { key, mark, result: result.to_vec(), reach })
}

/// Whether a split at `at` gives record `r` to its new range (007 FR-022): a retry goes to the
/// range of its first key, so only a statement that wrote a key at or past `at` can be retried
/// there. A record that does not keep its greatest key goes, as before records kept it.
pub fn reaches(r: &CopiedRecord, at: &[u8]) -> bool {
    r.reach.as_deref().is_none_or(|k| k >= at)
}

/// The leader's read of a range's whole idempotence table before a split.
pub struct RecordScan {
    cursor: teseladb_lsm::Scan,
    records: Vec<CopiedRecord>,
}

impl RecordScan {
    /// A read of range `g`'s live records.
    pub fn new(lsm: &std::cell::RefCell<teseladb_lsm::Lsm>, g: GroupId) -> RecordScan {
        let (start, end) = crate::sm::dedup_interval(g);
        let newest = Hlc { physical: u64::MAX, logical: u32::MAX };
        RecordScan { cursor: lsm.borrow().view().scan(&start, &end, newest), records: Vec::new() }
    }

    /// Moves the read on: every record once it ends, `None` while a block is being read and
    /// `Some(Err(()))` if the LSM failed or a key is not a record.
    pub fn step(
        &mut self,
        lsm: &std::cell::RefCell<teseladb_lsm::Lsm>,
    ) -> Option<Result<Vec<CopiedRecord>, ()>> {
        loop {
            match self.cursor.next(&mut lsm.borrow_mut()) {
                // One visible version per key: a purged record does not show up.
                teseladb_lsm::Step::Item(i) => match record_of(&i.key, &i.value) {
                    Some(r) => self.records.push(r),
                    None => return Some(Err(())),
                },
                teseladb_lsm::Step::Done => return Some(Ok(std::mem::take(&mut self.records))),
                teseladb_lsm::Step::Miss(_) => return None,
                teseladb_lsm::Step::Failed(_) => return Some(Err(())),
            }
        }
    }
}

/// The leader's count of the live bytes of `[at, end)` before a split (D-068): the right half's
/// counter goes in `Split`, and the left half keeps the rest.
pub struct LiveScan {
    cursor: teseladb_lsm::Scan,
    bytes: u64,
    /// Faulty variant (007 T017): the intents it passes over, which the split drops.
    #[cfg(teseladb_bug = "split_loses_intents")]
    pub intents: Vec<(Vec<u8>, Hlc, Vec<u8>)>,
}

impl LiveScan {
    /// A count of `[start, end)` at its newest versions.
    pub fn new(lsm: &std::cell::RefCell<teseladb_lsm::Lsm>, start: &[u8], end: &[u8]) -> LiveScan {
        let newest = Hlc { physical: u64::MAX, logical: u32::MAX };
        LiveScan {
            cursor: lsm.borrow().view().scan(start, end, newest),
            bytes: 0,
            #[cfg(teseladb_bug = "split_loses_intents")]
            intents: Vec::new(),
        }
    }

    /// Moves the count on: the bytes once it ends, `None` while a block is being read and
    /// `Some(Err(()))` if the LSM failed. A version's bytes are those `Write` counts: its key, its
    /// type and its value.
    pub fn step(&mut self, lsm: &std::cell::RefCell<teseladb_lsm::Lsm>) -> Option<Result<u64, ()>> {
        loop {
            match self.cursor.next(&mut lsm.borrow_mut()) {
                // The stored value is the type followed by the value: the same bytes.
                teseladb_lsm::Step::Item(i) => {
                    self.bytes += (i.key.len() + i.value.len()) as u64;
                    #[cfg(teseladb_bug = "split_loses_intents")]
                    if i.kind == teseladb_lsm::Kind::Intent {
                        self.intents.push((i.key.clone(), i.ts, i.value.to_vec()));
                    }
                }
                teseladb_lsm::Step::Done => return Some(Ok(self.bytes)),
                teseladb_lsm::Step::Miss(_) => return None,
                teseladb_lsm::Step::Failed(_) => return Some(Err(())),
            }
        }
    }
}
