//! The reaper of abandoned transactions (008 FR-007): each core goes over the pending records of
//! the ranges it leads and pushes every one without a heartbeat for
//! [`ABANDONED_AFTER_MS`](teseladb_range::intents::ABANDONED_AFTER_MS). A push that aborts the
//! record (or finds it finished) is followed by the resolution of its intents, range by range,
//! from the intervals the record keeps.

use std::collections::{BTreeMap, BTreeSet};

use teseladb_hlc::Hlc;
use teseladb_range::host::{RangeError, Reply, TxnOp};
use teseladb_range::intents::{Push, TxnId, TxnRecord, TxnStatus, record_key};
use teseladb_types::GroupId;

use crate::coordinator::{ReqNo, Request, Route};

/// What a request of the reaper was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Push(TxnId),
    Resolve(TxnId),
}

/// The reaper of one core.
#[derive(Debug, Default)]
pub struct Reaper {
    pending: BTreeMap<ReqNo, Step>,
    /// Transactions with a request in flight: a later scan does not push them again.
    busy: BTreeSet<TxnId>,
    /// Resolutions still in flight per transaction.
    resolving: BTreeMap<TxnId, usize>,
    out: Vec<Request>,
    next: ReqNo,
    reaped: u64,
}

/// The keys of an interval of a record: a run of keys each one the previous followed by `00`,
/// which is how the coordinator merges them. An interval of the keys that start with the first
/// bytes of a long key (`record::SPAN_KEY_BYTES`) gives none: the readers that meet its intents
/// resolve them, as they do any the reaper leaves.
fn keys_of((start, end): &(Vec<u8>, Vec<u8>)) -> Vec<Vec<u8>> {
    let chain = end.len() > start.len()
        && end.starts_with(start)
        && end[start.len()..].iter().all(|b| *b == 0);
    if !chain {
        return Vec::new();
    }
    let mut keys = Vec::new();
    let mut k = start.clone();
    while k < *end {
        keys.push(k.clone());
        k.push(0);
    }
    keys
}

impl Reaper {
    /// The requests to send.
    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.out)
    }

    /// Transactions it aborted.
    pub fn reaped(&self) -> u64 {
        self.reaped
    }

    /// Whether nothing is in flight.
    pub fn is_idle(&self) -> bool {
        self.pending.is_empty()
    }

    fn send(&mut self, g: GroupId, op: TxnOp, step: Step) {
        self.next += 1;
        self.pending.insert(self.next, step);
        self.out.push(Request { id: self.next, g, op });
    }

    /// Records found in a scan of the core's ranges at mark `now`: the abandoned pending ones are
    /// pushed.
    pub fn scan(
        &mut self,
        route: &dyn Route,
        now: Hlc,
        records: impl IntoIterator<Item = TxnRecord>,
    ) {
        for r in records {
            if !r.abandoned_at(now) || self.busy.contains(&r.txn) {
                continue;
            }
            let Some(g) = route.range_of(&record_key(r.txn)) else { continue };
            self.busy.insert(r.txn);
            self.send(
                g,
                TxnOp::PushTxn {
                    txn: r.txn,
                    push: Push::AbortIfAbandoned { intent_ts: r.heartbeat_ts },
                },
                Step::Push(r.txn),
            );
        }
    }

    /// The answer to request `id`.
    pub fn on_reply(&mut self, route: &dyn Route, id: ReqNo, r: Result<Reply, RangeError>) {
        match self.pending.remove(&id) {
            Some(Step::Push(txn)) => match r {
                Ok(Reply::Record(rec)) if rec.status.is_final() => self.resolve(route, &rec),
                // Alive after all, or the push failed: the next scan looks again.
                Ok(_) | Err(_) => {
                    self.busy.remove(&txn);
                }
            },
            Some(Step::Resolve(txn)) => {
                let left = self.resolving.get(&txn).map_or(0, |n| n.saturating_sub(1));
                if left == 0 {
                    self.resolving.remove(&txn);
                    self.busy.remove(&txn);
                } else {
                    self.resolving.insert(txn, left);
                }
            }
            None => {}
        }
    }

    fn resolve(&mut self, route: &dyn Route, rec: &TxnRecord) {
        if rec.status == TxnStatus::Aborted {
            self.reaped += 1;
        }
        let mut by_range: BTreeMap<GroupId, Vec<Vec<u8>>> = BTreeMap::new();
        for k in rec.spans.iter().flat_map(keys_of) {
            if let Some(g) = route.range_of(&k) {
                by_range.entry(g).or_default().push(k);
            }
        }
        if by_range.is_empty() {
            self.busy.remove(&rec.txn);
            return;
        }
        self.resolving.insert(rec.txn, by_range.len());
        for (g, keys) in by_range {
            let op = TxnOp::ResolveIntents {
                txn: rec.txn,
                status: rec.status,
                commit_ts: rec.commit_ts,
                keys,
            };
            self.send(g, op, Step::Resolve(rec.txn));
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::record::SPAN_KEY_BYTES;
    use crate::record::Spans;
    use teseladb_range::intents::ABANDONED_AFTER_MS;

    struct Two;

    impl Route for Two {
        fn range_of(&self, key: &[u8]) -> Option<GroupId> {
            match key {
                [0x01, b'a'..=b'l', ..] => Some(GroupId(1)),
                [0x01, ..] | [0x02, ..] => Some(GroupId(2)),
                _ => None,
            }
        }
    }

    fn ts(physical: u64) -> Hlc {
        Hlc { physical, logical: 0 }
    }

    fn record(txn: u128, beat: u64, keys: &[&[u8]]) -> TxnRecord {
        let mut spans = Spans::default();
        for k in keys {
            spans.add(k);
        }
        TxnRecord {
            txn: TxnId(txn),
            lsid: 0,
            txn_number: 0,
            status: TxnStatus::Pending,
            read_ts: ts(0),
            min_commit_ts: ts(0),
            commit_ts: ts(0),
            heartbeat_ts: ts(beat),
            spans: spans.intervals(),
        }
    }

    #[test]
    fn keys_come_back_from_merged_intervals() {
        let mut s = Spans::default();
        for k in [b"\x01a".as_slice(), b"\x01a\x00", b"\x01a\x00\x00", b"\x01q"] {
            s.add(k);
        }
        let intervals = s.intervals();
        assert_eq!(intervals.len(), 2);
        let keys: Vec<Vec<u8>> = intervals.iter().flat_map(keys_of).collect();
        assert_eq!(
            keys,
            [
                b"\x01a".to_vec(),
                b"\x01a\x00".to_vec(),
                b"\x01a\x00\x00".to_vec(),
                b"\x01q".to_vec()
            ]
        );
        assert!(!s.is_empty() && Spans::default().is_empty());
        // A long key goes as the interval of the keys that start with its first bytes, which
        // takes in the keys under it; the reaper leaves its intents to the readers.
        let long = |b: u8| [vec![1, b], vec![7; SPAN_KEY_BYTES]].concat();
        let mut s = Spans::default();
        s.add(&long(b'a'));
        s.add(&long(b'a')[..SPAN_KEY_BYTES]);
        s.add(b"\x01b");
        let cut = long(b'a')[..SPAN_KEY_BYTES].to_vec();
        let mut after = cut.clone();
        if let Some(last) = after.last_mut() {
            *last += 1;
        }
        assert_eq!(
            s.intervals(),
            vec![(cut.clone(), after.clone()), (b"\x01b".to_vec(), b"\x01b\x00".to_vec())]
        );
        assert!(keys_of(&(cut, after)).is_empty());
        // A long key of FF bytes only has no interval after it: it stays whole.
        let mut s = Spans::default();
        let top = vec![u8::MAX; SPAN_KEY_BYTES + 1];
        s.add(&top);
        assert_eq!(s.intervals(), vec![(top.clone(), [top.as_slice(), &[0]].concat())]);
        assert_eq!(keys_of(&s.intervals()[0]), vec![top]);
    }

    #[test]
    fn abandoned_records_are_pushed_aborted_and_resolved() {
        let mut r = Reaper::default();
        let now = ts(10_000);
        let alive = record(1, now.physical - 100, &[b"\x01a"]);
        let dead = record(2, now.physical - ABANDONED_AFTER_MS - 1, &[b"\x01b", b"\x01z"]);
        r.scan(&Two, now, [alive, dead.clone()]);
        let reqs = r.take_requests();
        assert_eq!(reqs.len(), 1, "only the abandoned one");
        assert_eq!(reqs[0].g, GroupId(2));
        let push = Push::AbortIfAbandoned { intent_ts: dead.heartbeat_ts };
        assert_eq!(reqs[0].op, TxnOp::PushTxn { txn: TxnId(2), push });
        // A second scan while it is in flight does not push it again.
        r.scan(&Two, now, [dead.clone()]);
        assert!(r.take_requests().is_empty());
        let aborted = TxnRecord { status: TxnStatus::Aborted, ..dead.clone() };
        r.on_reply(&Two, reqs[0].id, Ok(Reply::Record(aborted)));
        let resolves = r.take_requests();
        let mut ops: Vec<(GroupId, TxnOp)> = resolves.iter().map(|q| (q.g, q.op.clone())).collect();
        ops.sort_by_key(|(g, _)| *g);
        let resolve = |keys: &[&[u8]]| TxnOp::ResolveIntents {
            txn: TxnId(2),
            status: TxnStatus::Aborted,
            commit_ts: ts(0),
            keys: keys.iter().map(|k| k.to_vec()).collect(),
        };
        assert_eq!(ops, [(GroupId(1), resolve(&[b"\x01b"])), (GroupId(2), resolve(&[b"\x01z"]))]);
        assert_eq!(r.reaped(), 1);
        r.on_reply(&Two, resolves[0].id, Err(RangeError::Timeout));
        assert!(!r.is_idle());
        r.on_reply(&Two, resolves[1].id, Ok(Reply::Intents { ts: now }));
        assert!(r.is_idle());
        // Done: a later scan that still sees it pending pushes it again.
        r.scan(&Two, now, [dead]);
        assert_eq!(r.take_requests().len(), 1);
    }

    #[test]
    fn a_push_that_finds_it_alive_or_fails_lets_the_next_scan_try_again() {
        let mut r = Reaper::default();
        let now = ts(10_000);
        let dead = record(3, 0, &[b"\x01b"]);
        for answer in [Ok(Reply::Record(dead.clone())), Err(RangeError::Timeout)] {
            r.scan(&Two, now, [dead.clone()]);
            let reqs = r.take_requests();
            assert_eq!(reqs.len(), 1);
            r.on_reply(&Two, reqs[0].id, answer);
            assert!(r.take_requests().is_empty() && r.is_idle());
        }
        // An unknown answer is ignored.
        r.on_reply(&Two, 999, Ok(Reply::Intents { ts: now }));
        // A committed record (it committed meanwhile) has its intents resolved, not counted.
        r.scan(&Two, now, [dead.clone()]);
        let reqs = r.take_requests();
        let committed =
            TxnRecord { status: TxnStatus::Committed, commit_ts: ts(5), ..dead.clone() };
        r.on_reply(&Two, reqs[0].id, Ok(Reply::Record(committed)));
        assert_eq!(r.take_requests().len(), 1);
        assert_eq!(r.reaped(), 0);
        // A record whose keys no range covers, or with none, is done at once.
        let mut r = Reaper::default();
        let nowhere = record(4, 0, &[b"\x09x"]);
        r.scan(&Two, now, [nowhere.clone(), record(5, 0, &[])]);
        let reqs = r.take_requests();
        assert_eq!(reqs.len(), 2);
        for q in reqs {
            let TxnOp::PushTxn { txn, .. } = q.op else { panic!("a push") };
            let rec = TxnRecord { status: TxnStatus::Aborted, ..record(txn.0, 0, &[]) };
            let rec = if txn == TxnId(4) {
                TxnRecord { spans: nowhere.spans.clone(), ..rec }
            } else {
                rec
            };
            r.on_reply(&Two, q.id, Ok(Reply::Record(rec)));
        }
        assert!(r.take_requests().is_empty() && r.is_idle());
        // A record whose own key no range covers is skipped.
        struct None0;
        impl Route for None0 {
            fn range_of(&self, _: &[u8]) -> Option<GroupId> {
                None
            }
        }
        r.scan(&None0, now, [dead]);
        assert!(r.take_requests().is_empty());
    }
}
