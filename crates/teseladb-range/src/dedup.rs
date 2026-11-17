//! Retryable writes (FR-011, data model §3): a write with a [`DedupKey`] leaves in the same entry
//! the record `(lsid, txnNumber, stmtId) → mark · result` at its mark, in the range's local keys,
//! so its snapshots carry it and a split's copy keeps the mark (007). A retry finds the record and
//! gets back the original result and mark. A session keeps in a range only the records of its
//! newest `txnNumber` there, as a shard of the reference server keeps one per session in
//! `config.transactions`: a statement of an older `txnNumber` without its record is too old
//! (`TransactionTooOld`, 225). Every [`PURGE_EVERY_MS`] of cluster time the leader looks for the
//! records [`DEDUP_TTL_MS`] old or older and those a newer `txnNumber` of their session replaced,
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

/// Sessions a leader remembers the newest `txnNumber` of; past it the map starts again empty.
const REMEMBERED_SESSIONS: usize = 1 << 16;

/// The newest `txnNumber` each session wrote with through this leader, or one above it: a
/// statement of a greater `txnNumber` has no record in the range and no newer one is there, so it
/// skips both reads of the LSM ([`lookup`], [`superseded`]). The leader notes a statement once it
/// passed them; a proposal that never applies leaves its number noted, which only sends the next
/// statements of the session to the reads. A session it does not remember reads them, as a new
/// leader's do, and so does any statement of the noted number or below.
#[derive(Debug, Default)]
pub(crate) struct NewestTxns(std::collections::HashMap<u128, i64>);

impl NewestTxns {
    /// Whether statement `key` is past every record its session has in the range.
    pub(crate) fn is_new(&self, key: &DedupKey) -> bool {
        self.0.get(&key.lsid).is_some_and(|newest| key.txn_number > *newest)
    }

    /// Notes statement `key`, which passed the reads or [`NewestTxns::is_new`].
    pub(crate) fn note(&mut self, key: &DedupKey) {
        if self.0.len() >= REMEMBERED_SESSIONS && !self.0.contains_key(&key.lsid) {
            self.0.clear();
        }
        let newest = self.0.entry(key.lsid).or_insert(key.txn_number);
        *newest = (*newest).max(key.txn_number);
    }
}

/// The searches [`superseded`] made since the process started (FR-011): a write whose session
/// the leader remembers makes none.
static SEARCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// [`superseded`]'s searches so far.
pub fn searches() -> u64 {
    SEARCHES.load(std::sync::atomic::Ordering::Relaxed)
}

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

/// The newest `txnNumber` session `key.lsid` has in range `g` above `key.txn_number`, if any:
/// `None` while a block is being read and `Some(Err(()))` if the LSM failed.
pub fn superseded(
    lsm: &RefCell<Lsm>,
    g: GroupId,
    key: DedupKey,
) -> Option<Result<Option<i64>, ()>> {
    let Some(next) = key.txn_number.checked_add(1) else { return Some(Ok(None)) };
    SEARCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let start = dedup_key(g, key.lsid, next, 0);
    let end = match key.lsid.checked_add(1) {
        Some(lsid) => dedup_key(g, lsid, i64::MIN, 0),
        None => dedup_interval(g).1,
    };
    let mut cursor = lsm.borrow().view().scan(&start, &end, NEWEST);
    let mut newest = None;
    loop {
        match cursor.next(&mut lsm.borrow_mut()) {
            Step::Item(i) if i.kind == Kind::Put => newest = parse(&i.key).map(|(_, t)| t),
            Step::Item(_) => {}
            Step::Done => return Some(Ok(newest)),
            Step::Miss(_) => return None,
            Step::Failed(_) => return Some(Err(())),
        }
    }
}

/// The session and `txnNumber` of record key `key`.
fn parse(key: &[u8]) -> Option<(u128, i64)> {
    let tail = key.len().checked_sub(LSID + TXN + STMT)?;
    let lsid = u128::from_be_bytes(key.get(tail..tail + LSID)?.try_into().ok()?);
    let txn = u64::from_be_bytes(key.get(tail + LSID..tail + LSID + TXN)?.try_into().ok()?);
    Some((lsid, (txn ^ (1 << 63)) as i64))
}

/// Bytes of a record key's session, `txnNumber` and statement ([`dedup_key`]).
const LSID: usize = 16;
const TXN: usize = 8;
const STMT: usize = 4;

/// The cutoff of a round at cluster time `now_ms`: records at or below it are expired.
pub fn cutoff(now_ms: u64) -> Option<Hlc> {
    now_ms.checked_sub(DEDUP_TTL_MS).map(|p| Hlc { physical: p, logical: u32::MAX })
}

/// The leader's search for the expired records of a range and those a newer `txnNumber` of
/// their session replaced.
pub struct PurgeScan {
    cursor: Scan,
    g: GroupId,
    cutoff: Hlc,
    keys: Vec<Vec<u8>>,
    /// The session being read and its records.
    session: Option<(u128, Vec<SessionRecord>)>,
    /// Where the next round starts when this one found a full batch: the first record of the
    /// session of its last key, since the records past the batch are left for it.
    resume: Option<Vec<u8>>,
}

/// A record of the session a purge reads: its key, its `txnNumber` and whether it expired.
struct SessionRecord {
    key: Vec<u8>,
    txn: i64,
    expired: bool,
}

impl PurgeScan {
    /// A search of range `g`'s records at or below `cutoff`, from record key `from` (a round
    /// that goes on where the last one left its batch, [`PurgeScan::resume`]) or from the first.
    pub fn new(lsm: &RefCell<Lsm>, g: GroupId, cutoff: Hlc, from: Option<&[u8]>) -> PurgeScan {
        let (first, end) = dedup_interval(g);
        let start =
            from.filter(|k| first.as_slice() <= *k && *k < end.as_slice()).unwrap_or(&first);
        let cursor = lsm.borrow().view().scan(start, &end, NEWEST);
        PurgeScan { cursor, g, cutoff, keys: Vec::new(), session: None, resume: None }
    }

    /// Where the next round starts, once [`PurgeScan::step`] gave a full batch: more records
    /// may be left past it, and the next round reads them at once instead of a period later
    /// (a range written faster than a batch per period kept every record past it). `None` when
    /// the search read every record.
    pub fn resume(&self) -> Option<&[u8]> {
        self.resume.as_deref()
    }

    /// The records of the session read so far that go: expired, or of a `txnNumber` below its
    /// newest.
    fn end_session(&mut self) {
        let Some((_, records)) = self.session.take() else { return };
        let newest = records.iter().map(|r| r.txn).max().unwrap_or(i64::MIN);
        let gone = records.into_iter().filter(|r| r.expired || r.txn < newest);
        self.keys.extend(gone.map(|r| r.key));
    }

    /// Moves the search on: the keys found once it ends or has [`PURGE_BATCH`], `None` while a
    /// block is being read, `Some(Err(()))` if the LSM failed. Records written after the cutoff
    /// that no newer `txnNumber` of their session replaced, and those already deleted, are not
    /// returned.
    pub fn step(&mut self, lsm: &RefCell<Lsm>) -> Option<Result<Vec<Vec<u8>>, ()>> {
        while self.keys.len() < PURGE_BATCH {
            match self.cursor.next(&mut lsm.borrow_mut()) {
                Step::Item(i) if i.kind == Kind::Put => {
                    let Some((lsid, txn)) = parse(&i.key) else { continue };
                    if self.session.as_ref().is_some_and(|(l, _)| *l != lsid) {
                        self.end_session();
                    }
                    let expired = i.ts <= self.cutoff;
                    let (_, records) = self.session.get_or_insert_with(|| (lsid, Vec::new()));
                    records.push(SessionRecord { key: i.key, txn, expired });
                }
                Step::Item(_) => {}
                Step::Done => {
                    self.end_session();
                    break;
                }
                Step::Miss(_) => return None,
                Step::Failed(_) => return Some(Err(())),
            }
        }
        // A session's records join at once: what passes the batch goes in the next round, which
        // reads again from the first record of the session of the batch's last key.
        if self.keys.len() >= PURGE_BATCH {
            self.keys.truncate(PURGE_BATCH);
            let last = self.keys.last().and_then(|k| parse(k));
            self.resume = last.map(|(lsid, _)| dedup_key(self.g, lsid, i64::MIN, 0));
        }
        Some(Ok(std::mem::take(&mut self.keys)))
    }
}
