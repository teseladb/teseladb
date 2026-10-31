//! What a node knows of the transactions its readers pushed (008 FR-003, SC-005): a reader that
//! meets an intent pushes its transaction once for every reader of the node. The push goes
//! [`PUSH_AHEAD_MS`] above the reader's mark, so the readers that come after it with a mark below
//! that read the version before the intent without pushing again; readers that meet the
//! transaction while its push is on the way wait for its answer.
//!
//! What the cache keeps stays true: a pending transaction pushed above a mark commits above it if
//! it ever commits, and a finished one does not change.

use std::collections::{BTreeMap, VecDeque};

use teseladb_hlc::Hlc;
use teseladb_range::intents::{TxnId, TxnRecord, TxnStatus, succ};
use teseladb_range::push::Known;

/// How far above a reader's mark its push goes: far below the uncertainty window (250 ms,
/// FR-008) and the clock's bound on disk (1 s ahead), and enough for the readers of the next
/// milliseconds to find the transaction already pushed above them.
pub const PUSH_AHEAD_MS: u64 = 10;
/// Transactions the cache remembers; the oldest goes first.
pub const CAPACITY: usize = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Entry {
    /// A push is on the way.
    InFlight,
    /// Pending, with its minimum commit mark at least this.
    PushedTo(Hlc),
    /// Committed at this mark.
    Committed(Hlc),
    /// Aborted.
    Aborted,
}

/// What a reader at a mark does about a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// It knows enough to read.
    Known(Known),
    /// Another reader's push is on the way: it waits for its answer.
    Wait,
    /// It pushes the transaction to this mark, and settles the answer with [`PushCache::settle`].
    Push(Hlc),
}

/// The pushes of one node.
#[derive(Debug, Default)]
pub struct PushCache {
    entries: BTreeMap<TxnId, Entry>,
    order: VecDeque<TxnId>,
}

impl PushCache {
    /// An empty cache.
    pub fn new() -> PushCache {
        PushCache::default()
    }

    /// What a reader at `at` does about `txn`: read with what the node knows, wait for a push on
    /// the way, or push it (marked on the way from here on).
    pub fn lookup(&mut self, txn: TxnId, at: Hlc) -> Lookup {
        match self.entries.get(&txn) {
            Some(Entry::InFlight) => return Lookup::Wait,
            Some(Entry::PushedTo(to)) if *to > at => return Lookup::Known(Known::Pushed),
            Some(Entry::Committed(ts)) => return Lookup::Known(Known::Committed(*ts)),
            Some(Entry::Aborted) => return Lookup::Known(Known::Aborted),
            Some(Entry::PushedTo(_)) | None => {}
        }
        self.put(txn, Entry::InFlight);
        Lookup::Push(ahead(at))
    }

    /// The answer of a push of `txn`: the record it left, or `None` if it failed (the next
    /// reader pushes again).
    pub fn settle(&mut self, txn: TxnId, record: Option<&TxnRecord>) {
        let entry = match record {
            None => {
                self.entries.remove(&txn);
                self.order.retain(|t| *t != txn);
                return;
            }
            Some(r) => match r.status {
                TxnStatus::Pending => Entry::PushedTo(r.min_commit_ts),
                TxnStatus::Committed => Entry::Committed(r.commit_ts),
                TxnStatus::Aborted => Entry::Aborted,
            },
        };
        self.put(txn, entry);
    }

    /// Transactions it remembers.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether it remembers none.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn put(&mut self, txn: TxnId, entry: Entry) {
        if self.entries.insert(txn, entry).is_none() {
            self.order.push_back(txn);
        }
        while self.order.len() > CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.entries.remove(&old);
            }
        }
    }
}

/// The mark a reader at `at` pushes to: [`PUSH_AHEAD_MS`] above it, and never at or below it.
pub fn ahead(at: Hlc) -> Hlc {
    succ(at).max(Hlc { physical: at.physical.saturating_add(PUSH_AHEAD_MS), logical: 0 })
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn ts(p: u64, l: u32) -> Hlc {
        Hlc { physical: p, logical: l }
    }

    fn record(status: TxnStatus, min_commit: Hlc, commit: Hlc) -> TxnRecord {
        TxnRecord {
            txn: TxnId(1),
            lsid: 0,
            txn_number: 0,
            status,
            read_ts: ts(1, 0),
            min_commit_ts: min_commit,
            commit_ts: commit,
            heartbeat_ts: ts(1, 0),
            spans: vec![],
        }
    }

    #[test]
    fn one_push_serves_the_readers_below_it() {
        let mut c = PushCache::new();
        let t = TxnId(1);
        // The first reader pushes ahead of its mark; the others wait for it.
        assert_eq!(c.lookup(t, ts(100, 3)), Lookup::Push(ts(110, 0)));
        assert_eq!(c.lookup(t, ts(101, 0)), Lookup::Wait);
        c.settle(t, Some(&record(TxnStatus::Pending, ts(110, 0), ts(0, 0))));
        // Below the pushed mark: read without a push; at or above it: push again.
        assert_eq!(c.lookup(t, ts(109, 9)), Lookup::Known(Known::Pushed));
        assert_eq!(c.lookup(t, ts(110, 0)), Lookup::Push(ts(120, 0)));
        // A failed push: the next reader pushes.
        c.settle(t, None);
        assert!(c.is_empty());
        assert_eq!(c.lookup(t, ts(111, 0)), Lookup::Push(ts(121, 0)));
        // Finished transactions stay as they ended.
        c.settle(t, Some(&record(TxnStatus::Committed, ts(121, 0), ts(130, 0))));
        assert_eq!(c.lookup(t, ts(500, 0)), Lookup::Known(Known::Committed(ts(130, 0))));
        c.settle(TxnId(2), Some(&record(TxnStatus::Aborted, ts(1, 0), ts(1, 0))));
        assert_eq!(c.lookup(TxnId(2), ts(500, 0)), Lookup::Known(Known::Aborted));
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn the_push_goes_above_the_mark_even_at_the_end_of_time() {
        assert_eq!(ahead(ts(u64::MAX, 0)), ts(u64::MAX, 1));
        assert_eq!(ahead(ts(5, u32::MAX)), ts(15, 0));
        assert_eq!(ahead(ts(u64::MAX - 1, u32::MAX)), ts(u64::MAX, 0));
    }

    #[test]
    fn the_oldest_transactions_go_first() {
        let mut c = PushCache::new();
        for i in 0..=CAPACITY as u128 {
            c.settle(TxnId(i), Some(&record(TxnStatus::Aborted, ts(1, 0), ts(1, 0))));
        }
        assert_eq!(c.len(), CAPACITY);
        assert_eq!(c.lookup(TxnId(0), ts(1, 0)), Lookup::Push(ts(11, 0)));
        assert_eq!(c.lookup(TxnId(CAPACITY as u128), ts(1, 0)), Lookup::Known(Known::Aborted));
    }
}
