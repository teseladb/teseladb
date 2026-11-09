//! Reads that meet intents (008 T013, FR-003, FR-005): a read never waits for a transaction. An
//! intent at or below its mark stops it with the intent's owner until the reader knows the
//! transaction's state: pushed above the read (`PushTxn`, replicated), it reads the version below
//! the intent; committed at or below its mark, the intent's value; aborted, the version below. A
//! transaction reads its own intents. Once the reader resolves the intents of a finished
//! transaction (lazy resolution), every read sees the result without knowing anything.

mod txn_node;

use std::collections::BTreeMap;

use teseladb_hlc::Hlc;
use teseladb_range::host::{RangeError, ReadSpec, Reply, TxnOp, TxnReq};
use teseladb_range::intents::{
    IntentOp, IntentOwner, Push, RecordOp, TxnId, TxnStatus, record_key, succ,
};
use teseladb_range::push::{Known, TxnView};
use teseladb_range::read::ReadConcern;
use txn_node::*;
use teseladb_types::GroupId;

const C: &[u8] = b"\x01c";
const D: &[u8] = b"\x01d";

impl Node {
    fn read(&mut self, key: &[u8], at: Option<Hlc>, view: TxnView) -> Result<Reply, RangeError> {
        let spec = ReadSpec {
            g: G,
            key: key.to_vec(),
            scan: None,
            backward: false,
            at,
            concern: ReadConcern::Majority,
            view,
            uncertain_to: None,
        };
        let (now, wall) = (self.now, self.wall());
        let id = self.host.read_with(&mut self.core, spec, now, wall);
        self.answer(id)
    }

    fn scan_view(&mut self, at: Hlc, limit: usize, view: TxnView) -> Result<Reply, RangeError> {
        self.scan_dir(at, limit, view, false)
    }

    /// [`Node::scan_view`] from the last key down (H-003).
    fn scan_back(&mut self, at: Hlc, limit: usize, view: TxnView) -> Result<Reply, RangeError> {
        self.scan_dir(at, limit, view, true)
    }

    fn scan_dir(
        &mut self,
        at: Hlc,
        limit: usize,
        view: TxnView,
        backward: bool,
    ) -> Result<Reply, RangeError> {
        let spec = ReadSpec {
            g: G,
            key: b"\x01".to_vec(),
            scan: Some((b"\x02".to_vec(), limit)),
            backward,
            at: Some(at),
            concern: ReadConcern::Majority,
            view,
            uncertain_to: None,
        };
        let (now, wall) = (self.now, self.wall());
        let id = self.host.read_with(&mut self.core, spec, now, wall);
        self.answer(id)
    }
}

fn value(r: Result<Reply, RangeError>) -> Option<Vec<u8>> {
    match r {
        Ok(Reply::Value { value, .. }) => value.map(|(_, v)| v),
        other => panic!("not a value: {other:?}"),
    }
}

fn rows(r: Result<Reply, RangeError>) -> Vec<(Vec<u8>, Vec<u8>)> {
    match r {
        Ok(Reply::Batch(rows)) => rows.into_iter().map(|(k, (_, v))| (k, v)).collect(),
        other => panic!("not rows: {other:?}"),
    }
}

fn knowing(txn: TxnId, k: Known) -> TxnView {
    TxnView { own: None, known: BTreeMap::from([(txn, k)]) }
}

#[test]
fn a_read_that_meets_a_pending_intent_pushes_it_and_reads_below() {
    let mut n = Node::new();
    let old = n.write(A, b"old");
    let at = intents_mark(n.txn(TxnOp::PutIntents {
        txn: T1,
        read_ts: old,
        writes: vec![(A.to_vec(), put(b"new"))],
    }));
    // Below the intent nothing changes and nothing is pushed.
    assert_eq!(value(n.read(A, Some(old), TxnView::default())), Some(b"old".to_vec()));
    // At or above it, the read stops with the owner and its own mark.
    let owner = IntentOwner { txn: T1, record_key: record_key(T1), key: A.to_vec(), ts: at };
    let read_at = Hlc { physical: at.physical + 1, logical: 0 };
    assert_eq!(
        n.read(A, Some(read_at), TxnView::default()),
        Err(RangeError::ReadIntent { owner, at: read_at })
    );
    // The reader pushes the transaction above its mark, which the record keeps.
    let push = TxnOp::PushTxn { txn: T1, push: Push::To(succ(read_at)) };
    let Ok(Reply::Record(rec)) = n.txn(push) else { panic!("no record") };
    assert_eq!((rec.status, rec.min_commit_ts), (TxnStatus::Pending, succ(read_at)));
    assert_eq!(value(n.read(A, Some(read_at), knowing(T1, Known::Pushed))), Some(b"old".to_vec()));
    // The coordinator adopts the push: its commit goes above the read.
    let Ok(Reply::Record(rec)) = n.txn(TxnOp::EndTxn {
        txn: T1,
        op: RecordOp::Begin { lsid: 1, txn_number: 1, read_ts: old },
    }) else {
        panic!("no record")
    };
    let commit = RecordOp::Commit { ts: at, spans: vec![] };
    let Ok(Reply::Record(done)) = n.txn(TxnOp::EndTxn { txn: T1, op: commit }) else {
        panic!("no record")
    };
    assert_eq!(rec.min_commit_ts, succ(read_at));
    assert!(done.commit_ts > read_at, "the commit went above the pushed read");
    // Committed above the read: still the old version; at or below it: the intent's value.
    let committed = Known::Committed(done.commit_ts);
    assert_eq!(value(n.read(A, Some(read_at), knowing(T1, committed))), Some(b"old".to_vec()));
    let after = done.commit_ts;
    assert_eq!(value(n.read(A, Some(after), knowing(T1, committed))), Some(b"new".to_vec()));
    // Lazy resolution: then nobody needs to know.
    intents_mark(n.txn(TxnOp::ResolveIntents {
        txn: T1,
        status: TxnStatus::Committed,
        commit_ts: done.commit_ts,
        keys: vec![A.to_vec()],
    }));
    assert_eq!(value(n.read(A, None, TxnView::default())), Some(b"new".to_vec()));
    assert_eq!(n.get(A), Some(b"new".to_vec()));
    assert_eq!(value(n.read(A, Some(read_at), TxnView::default())), Some(b"old".to_vec()));
}

#[test]
fn aborted_intents_are_skipped_and_own_intents_are_read() {
    let mut n = Node::new();
    n.write(B, b"b");
    n.delete(B);
    let old = n.write(A, b"old");
    let live = n.live();
    let at = intents_mark(n.txn(intents(T1, old, &[(A, IntentOp::Delete), (C, put(b"c"))])));
    assert_eq!(n.live(), live, "intents are not live data");
    assert_eq!(value(n.read(B, None, knowing(T1, Known::Aborted))), None);
    let read_at = Hlc { physical: at.physical + 5, logical: 0 };
    assert_eq!(value(n.read(A, Some(read_at), knowing(T1, Known::Aborted))), Some(b"old".to_vec()));
    assert_eq!(value(n.read(C, Some(read_at), knowing(T1, Known::Aborted))), None);
    // The transaction itself, reading at its read mark below its intents, sees them.
    let own = TxnView { own: Some(T1), known: BTreeMap::new() };
    assert_eq!(value(n.read(A, Some(old), own.clone())), None, "its own deletion");
    assert_eq!(value(n.read(C, Some(old), own.clone())), Some(b"c".to_vec()));
    // Another transaction's own view does not cover it.
    let other = TxnView { own: Some(T2), known: BTreeMap::new() };
    assert!(matches!(n.read(C, Some(read_at), other), Err(RangeError::ReadIntent { .. })));
}

#[test]
fn scans_apply_the_same_rules_key_by_key() {
    let mut n = Node::new();
    n.write(A, b"a0");
    n.write(B, b"b0");
    let old = n.write(D, b"d0");
    intents_mark(n.txn(TxnOp::PutIntents {
        txn: T1,
        read_ts: old,
        writes: vec![(B.to_vec(), put(b"b1")), (C.to_vec(), put(b"c1"))],
    }));
    let at = intents_mark(n.txn(TxnOp::PutIntents {
        txn: T2,
        read_ts: old,
        writes: vec![(D.to_vec(), IntentOp::Delete)],
    }));
    let read_at = Hlc { physical: at.physical + 5, logical: 0 };
    // Knowing nothing, the scan stops at the first intent, with its mark.
    let r = n.scan_view(read_at, 10, TxnView::default());
    let stopped = matches!(r, Err(RangeError::ReadIntent { owner: IntentOwner { txn: T1, .. }, at })
        if at == read_at);
    assert!(stopped, "{r:?}");
    let view = TxnView {
        own: None,
        known: BTreeMap::from([(T1, Known::Committed(at)), (T2, Known::Pushed)]),
    };
    let got = rows(n.scan_view(read_at, 10, view.clone()));
    let want = [(A, b"a0"), (B, b"b1"), (C, b"c1"), (D, b"d0")];
    let want: Vec<_> = want.iter().map(|(k, v)| (k.to_vec(), v.to_vec())).collect();
    assert_eq!(got, want);
    assert_eq!(rows(n.scan_view(read_at, 2, view.clone())).len(), 2);
    // Backward (H-003): the same documents from the last one down, and its limit takes the last.
    let back = |w: &[(Vec<u8>, Vec<u8>)]| w.iter().rev().cloned().collect::<Vec<_>>();
    assert_eq!(rows(n.scan_back(read_at, 10, view.clone())), back(&want));
    assert_eq!(rows(n.scan_back(read_at, 2, view)), back(&want)[..2]);
    // Knowing nothing, a backward scan stops at the last intent first, T2's deletion.
    let r = n.scan_back(read_at, 10, TxnView::default());
    let stopped =
        matches!(r, Err(RangeError::ReadIntent { owner: IntentOwner { txn: T2, .. }, .. }));
    assert!(stopped, "{r:?}");
    // Transaction 1, at a mark above its intents, finds them in the scan itself.
    let own = TxnView { own: Some(T1), known: BTreeMap::from([(T2, Known::Pushed)]) };
    let got = rows(n.scan_view(read_at, 10, own.clone()));
    assert_eq!(got, want);
    assert_eq!(rows(n.scan_back(read_at, 10, own)), back(&want));
    // Transaction 2 reads its own deletion and, below its intents, T1's are above it: unseen.
    let own = TxnView { own: Some(T2), known: BTreeMap::new() };
    let got = rows(n.scan_view(old, 10, own.clone()));
    let want: Vec<_> =
        [(A, b"a0"), (B, b"b0")].iter().map(|(k, v)| (k.to_vec(), v.to_vec())).collect();
    assert_eq!(got, want);
    assert_eq!(rows(n.scan_back(old, 10, own)), back(&want));
    // A scan that ends past the range, or on a range this node lacks, is not its to answer.
    for (g, end) in [(G, [END, b"\x01"].concat()), (GroupId(99), b"\x02".to_vec())] {
        let spec = ReadSpec {
            g,
            key: A.to_vec(),
            scan: Some((end, 10)),
            backward: false,
            at: Some(read_at),
            concern: ReadConcern::Majority,
            view: TxnView::default(),
            uncertain_to: None,
        };
        let (now, wall) = (n.now, n.wall());
        let id = n.host.read_with(&mut n.core, spec, now, wall);
        assert_eq!(n.answer(id), Err(RangeError::OutOfRange));
    }
}

#[test]
fn a_read_at_a_mark_ahead_of_the_leaders_clock_waits_for_it() {
    let mut n = Node::new();
    n.write(A, b"v");
    // 100 ms ahead: answered once the node's clock passes the mark, and later writes go above.
    let ahead = Hlc { physical: n.wall() + 100, logical: 0 };
    let spec = ReadSpec {
        g: G,
        key: A.to_vec(),
        scan: None,
        backward: false,
        at: Some(ahead),
        concern: ReadConcern::Majority,
        view: TxnView::default(),
        uncertain_to: None,
    };
    let (now, wall) = (n.now, n.wall());
    let id = n.host.read_with(&mut n.core, spec.clone(), now, wall);
    n.settle();
    assert!(n.replies.iter().all(|(r, _)| *r != id), "answered before the clock reached the mark");
    let start = n.wall();
    while n.replies.iter().all(|(r, _)| *r != id) {
        n.settle();
        assert!(n.wall() < start + 1_000, "never answered");
    }
    assert!(n.wall() >= ahead.physical);
    assert_eq!(value(n.answer(id)), Some(b"v".to_vec()));
    assert!(n.write(A, b"w") > ahead);
    // Further ahead than the clocks of a cluster can differ: refused at once.
    let far = ReadSpec { at: Some(far()), ..spec };
    let (now, wall) = (n.now, n.wall());
    let id = n.host.read_with(&mut n.core, far, now, wall);
    assert_eq!(n.answer(id), Err(RangeError::Timeout));
}

/// A read whose mark came from another node's clock checks its uncertainty window (008 FR-008,
/// T023): a version of another writer in `(mark, window end]` stops it with that version's mark,
/// in a point read and in a scan; above the window, or without one, it reads below as before, and
/// its own transaction's intents are no other writer's.
#[test]
fn a_version_in_the_uncertainty_window_stops_the_read() {
    let mut n = Node::new();
    let old = n.write(A, b"old");
    let new = n.write(A, b"new");
    let mark = Hlc { physical: new.physical.saturating_sub(1), logical: 0 }.max(old);
    let read = |n: &mut Node, scan: bool, window: Option<Hlc>, view: TxnView| {
        let spec = ReadSpec {
            g: G,
            key: if scan { b"\x01".to_vec() } else { A.to_vec() },
            scan: scan.then(|| (b"\x02".to_vec(), 10)),
            backward: false,
            at: Some(mark),
            concern: ReadConcern::Majority,
            view,
            uncertain_to: window,
        };
        let (now, wall) = (n.now, n.wall());
        let id = n.host.read_with(&mut n.core, spec, now, wall);
        n.answer(id)
    };
    let window = Some(Hlc { physical: new.physical + 250, logical: u32::MAX });
    for scan in [false, true] {
        assert_eq!(
            read(&mut n, scan, window, TxnView::default()),
            Err(RangeError::ReadUncertain { ts: new }),
            "scan {scan}"
        );
    }
    // Without a window, or one that ends below the version: the version before.
    assert_eq!(value(read(&mut n, false, None, TxnView::default())), Some(b"old".to_vec()));
    let short =
        Some(Hlc { physical: mark.physical, logical: mark.logical + 1 }).filter(|w| *w < new);
    assert_eq!(value(read(&mut n, false, short, TxnView::default())), Some(b"old".to_vec()));
    // The reader's own intent in the window does not stop it.
    let mine = intents_mark(n.txn(TxnOp::PutIntents {
        txn: T1,
        read_ts: new,
        writes: vec![(C.to_vec(), put(b"mine"))],
    }));
    let own = TxnView { own: Some(T1), known: BTreeMap::new() };
    let spec = ReadSpec {
        g: G,
        key: C.to_vec(),
        scan: None,
        backward: false,
        at: Some(mark),
        concern: ReadConcern::Majority,
        view: own,
        uncertain_to: Some(Hlc { physical: mine.physical + 250, logical: 0 }),
    };
    let (now, wall) = (n.now, n.wall());
    let id = n.host.read_with(&mut n.core, spec, now, wall);
    assert_eq!(value(n.answer(id)), Some(b"mine".to_vec()));
}

/// A read below an intent whose transaction it learned committed above its mark but inside its
/// window (008 FR-008): the commit may have been acknowledged before the read began, so the read
/// stops at the commit's mark like it does for a resolved version there (`txn-append`, seed 489).
#[test]
fn a_known_commit_in_the_uncertainty_window_stops_the_read() {
    let mut n = Node::new();
    let old = n.write(A, b"old");
    let at = intents_mark(n.txn(TxnOp::PutIntents {
        txn: T1,
        read_ts: old,
        writes: vec![(A.to_vec(), put(b"new"))],
    }));
    let read_at = Hlc { physical: at.physical + 1, logical: 0 };
    let commit = Hlc { physical: read_at.physical + 5, logical: 0 };
    let read = |n: &mut Node, scan: bool, window: Option<Hlc>| {
        let spec = ReadSpec {
            g: G,
            key: if scan { b"\x01".to_vec() } else { A.to_vec() },
            scan: scan.then(|| (b"\x02".to_vec(), 10)),
            backward: false,
            at: Some(read_at),
            concern: ReadConcern::Majority,
            view: knowing(T1, Known::Committed(commit)),
            uncertain_to: window,
        };
        let (now, wall) = (n.now, n.wall());
        let id = n.host.read_with(&mut n.core, spec, now, wall);
        n.answer(id)
    };
    let window = Some(Hlc { physical: read_at.physical + 250, logical: u32::MAX });
    for scan in [false, true] {
        assert_eq!(
            read(&mut n, scan, window),
            Err(RangeError::ReadUncertain { ts: commit }),
            "scan {scan}"
        );
    }
    // A window that ends below the commit, or none: the version below the intent.
    let short = Some(Hlc { physical: commit.physical - 1, logical: 0 });
    assert_eq!(value(read(&mut n, false, short)), Some(b"old".to_vec()));
    assert_eq!(rows(read(&mut n, true, None)), vec![(A.to_vec(), b"old".to_vec())]);
}

/// `EndTxn(Commit)` and `PushTxn` on one record at the same time (008 T029, FR-003): the record's
/// latch puts one after the other. A push that goes first lifts the commit above its mark; one
/// that goes second finds the record committed and tells the reader where.
#[test]
fn a_commit_and_a_push_on_one_record_are_ordered() {
    for push_first in [false, true] {
        let mut n = Node::new();
        let old = n.write(A, b"old");
        let at = intents_mark(n.txn(TxnOp::PutIntents {
            txn: T1,
            read_ts: old,
            writes: vec![(A.to_vec(), put(b"new"))],
        }));
        let begin = RecordOp::Begin { lsid: 1, txn_number: 1, read_ts: old };
        assert!(matches!(n.txn(TxnOp::EndTxn { txn: T1, op: begin }), Ok(Reply::Record(_))));
        let reader = Hlc { physical: at.physical + 50, logical: 0 };
        let commit = TxnOp::EndTxn { txn: T1, op: RecordOp::Commit { ts: at, spans: vec![] } };
        let push = TxnOp::PushTxn { txn: T1, push: Push::To(succ(reader)) };
        let ops = if push_first { [push, commit] } else { [commit, push] };
        let (now, wall) = (n.now, n.wall());
        let ids =
            ops.map(|op| n.host.txn(&mut n.core, TxnReq { g: G, op, wc: MAJORITY }, now, wall));
        let answers = ids.map(|id| match n.answer(id) {
            Ok(Reply::Record(rec)) => rec,
            other => panic!("not a record: {other:?}"),
        });
        let (pushed, committed) =
            if push_first { (&answers[0], &answers[1]) } else { (&answers[1], &answers[0]) };
        assert_eq!(committed.status, TxnStatus::Committed, "push first {push_first}");
        if push_first {
            assert_eq!(pushed.status, TxnStatus::Pending);
            assert!(committed.commit_ts >= succ(reader), "the commit went above the push");
        } else {
            assert_eq!(pushed.status, TxnStatus::Committed);
            assert_eq!(
                pushed.commit_ts, committed.commit_ts,
                "the reader learns the commit's mark"
            );
        }
    }
}

/// Writes a document and, next to it, a version that only serves the history (014).
struct Described(Vec<u8>, Vec<u8>);

impl teseladb_range::host::Mutation for Described {
    fn eval(
        &self,
        _: &[Option<(u8, &[u8])>],
        _: Hlc,
    ) -> Result<teseladb_range::mutate::Outcome, teseladb_range::host::MutationError> {
        use teseladb_range::command::Put;
        let history = [&self.0[..], &[0][..]].concat();
        Ok(teseladb_range::mutate::Outcome {
            puts: vec![
                Put { key: self.0.clone(), value_type: 1, value: self.1.clone() },
                Put { key: history, value_type: teseladb_range::HISTORY_VALUE, value: vec![7; 9] },
            ],
            deletes: vec![],
            change: vec![],
            result: vec![],
        })
    }
}

/// The versions that only serve the history of a document (014 FR-009) are not documents: no scan
/// returns them, written by a write or committed by a transaction, and they are not live data.
#[test]
fn history_versions_are_neither_rows_nor_live_data() {
    use teseladb_range::host::WriteReq;
    let mut n = Node::new();
    let live = n.live();
    let (now, wall) = (n.now, n.wall());
    let m = Box::new(Described(A.to_vec(), b"a0".to_vec()));
    let w = WriteReq { g: G, keys: vec![A.to_vec()], m, dedup: None, wc: MAJORITY };
    let id = n.host.mutate(&mut n.core, w, now, wall);
    let at = match n.answer(id) {
        Ok(Reply::Mutated { ts, .. }) => ts,
        other => panic!("not written: {other:?}"),
    };
    assert_eq!(n.live(), live + (A.len() as u64 + 1 + 2));
    let read_at = Hlc { physical: at.physical + 5, logical: 0 };
    let got = rows(n.scan_view(read_at, 10, TxnView::default()));
    assert_eq!(got, vec![(A.to_vec(), b"a0".to_vec())]);
    // A transaction's history intent, read by the transaction and once committed.
    let history = [B, &[0][..]].concat();
    let op = IntentOp::Put { value_type: teseladb_range::HISTORY_VALUE, value: vec![1; 4] };
    let mark = intents_mark(n.txn(intents(T1, at, &[(B, put(b"bb")), (&history, op)])));
    let own = TxnView { own: Some(T1), known: BTreeMap::new() };
    let got = rows(n.scan_view(at, 10, own));
    assert_eq!(got, vec![(A.to_vec(), b"a0".to_vec()), (B.to_vec(), b"bb".to_vec())]);
    // The reaper finds the history intent too, so that it resolves it with the document's.
    let find = TxnOp::FindIntents { txn: T1, start: b"\x01".to_vec(), end: b"\x02".to_vec() };
    let found: Vec<Vec<u8>> = match n.txn(find) {
        Ok(Reply::Batch(rows)) => rows.into_iter().map(|(k, _)| k).collect(),
        other => panic!("not rows: {other:?}"),
    };
    assert_eq!(found, vec![B.to_vec(), history.clone()]);
    let live = n.live();
    let resolve = TxnOp::ResolveIntents {
        txn: T1,
        status: TxnStatus::Committed,
        commit_ts: mark,
        keys: vec![B.to_vec(), history.clone()],
    };
    intents_mark(n.txn(resolve));
    assert_eq!(n.live(), live + (B.len() as u64 + 1 + 2));
    let read_at = Hlc { physical: mark.physical + 5, logical: 0 };
    let got = rows(n.scan_view(read_at, 10, TxnView::default()));
    assert_eq!(got, vec![(A.to_vec(), b"a0".to_vec()), (B.to_vec(), b"bb".to_vec())]);
}

#[test]
/// What a change stream reads of a range (014 FR-003, FR-004): every version of `(from, to]` with
/// its value type, the deletions, and the mark of the oldest intent still pending there, which
/// holds the range's resolved mark below it.
fn a_range_gives_its_versions_and_its_oldest_pending_intent() {
    use teseladb_range::changes::{ChangeRows, Versioned, change_rows};
    let mut n = Node::new();
    let zero = Hlc { physical: 0, logical: 0 };
    let a = n.write(A, b"a0");
    let b = n.delete(A);
    let rows = |n: &Node, from: Hlc, to: Hlc| loop {
        if let Some(r) = change_rows(n.host.lsm(), (b"\x01", b"\x02"), (from, to)) {
            break r.unwrap();
        }
    };
    let got = rows(&n, zero, far());
    let want = ChangeRows {
        rows: vec![
            Versioned { key: A.to_vec(), ts: b, value: None },
            Versioned { key: A.to_vec(), ts: a, value: Some((1, b"a0".to_vec())) },
        ],
        oldest_intent: None,
    };
    assert_eq!(got, want);
    // Only `(from, to]`.
    assert_eq!(rows(&n, a, b).rows.len(), 1);
    assert!(rows(&n, b, far()).rows.is_empty());
    // A pending intent shows as its mark, not as a row.
    let at = intents_mark(n.txn(intents(T1, b, &[(C, put(b"c"))])));
    let got = rows(&n, b, far());
    assert_eq!((got.rows.len(), got.oldest_intent), (0, Some(at)));
    // A stored version without its value type is damage: the read fails instead of guessing.
    let late = Hlc { physical: at.physical + 1_000, logical: 0 };
    n.host.lsm().borrow_mut().put(G, u64::MAX / 2, b"\x01d", late, &[]).expect("written");
    let read = loop {
        if let Some(r) = change_rows(n.host.lsm(), (b"\x01", b"\x02"), (at, far())) {
            break r;
        }
    };
    assert!(read.is_err(), "{read:?}");
}

/// What the leader gives a change stream (014 FR-003, FR-004): the versions of the range up to
/// its closed mark, which it closes 200 ms behind its clock while streams read it, and the oldest
/// intent pending there, which holds the stream's resolved mark below it.
#[test]
fn the_leader_gives_a_stream_its_versions_up_to_the_closed_mark() {
    use teseladb_range::host::ChangesReq;
    use teseladb_types::Nanos;
    let mut n = Node::new();
    let a = n.write(A, b"a0");
    let from = Hlc { physical: a.physical - 1, logical: 0 };
    let ask = |n: &mut Node, from: Hlc| {
        let req =
            ChangesReq { g: G, start: b"\x01".to_vec(), end: b"\x02".to_vec(), from, at: None };
        let now = n.now;
        let id = n.host.changes(&mut n.core, req, now);
        n.answer(id)
    };
    // Nothing closed above the write yet: no rows, and nothing resolved past the start.
    match ask(&mut n, from) {
        Ok(Reply::Changes { rows, to, .. }) => assert!(rows.is_empty() && to == from),
        other => panic!("not changes: {other:?}"),
    }
    // With a stream open the leader closes 200 ms behind its clock.
    for _ in 0..60 {
        n.now = n.now + Nanos::from_millis(10);
        n.settle();
    }
    match ask(&mut n, from) {
        Ok(Reply::Changes { rows, to, oldest_intent }) => {
            assert!(to >= a, "closed at {to:?}, the write at {a:?}");
            assert_eq!(rows.len(), 1);
            assert_eq!((rows[0].key.as_slice(), rows[0].ts), (A, a));
            assert_eq!(oldest_intent, None);
        }
        other => panic!("not changes: {other:?}"),
    }
    // With a stream open the leader closes marks every 20 ms, not every 200 ms (014 SC-002).
    let first = match ask(&mut n, from) {
        Ok(Reply::Changes { to, .. }) => to,
        other => panic!("not changes: {other:?}"),
    };
    for _ in 0..4 {
        n.now = n.now + Nanos::from_millis(10);
        n.settle();
    }
    match ask(&mut n, first) {
        Ok(Reply::Changes { to, .. }) => {
            assert!(to.physical >= first.physical + 20, "{first:?} then {to:?}");
        }
        other => panic!("not changes: {other:?}"),
    }
    // A read from the start of time (the first build of a search index, 011 FR-014) has no
    // window: it reaches the closed mark at once instead of one second after the epoch.
    let zero = Hlc { physical: 0, logical: 0 };
    match ask(&mut n, zero) {
        Ok(Reply::Changes { rows, to, .. }) => {
            assert!(to >= a, "closed at {to:?}, the write at {a:?}");
            assert_eq!(rows.len(), 1);
        }
        other => panic!("not changes: {other:?}"),
    }
    // A pending intent shows its mark.
    let at = intents_mark(n.txn(intents(T1, a, &[(b"\x01c", put(b"c"))])));
    for _ in 0..60 {
        n.now = n.now + Nanos::from_millis(10);
        n.settle();
    }
    match ask(&mut n, a) {
        Ok(Reply::Changes { oldest_intent, .. }) => assert_eq!(oldest_intent, Some(at)),
        other => panic!("not changes: {other:?}"),
    }
    // 014 FR-010: an interval the range no longer holds whole (a split shrank it under a stale
    // table) is refused, never answered for the part it holds, so the cursor reads it again.
    for (start, end) in [(&b"\x01"[..], &b"\x04"[..]), (b"\x00", b"\x02"), (b"\x01b", b"\x01b")] {
        let req = ChangesReq { g: G, start: start.to_vec(), end: end.to_vec(), from: a, at: None };
        let now = n.now;
        let id = n.host.changes(&mut n.core, req, now);
        assert_eq!(n.answer(id), Err(RangeError::OutOfRange), "{start:?}..{end:?}");
    }
}

/// The record a push by read answers with.
fn pushed_record(r: Result<Reply, RangeError>) -> teseladb_range::intents::TxnRecord {
    match r {
        Ok(Reply::Record(rec)) => rec,
        other => panic!("not a record: {other:?}"),
    }
}

#[test]
fn a_reader_pushes_a_pending_transaction_by_reading_its_record() {
    let mut n = Node::new();
    let old = n.write(A, b"old");
    let at = intents_mark(n.txn(TxnOp::PutIntents {
        txn: T1,
        read_ts: old,
        writes: vec![(A.to_vec(), put(b"new"))],
    }));
    let begin = RecordOp::Begin { lsid: 1, txn_number: 1, read_ts: old };
    assert!(matches!(n.txn(TxnOp::EndTxn { txn: T1, op: begin }), Ok(Reply::Record(_))));
    // A reader at a mark the leader's clock passed reads the record there (008 T043): it learns
    // the transaction pending above its mark, and the record itself stays as it was.
    let read_at = Hlc { physical: at.physical + 1, logical: 0 };
    let rec = pushed_record(n.txn(TxnOp::PushByRead { txn: T1, at: read_at }));
    assert_eq!(rec.status, TxnStatus::Pending);
    assert!(rec.min_commit_ts > read_at, "{rec:?}");
    let again = RecordOp::Begin { lsid: 1, txn_number: 1, read_ts: old };
    let Ok(Reply::Record(stored)) = n.txn(TxnOp::EndTxn { txn: T1, op: again }) else {
        panic!("no record")
    };
    assert!(stored.min_commit_ts < read_at, "nothing was written: {stored:?}");
    // The commit, with the coordinator's mark below the read, goes above it.
    let commit = RecordOp::Commit { ts: at, spans: vec![] };
    let done = pushed_record(n.txn(TxnOp::EndTxn { txn: T1, op: commit }));
    assert_eq!(done.status, TxnStatus::Committed);
    assert!(done.commit_ts >= rec.min_commit_ts, "{done:?} below {rec:?}");
    // A push by read of a finished transaction answers its record as it is.
    assert_eq!(pushed_record(n.txn(TxnOp::PushByRead { txn: T1, at: read_at })), done);
}

#[test]
fn a_push_by_read_before_the_record_exists_holds_the_coordinator_above_it() {
    let mut n = Node::new();
    let old = n.write(A, b"old");
    let at = intents_mark(n.txn(TxnOp::PutIntents {
        txn: T1,
        read_ts: old,
        writes: vec![(A.to_vec(), put(b"new"))],
    }));
    let read_at = Hlc { physical: at.physical + 1, logical: 0 };
    let rec = pushed_record(n.txn(TxnOp::PushByRead { txn: T1, at: read_at }));
    assert_eq!((rec.txn, rec.status), (T1, TxnStatus::Pending));
    assert!(rec.min_commit_ts > read_at);
    // The coordinator creates its record afterwards: its commit still goes above the read.
    let begin = RecordOp::Begin { lsid: 1, txn_number: 1, read_ts: old };
    assert!(matches!(n.txn(TxnOp::EndTxn { txn: T1, op: begin }), Ok(Reply::Record(_))));
    let commit = RecordOp::Commit { ts: at, spans: vec![] };
    let done = pushed_record(n.txn(TxnOp::EndTxn { txn: T1, op: commit }));
    assert!(done.commit_ts >= rec.min_commit_ts, "{done:?} below {rec:?}");
}

#[test]
fn a_push_by_read_and_a_commit_on_one_record_are_ordered() {
    for push_first in [false, true] {
        let mut n = Node::new();
        let old = n.write(A, b"old");
        let at = intents_mark(n.txn(TxnOp::PutIntents {
            txn: T1,
            read_ts: old,
            writes: vec![(A.to_vec(), put(b"new"))],
        }));
        let begin = RecordOp::Begin { lsid: 1, txn_number: 1, read_ts: old };
        assert!(matches!(n.txn(TxnOp::EndTxn { txn: T1, op: begin }), Ok(Reply::Record(_))));
        let reader = Hlc { physical: at.physical + 1, logical: 0 };
        let commit = TxnOp::EndTxn { txn: T1, op: RecordOp::Commit { ts: at, spans: vec![] } };
        let push = TxnOp::PushByRead { txn: T1, at: reader };
        let ops = if push_first { [push, commit] } else { [commit, push] };
        let (now, wall) = (n.now, n.wall());
        let ids =
            ops.map(|op| n.host.txn(&mut n.core, TxnReq { g: G, op, wc: MAJORITY }, now, wall));
        let answers = ids.map(|id| pushed_record(n.answer(id)));
        let (pushed, committed) =
            if push_first { (&answers[0], &answers[1]) } else { (&answers[1], &answers[0]) };
        assert_eq!(committed.status, TxnStatus::Committed, "push first {push_first}");
        if push_first {
            assert_eq!(pushed.status, TxnStatus::Pending);
            assert!(committed.commit_ts >= pushed.min_commit_ts, "the commit went above the read");
        } else {
            assert_eq!(pushed.status, TxnStatus::Committed);
            assert_eq!(pushed.commit_ts, committed.commit_ts, "the reader learns the commit");
        }
    }
}

#[test]
fn a_push_by_read_at_a_mark_ahead_of_the_leaders_wall_clock_is_refused_for_now() {
    let mut n = Node::new();
    let ahead = Hlc { physical: n.wall() + 60_000, logical: 0 };
    // The reader goes again after a pause (`push_for_read`): the leader never reads its record
    // at a mark its clock has not reached.
    assert_eq!(n.txn(TxnOp::PushByRead { txn: T1, at: ahead }), Err(RangeError::Overloaded));
}

/// A read outside the core (005 FR-028) that meets an intent leaves it to the core, which
/// decides it: the scan of a span with one, and the read of its key.
#[test]
fn a_read_outside_the_core_that_meets_an_intent_goes_to_the_core() {
    let mut n = Node::new();
    let old = n.write(A, b"old");
    n.write(C, b"c");
    intents_mark(n.txn(TxnOp::PutIntents {
        txn: T1,
        read_ts: old,
        writes: vec![(A.to_vec(), put(b"new"))],
    }));
    let view = n.host.views().get(G).expect("the view of a range this node leads");
    let mut cache = teseladb_lsm::ViewCache::default();
    let clocks = (n.wall(), || n.now);
    let span: (&[u8], &[u8]) = (A, D);
    assert!(
        view.scan(&mut cache, span, (10, false), clocks, ReadConcern::Local, |_| false).is_none()
    );
    let clocks = (n.wall(), || n.now);
    assert!(
        view.scan(&mut cache, span, (10, true), clocks, ReadConcern::Local, |_| false).is_none()
    );
    let read = view.read(&mut cache, A, n.wall(), || n.now, ReadConcern::Local);
    assert!(matches!(read, teseladb_range::view::FastRead::Core));
    // A span past the intent is served.
    let (_, rows) = view
        .scan(&mut cache, (C, D), (10, false), (n.wall(), || n.now), ReadConcern::Local, |_| false)
        .expect("served outside");
    assert_eq!(rows.len(), 1);
}
