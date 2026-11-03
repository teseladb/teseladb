//! The reaper of abandoned transactions (008 FR-007): each core goes over the pending records of
//! the ranges it leads and pushes every one without a heartbeat for
//! [`ABANDONED_AFTER_MS`](teseladb_range::intents::ABANDONED_AFTER_MS). A push that aborts the
//! record (or finds it finished) is followed by the resolution of its intents, range by range,
//! from the intervals the record keeps.
//!
//! It also removes the records no one needs any more (record GC), as RefDb removes
//! `config.transactions` entries: a final record of an internal transaction a while after it ended,
//! and one of a client transaction once its session has expired ([`SESSION_TTL_MS`], RefDb's
//! `logicalSessionTimeoutMinutes`) or once its session has a later transaction: the same scan
//! shows one, or the record of the next number exists (identifiers come from the session and the
//! number, `recovery::client_txn_id`). RefDb keeps one `config.transactions` entry per session
//! the same way. A recovery of its commit after that answers as for a number the session never
//! began. Before the removal it resolves every intent the record covers again
//! (a no-op for those already resolved): the keys its intervals list, and for an interval it can
//! only give as a prefix (a key longer than the record keeps) the keys the ranges find with
//! `FindIntents`. Then the record forgets its intervals (`RecordOp::Resolved`) and goes
//! (`ForgetRecord`), which the range checks again under its latch.

use std::collections::{BTreeMap, BTreeSet};

use teseladb_hlc::Hlc;
use teseladb_range::host::{CONTINUES, RangeError, Reply, TxnOp, last_activity};
use teseladb_range::intents::{
    ABANDONED_AFTER_MS, Push, RecordOp, TxnId, TxnRecord, TxnStatus, record_key,
};
use teseladb_types::GroupId;

use crate::coordinator::{ReqNo, Request, Route};

/// How long a client transaction's record stays after its last activity: RefDb's default
/// `logicalSessionTimeoutMinutes`, 30 (measured on 9.0.4).
pub const SESSION_TTL_MS: u64 = 30 * 60_000;

/// The cut of a removal for a session that has a later transaction: no age is needed.
const NO_CUT: Hlc = Hlc { physical: u64::MAX, logical: u32::MAX };
/// The read mark of a record no coordinator began: a push created it.
const NEVER_BEGUN: Hlc = Hlc { physical: 0, logical: 0 };

/// What a request of the reaper was.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    Push(TxnId),
    Resolve(TxnId),
    /// A resolution before a record's removal; with the interval to search again after it, if it
    /// resolved what a search found.
    GcResolve(TxnId, Option<(Vec<u8>, Vec<u8>)>),
    /// A search of an interval's intents before a record's removal.
    Find(TxnId, (Vec<u8>, Vec<u8>)),
    /// The record forgets its intervals.
    Resolved(TxnId),
    /// A look for the record of the next transaction of a final record's session.
    Successor(Box<TxnRecord>),
    /// The record goes.
    Forget(TxnId),
}

/// A record on its way out.
#[derive(Debug)]
struct Removal {
    rec: TxnRecord,
    /// Its last activity has to stay below this mark.
    before: Hlc,
    /// Resolutions and searches in flight.
    waiting: usize,
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
    /// Records on their way out.
    removals: BTreeMap<TxnId, Removal>,
    forgotten: u64,
    /// Records removed since the last [`Reaper::take_forgotten`].
    gone: Vec<TxnId>,
    /// How long a client transaction's record stays ([`SESSION_TTL_MS`] if not set).
    session_ttl_ms: Option<u64>,
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
    /// A reaper whose client records stay `ms` after their last activity (the simulator's runs
    /// last seconds).
    pub fn with_session_ttl(ms: u64) -> Reaper {
        Reaper { session_ttl_ms: Some(ms), ..Reaper::default() }
    }

    /// Records it removed.
    pub fn forgotten(&self) -> u64 {
        self.forgotten
    }

    /// The transactions whose records it removed since the last call.
    pub fn take_forgotten(&mut self) -> Vec<TxnId> {
        std::mem::take(&mut self.gone)
    }

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
        let records: Vec<TxnRecord> = records.into_iter().collect();
        // Each session's last transaction among them.
        let mut last: BTreeMap<u128, i64> = BTreeMap::new();
        for r in records.iter().filter(|r| r.lsid != 0) {
            let n = last.entry(r.lsid).or_insert(r.txn_number);
            *n = (*n).max(r.txn_number);
        }
        for r in records {
            if self.busy.contains(&r.txn) {
                continue;
            }
            if r.status.is_final() {
                let later = r.lsid != 0 && last.get(&r.lsid).is_some_and(|n| *n > r.txn_number);
                match later {
                    true => self.start_removal(route, r, NO_CUT),
                    false => self.remove_if_old(route, now, r),
                }
                continue;
            }
            if !r.abandoned_at(now) {
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
            Some(Step::Successor(rec)) => match r {
                Ok(Reply::Record(_)) => {
                    self.busy.remove(&rec.txn);
                    self.start_removal(route, *rec, NO_CUT);
                }
                _ => {
                    self.busy.remove(&rec.txn);
                }
            },
            Some(Step::GcResolve(txn, again)) => self.gc_resolved(route, txn, r.is_ok(), again),
            Some(Step::Find(txn, span)) => self.found(route, txn, span, r),
            Some(Step::Resolved(txn)) => match (r, self.removals.get(&txn).map(|x| x.before)) {
                (Ok(Reply::Record(_)), Some(before)) => {
                    let Some(g) = route.range_of(&record_key(txn)) else {
                        return self.give_up(txn);
                    };
                    self.send(g, TxnOp::ForgetRecord { txn, before }, Step::Forget(txn));
                }
                _ => self.give_up(txn),
            },
            Some(Step::Forget(txn)) => {
                if r.is_ok() {
                    self.forgotten += 1;
                    self.gone.push(txn);
                }
                self.give_up(txn);
            }
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

    /// Starts the removal of final record `r` if it is old enough at `now`; a client one that is
    /// not looks for the record of its session's next transaction.
    fn remove_if_old(&mut self, route: &dyn Route, now: Hlc, r: TxnRecord) {
        // A record no coordinator began (a push created it, and its `Begin` and abort never
        // landed) may be a client transaction's: it stays as long as a session.
        let internal = r.lsid == 0 && r.read_ts != NEVER_BEGUN;
        let ttl = match internal {
            true => ABANDONED_AFTER_MS,
            false => self.session_ttl_ms.unwrap_or(SESSION_TTL_MS),
        };
        let before = now.physical.checked_sub(ttl).map(|cut| Hlc { physical: cut, logical: 0 });
        // Faulty variant (008): a client transaction's record goes as soon as it is final.
        #[cfg(teseladb_bug = "record_forgotten_early")]
        let before = before.or(Some(NO_CUT)).map(|_| NO_CUT);
        if let Some(before) = before.filter(|b| last_activity(&r) < *b) {
            return self.start_removal(route, r, before);
        }
        if r.lsid == 0 {
            return;
        }
        let next = crate::recovery::client_txn_id(r.lsid, r.txn_number.saturating_add(1));
        let Some(g) = route.range_of(&record_key(next)) else { return };
        self.busy.insert(r.txn);
        self.send(g, TxnOp::ReadRecord { txn: next }, Step::Successor(Box::new(r)));
    }

    /// Removes final record `r`, whose last activity has to stay below `before`: its intents are
    /// resolved again, then it forgets its intervals and goes.
    fn start_removal(&mut self, route: &dyn Route, r: TxnRecord, before: Hlc) {
        let txn = r.txn;
        self.busy.insert(txn);
        let spans = r.spans.clone();
        self.removals.insert(txn, Removal { rec: r, before, waiting: 0 });
        let mut by_range: BTreeMap<GroupId, Vec<Vec<u8>>> = BTreeMap::new();
        let mut prefixes = Vec::new();
        for span in spans {
            let keys = keys_of(&span);
            if keys.is_empty() {
                prefixes.push(span);
            }
            for k in keys {
                match route.range_of(&k) {
                    Some(g) => by_range.entry(g).or_default().push(k),
                    None => return self.give_up(txn),
                }
            }
        }
        for (g, keys) in by_range {
            self.gc_resolve(g, txn, keys, None);
        }
        for span in prefixes {
            self.find(route, txn, span);
        }
        self.gc_step(route, txn);
    }

    fn gc_resolve(
        &mut self,
        g: GroupId,
        txn: TxnId,
        keys: Vec<Vec<u8>>,
        again: Option<(Vec<u8>, Vec<u8>)>,
    ) {
        let Some(x) = self.removals.get_mut(&txn) else { return };
        x.waiting += 1;
        let (status, commit_ts) = (x.rec.status, x.rec.commit_ts);
        let op = TxnOp::ResolveIntents { txn, status, commit_ts, keys };
        self.send(g, op, Step::GcResolve(txn, again));
    }

    fn find(&mut self, route: &dyn Route, txn: TxnId, span: (Vec<u8>, Vec<u8>)) {
        let Some(g) = route.range_of(&span.0) else { return self.give_up(txn) };
        let Some(x) = self.removals.get_mut(&txn) else { return };
        x.waiting += 1;
        let op = TxnOp::FindIntents { txn, start: span.0.clone(), end: span.1.clone() };
        self.send(g, op, Step::Find(txn, span));
    }

    /// A resolution before a removal answered; one that resolved what a search found searches
    /// its interval again.
    fn gc_resolved(
        &mut self,
        route: &dyn Route,
        txn: TxnId,
        ok: bool,
        again: Option<(Vec<u8>, Vec<u8>)>,
    ) {
        if !ok {
            return self.give_up(txn);
        }
        if let Some(x) = self.removals.get_mut(&txn) {
            x.waiting -= 1;
        }
        if let Some(span) = again {
            self.find(route, txn, span);
        }
        self.gc_step(route, txn);
    }

    /// A search of `span` answered: the keys it found are resolved, then it searches again; an
    /// answer that says where the interval goes on searches there.
    fn found(
        &mut self,
        route: &dyn Route,
        txn: TxnId,
        span: (Vec<u8>, Vec<u8>),
        r: Result<Reply, RangeError>,
    ) {
        let Ok(Reply::Batch(rows)) = r else { return self.give_up(txn) };
        if let Some(x) = self.removals.get_mut(&txn) {
            x.waiting -= 1;
        }
        match rows.last() {
            None => {}
            Some((at, (_, v))) if v.as_slice() == CONTINUES => {
                let rest = (at.clone(), span.1);
                self.find(route, txn, rest);
            }
            Some(_) => {
                let keys: Vec<Vec<u8>> = rows.into_iter().map(|(k, _)| k).collect();
                let Some(g) = route.range_of(&keys[0]) else { return self.give_up(txn) };
                self.gc_resolve(g, txn, keys, Some(span));
            }
        }
        self.gc_step(route, txn);
    }

    /// Once nothing of a removal is in flight, the record forgets its intervals.
    fn gc_step(&mut self, route: &dyn Route, txn: TxnId) {
        if self.removals.get(&txn).is_none_or(|x| x.waiting > 0) {
            return;
        }
        let Some(g) = route.range_of(&record_key(txn)) else { return self.give_up(txn) };
        self.send(g, TxnOp::EndTxn { txn, op: RecordOp::Resolved }, Step::Resolved(txn));
    }

    /// A removal ends, done or not: the next scan looks at the record again.
    fn give_up(&mut self, txn: TxnId) {
        self.removals.remove(&txn);
        self.busy.remove(&txn);
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

    /// A final record ready to go: committed at `at`, of session `lsid` (0: internal), with the
    /// intervals of `keys`.
    fn finished(txn: u128, lsid: u128, at: u64, keys: &[&[u8]]) -> TxnRecord {
        let r = record(txn, at, keys);
        TxnRecord { status: TxnStatus::Committed, commit_ts: ts(at), lsid, read_ts: ts(1), ..r }
    }

    fn ops(reqs: &[Request]) -> Vec<(GroupId, TxnOp)> {
        let mut ops: Vec<(GroupId, TxnOp)> = reqs.iter().map(|q| (q.g, q.op.clone())).collect();
        ops.sort_by_key(|(g, _)| *g);
        ops
    }

    fn resolve(txn: u128, at: u64, keys: &[&[u8]]) -> TxnOp {
        TxnOp::ResolveIntents {
            txn: TxnId(txn),
            status: TxnStatus::Committed,
            commit_ts: ts(at),
            keys: keys.iter().map(|k| k.to_vec()).collect(),
        }
    }

    #[test]
    fn a_final_record_goes_once_old_enough_after_its_intents_are_resolved_again() {
        let mut r = Reaper::with_session_ttl(60_000);
        let now = ts(100_000);
        // Internal: a while after it ended. Client: once its session expired.
        let young = finished(1, 0, now.physical - 100, &[b"\x01a"]);
        let client = finished(2, 9, 10, &[b"\x01b"]);
        let fresh_client = finished(3, 9, now.physical - 59_000, &[b"\x01c"]);
        r.scan(&Two, now, [young, fresh_client.clone(), client.clone()]);
        let reqs = r.take_requests();
        let next = crate::recovery::client_txn_id(9, 1);
        assert_eq!(
            ops(&reqs),
            [
                (GroupId(1), resolve(2, client.commit_ts.physical, &[b"\x01b"])),
                (GroupId(2), TxnOp::ReadRecord { txn: next }),
            ],
            "the young client record looks for its session's next transaction"
        );
        let probe = reqs.iter().find(|q| matches!(q.op, TxnOp::ReadRecord { .. })).unwrap().id;
        r.on_reply(&Two, probe, Err(RangeError::NoSuchTransaction));
        let reqs: Vec<Request> =
            reqs.into_iter().filter(|q| !matches!(q.op, TxnOp::ReadRecord { .. })).collect();
        r.scan(&Two, now, [client.clone()]);
        assert!(r.take_requests().is_empty(), "already on its way out");
        r.on_reply(&Two, reqs[0].id, Ok(Reply::Intents { ts: now }));
        let reqs = r.take_requests();
        let resolved = TxnOp::EndTxn { txn: TxnId(2), op: RecordOp::Resolved };
        assert_eq!(ops(&reqs), [(GroupId(2), resolved)]);
        let forgetting = TxnRecord { spans: Vec::new(), ..client.clone() };
        r.on_reply(&Two, reqs[0].id, Ok(Reply::Record(forgetting)));
        let reqs = r.take_requests();
        let before = ts(now.physical - 60_000);
        assert_eq!(ops(&reqs), [(GroupId(2), TxnOp::ForgetRecord { txn: TxnId(2), before })]);
        r.on_reply(&Two, reqs[0].id, Ok(Reply::Intents { ts: now }));
        assert_eq!(r.forgotten(), 1);
        assert_eq!(r.take_forgotten(), vec![TxnId(2)]);
        assert!(r.take_forgotten().is_empty());
        assert!(r.is_idle());
        // A record whose intervals are already forgotten goes at once.
        let bare = TxnRecord { spans: Vec::new(), ..finished(4, 0, 10, &[]) };
        r.scan(&Two, now, [bare]);
        let reqs = r.take_requests();
        assert!(matches!(reqs[0].op, TxnOp::EndTxn { op: RecordOp::Resolved, .. }));
        // A failure lets the next scan start again.
        r.on_reply(&Two, reqs[0].id, Err(RangeError::Timeout));
        assert!(r.is_idle());
        r.scan(&Two, now, [TxnRecord { spans: Vec::new(), ..finished(4, 0, 10, &[]) }]);
        assert_eq!(r.take_requests().len(), 1);
    }

    #[test]
    fn the_intents_of_an_interval_given_as_a_prefix_are_searched_range_by_range() {
        let mut r = Reaper::default();
        let now = ts(100_000);
        let prefix = (b"\x01k".to_vec(), b"\x01l".to_vec());
        let rec = TxnRecord { spans: vec![prefix.clone()], ..finished(5, 0, 10, &[]) };
        r.scan(&Two, now, [rec]);
        let reqs = r.take_requests();
        let find = |start: &[u8], end: &[u8]| TxnOp::FindIntents {
            txn: TxnId(5),
            start: start.to_vec(),
            end: end.to_vec(),
        };
        assert_eq!(ops(&reqs), [(GroupId(1), find(b"\x01k", b"\x01l"))]);
        // Two keys found: resolved, then the interval is searched again.
        let row = |k: &[u8]| (k.to_vec(), (0, Vec::new()));
        r.on_reply(&Two, reqs[0].id, Ok(Reply::Batch(vec![row(b"\x01kx"), row(b"\x01ky")])));
        let reqs = r.take_requests();
        assert_eq!(ops(&reqs), [(GroupId(1), resolve(5, 10, &[b"\x01kx", b"\x01ky"]))]);
        r.on_reply(&Two, reqs[0].id, Ok(Reply::Intents { ts: now }));
        let reqs = r.take_requests();
        assert_eq!(ops(&reqs), [(GroupId(1), find(b"\x01k", b"\x01l"))]);
        // The range ends inside the interval: it goes on in the next one.
        let more = teseladb_range::host::continues(b"\x01m".to_vec());
        r.on_reply(&Two, reqs[0].id, Ok(Reply::Batch(vec![more])));
        let reqs = r.take_requests();
        assert_eq!(ops(&reqs), [(GroupId(2), find(b"\x01m", b"\x01l"))]);
        r.on_reply(&Two, reqs[0].id, Ok(Reply::Batch(Vec::new())));
        let reqs = r.take_requests();
        assert!(matches!(reqs[0].op, TxnOp::EndTxn { op: RecordOp::Resolved, .. }));
        // A failed search lets the next scan start again.
        let mut r = Reaper::default();
        r.scan(&Two, now, [TxnRecord { spans: vec![prefix], ..finished(6, 0, 10, &[]) }]);
        let reqs = r.take_requests();
        r.on_reply(&Two, reqs[0].id, Err(RangeError::Unavailable));
        assert!(r.is_idle());
    }

    #[test]
    fn a_record_goes_once_its_session_has_a_later_transaction() {
        let mut r = Reaper::default();
        let now = ts(100_000);
        let at = now.physical - 100;
        // The same scan shows the session's next transaction: the first goes at once.
        let first = TxnRecord { txn_number: 3, ..finished(10, 9, at, &[]) };
        let second = TxnRecord { txn_number: 4, status: TxnStatus::Pending, ..first.clone() };
        let second = TxnRecord { txn: TxnId(11), ..second };
        r.scan(&Two, now, [TxnRecord { spans: Vec::new(), ..first.clone() }, second]);
        let reqs = r.take_requests();
        let resolved = TxnOp::EndTxn { txn: TxnId(10), op: RecordOp::Resolved };
        assert_eq!(ops(&reqs), [(GroupId(2), resolved)]);
        r.on_reply(&Two, reqs[0].id, Ok(Reply::Record(first.clone())));
        let reqs = r.take_requests();
        let forget = TxnOp::ForgetRecord { txn: TxnId(10), before: NO_CUT };
        assert_eq!(ops(&reqs), [(GroupId(2), forget)]);
        // Alone in its scan: the record of the next number decides.
        let mut r = Reaper::default();
        let lone = TxnRecord { spans: Vec::new(), txn_number: 7, ..finished(12, 9, at, &[]) };
        r.scan(&Two, now, [lone.clone()]);
        let reqs = r.take_requests();
        let next = crate::recovery::client_txn_id(9, 8);
        assert_eq!(ops(&reqs), [(GroupId(2), TxnOp::ReadRecord { txn: next })]);
        r.scan(&Two, now, [lone.clone()]);
        assert!(r.take_requests().is_empty(), "already looking");
        let found = TxnRecord { txn: next, txn_number: 8, ..lone.clone() };
        r.on_reply(&Two, reqs[0].id, Ok(Reply::Record(found)));
        let reqs = r.take_requests();
        assert!(matches!(reqs[0].op, TxnOp::EndTxn { op: RecordOp::Resolved, .. }));
        // An internal record does not look for one.
        let mut r = Reaper::default();
        r.scan(&Two, now, [TxnRecord { spans: Vec::new(), ..finished(13, 0, at, &[]) }]);
        assert!(r.take_requests().is_empty());
    }

    /// A record a push created and no coordinator began has no session to say whose it is: it
    /// may be a client transaction's whose `Begin` and abort never landed, so it stays as long as
    /// a session, not the while of an internal one (`txn-bank` 996, `heavy`).
    #[test]
    fn a_record_no_coordinator_began_stays_as_long_as_a_session() {
        let mut r = Reaper::with_session_ttl(60_000);
        let now = ts(100_000);
        let pushed = TxnRecord {
            status: TxnStatus::Aborted,
            read_ts: NEVER_BEGUN,
            spans: Vec::new(),
            ..record(20, now.physical - 10_000, &[])
        };
        r.scan(&Two, now, [pushed.clone()]);
        assert!(r.take_requests().is_empty(), "younger than a session, and no session to look at");
        let old = TxnRecord { heartbeat_ts: ts(now.physical - 61_000), ..pushed };
        r.scan(&Two, now, [old]);
        let reqs = r.take_requests();
        let resolved = TxnOp::EndTxn { txn: TxnId(20), op: RecordOp::Resolved };
        assert_eq!(ops(&reqs), [(GroupId(2), resolved)]);
    }
}
