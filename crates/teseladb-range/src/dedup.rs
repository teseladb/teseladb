//! Retryable writes (FR-011, data model §3): a write with a [`DedupKey`] leaves in the same entry
//! the record `(lsid, txnNumber, stmtId) → mark · result` at its mark, in the range's local keys,
//! so its snapshots carry it and a split's copy keeps the mark (007). A retry finds the record and gets back the original result and mark. Every
//! [`PURGE_EVERY_MS`] of cluster time the leader looks for the records [`DEDUP_TTL_MS`] old or older
//! and proposes their deletion (`PurgeDedup`): applying cannot wait for a block from the disk, so
//! the replicas do not look for them.

use std::cell::RefCell;

use teseladb_hlc::Hlc;
use teseladb_lsm::{Kind, Lookup, Lsm, Scan, Step};

use crate::command::DedupKey;
use crate::sm::{dedup_interval, dedup_key};
use teseladb_types::GroupId;

/// How long a record is kept, in cluster time (FR-011).
pub const DEDUP_TTL_MS: u64 = 30 * 60_000;
/// How often the leader looks for expired records, in cluster time.
pub const PURGE_EVERY_MS: u64 = 60_000;
/// Records deleted by one `PurgeDedup` at most; the rest go in the next round.
pub const PURGE_BATCH: usize = 10_000;

const NEWEST: Hlc = Hlc { physical: u64::MAX, logical: u32::MAX };

/// The record of statement `key` in range `g`: its mark and result, `Some(Ok(None))` if there is
/// none, `None` while a block is being read and `Some(Err(()))` if the LSM failed.
pub fn lookup(
    lsm: &RefCell<Lsm>,
    g: GroupId,
    key: DedupKey,
) -> Option<Result<Option<(Hlc, Vec<u8>)>, ()>> {
    let k = dedup_key(g, key.lsid, key.txn_number, key.stmt_id);
    let view = lsm.borrow().view();
    let found = view.get(&mut lsm.borrow_mut(), &k, NEWEST);
    match found {
        Lookup::Hit(f) => match crate::sm::parse_record_value(&f.value) {
            Some((ts, result)) => Some(Ok(Some((ts, result.to_vec())))),
            None => Some(Err(())),
        },
        Lookup::NotFound => Some(Ok(None)),
        Lookup::Miss(_) => None,
        Lookup::Failed(_) => Some(Err(())),
    }
}

/// The cutoff of a round at cluster time `now_ms`: records at or below it are expired.
pub fn cutoff(now_ms: u64) -> Option<Hlc> {
    now_ms.checked_sub(DEDUP_TTL_MS).map(|p| Hlc { physical: p, logical: u32::MAX })
}

/// The leader's search for the expired records of a range.
pub struct PurgeScan {
    cursor: Scan,
    cutoff: Hlc,
    keys: Vec<Vec<u8>>,
}

impl PurgeScan {
    /// A search of range `g`'s records at or below `cutoff`.
    pub fn new(lsm: &RefCell<Lsm>, g: GroupId, cutoff: Hlc) -> PurgeScan {
        let (start, end) = dedup_interval(g);
        let cursor = lsm.borrow().view().scan(&start, &end, NEWEST);
        PurgeScan { cursor, cutoff, keys: Vec::new() }
    }

    /// Moves the search on: the keys found once it ends or has [`PURGE_BATCH`], `None` while a
    /// block is being read, `Some(Err(()))` if the LSM failed. Records written after the cutoff,
    /// and those already deleted, are not returned.
    pub fn step(&mut self, lsm: &RefCell<Lsm>) -> Option<Result<Vec<Vec<u8>>, ()>> {
        while self.keys.len() < PURGE_BATCH {
            match self.cursor.next(&mut lsm.borrow_mut()) {
                Step::Item(i) => {
                    if i.kind == Kind::Put && i.ts <= self.cutoff {
                        self.keys.push(i.key);
                    }
                }
                Step::Done => break,
                Step::Miss(_) => return None,
                Step::Failed(_) => return Some(Err(())),
            }
        }
        Some(Ok(std::mem::take(&mut self.keys)))
    }
}
