//! Transactions on the host of a range (008 T005, FR-002 to FR-005): `PutIntents` with its
//! conflicts, `EndTxn` and `PushTxn` over the record, and `ResolveIntents`, each taking its
//! latches, reading the newest versions and answering once applied. One replica that leads on
//! its own, driven as an embedder would.

mod txn_node;

use teseladb_hlc::Hlc;
use teseladb_range::host::Reply;
use teseladb_range::host::{RangeError, TxnOp, WriteReq};
use teseladb_range::intents::{
    IntentOp, IntentOwner, Push, RecordOp, TxnId, TxnRecord, TxnStatus, record_key,
};
use txn_node::*;
use teseladb_types::GroupId;

#[test]
fn put_intents_conflict_with_newer_versions_and_other_transactions() {
    let mut n = Node::new();
    let written = n.write(A, b"v1");
    // A version newer than the read mark: WriteConflict.
    let before = Hlc { physical: written.physical - 1, logical: 0 };
    assert_eq!(n.txn(intents(T1, before, &[(A, put(b"x"))])), Err(RangeError::WriteConflict));
    // A deletion newer than the read mark too.
    let deleted = n.delete(A);
    assert_eq!(n.txn(intents(T1, written, &[(A, put(b"x"))])), Err(RangeError::WriteConflict));
    // At or above the newest version: the intents go in, above the read cache.
    let at = intents_mark(n.txn(intents(T1, deleted, &[(A, put(b"x")), (B, IntentOp::Delete)])));
    assert!(at > deleted);
    // Another transaction gets the owner of the intent, whatever its read mark.
    let owner = IntentOwner { txn: T1, record_key: record_key(T1), key: B.to_vec(), ts: at };
    let r = n.txn(intents(T2, far(), &[(B, put(b"y"))]));
    assert_eq!(r, Err(RangeError::IntentConflict(owner)));
    // The transaction writes its own key again.
    let again = intents_mark(n.txn(intents(T1, deleted, &[(A, put(b"x2"))])));
    assert!(again > at);
}

#[test]
fn the_record_moves_through_begin_push_and_commit() {
    let mut n = Node::new();
    let rec = record(n.txn(TxnOp::EndTxn { txn: T1, op: begin(Hlc { physical: 5, logical: 0 }) }));
    assert_eq!(rec.status, TxnStatus::Pending);
    let push_to = Hlc { physical: far().physical, logical: 3 };
    let pushed = record(n.txn(TxnOp::PushTxn { txn: T1, push: Push::To(push_to) }));
    assert_eq!((pushed.status, pushed.min_commit_ts), (TxnStatus::Pending, push_to));
    // Alive: a writer's push does not abort it.
    let alive = record(n.txn(TxnOp::PushTxn {
        txn: T1,
        push: Push::AbortIfAbandoned { intent_ts: Hlc { physical: 0, logical: 0 } },
    }));
    assert_eq!(alive.status, TxnStatus::Pending);
    let commit = RecordOp::Commit { ts: Hlc { physical: 9, logical: 0 }, spans: vec![] };
    let done = record(n.txn(TxnOp::EndTxn { txn: T1, op: commit.clone() }));
    assert_eq!((done.status, done.commit_ts), (TxnStatus::Committed, push_to));
    // A retried commit sees the same record.
    assert_eq!(record(n.txn(TxnOp::EndTxn { txn: T1, op: commit })), done);
    // A record pushed before it exists is aborted, and its coordinator then fails.
    let gone = record(n.txn(TxnOp::PushTxn {
        txn: T2,
        push: Push::AbortIfAbandoned { intent_ts: Hlc { physical: 0, logical: 0 } },
    }));
    assert_eq!(gone.status, TxnStatus::Aborted);
    let late = n.txn(TxnOp::EndTxn { txn: T2, op: begin(Hlc { physical: 5, logical: 0 }) });
    assert_eq!(late, Err(RangeError::NoSuchTransaction));
}

#[test]
fn resolving_turns_intents_into_versions_or_drops_them() {
    let mut n = Node::new();
    let first = n.write(A, b"old");
    let live = n.live();
    intents_mark(n.txn(intents(T1, first, &[(A, put(b"newer")), (B, put(b"bb"))])));
    let rec = record(n.txn(TxnOp::EndTxn { txn: T1, op: begin(first) }));
    let commit = RecordOp::Commit { ts: n.host_mark(), spans: vec![] };
    let done = record(n.txn(TxnOp::EndTxn { txn: T1, op: commit }));
    assert_eq!(rec.txn, done.txn);
    let resolve = |status, keys: &[&[u8]]| TxnOp::ResolveIntents {
        txn: T1,
        status,
        commit_ts: done.commit_ts,
        keys: keys.iter().map(|k| k.to_vec()).collect(),
    };
    // A pending transaction is not resolved.
    let r = n.txn(resolve(TxnStatus::Pending, &[A]));
    assert!(matches!(r, Err(RangeError::Mutation(_))), "{r:?}");
    // A key without an intent of the transaction, or with another one's, is left as it is.
    let other = TxnId(44);
    intents_mark(n.txn(intents(other, far(), &[(b"\x01c", put(b"c"))])));
    intents_mark(n.txn(resolve(TxnStatus::Committed, &[A, B, b"\x01c", b"\x01none"])));
    let still = n.txn(intents(T2, far(), &[(b"\x01c", put(b"x"))]));
    assert!(matches!(still, Err(RangeError::IntentConflict(IntentOwner { txn: TxnId(44), .. }))));
    assert_eq!(n.get(A), Some(b"newer".to_vec()));
    assert_eq!(n.get(B), Some(b"bb".to_vec()));
    // `newer` replaced `old` (two bytes more) and `bb` is new: key, type and value.
    assert_eq!(n.live(), live + 2 + (B.len() as u64 + 1 + 2));
    // An aborted transaction's intent goes and the version below shows again.
    intents_mark(n.txn(intents(T2, far(), &[(A, IntentOp::Delete)])));
    let abort = TxnOp::ResolveIntents {
        txn: T2,
        status: TxnStatus::Aborted,
        commit_ts: Hlc { physical: 0, logical: 0 },
        keys: vec![A.to_vec()],
    };
    intents_mark(n.txn(abort));
    assert_eq!(n.get(A), Some(b"newer".to_vec()));
    // A committed deletion takes the version's bytes away.
    let before = n.live();
    let t3 = TxnId(33);
    intents_mark(n.txn(intents(t3, far(), &[(A, IntentOp::Delete)])));
    let del = TxnOp::ResolveIntents {
        txn: t3,
        status: TxnStatus::Committed,
        commit_ts: n.host_mark(),
        keys: vec![A.to_vec()],
    };
    intents_mark(n.txn(del));
    assert_eq!(n.get(A), None);
    assert_eq!(n.live(), before - (A.len() as u64 + 1 + 5));
}

#[test]
fn a_request_outside_the_range_or_with_a_corrupt_record_fails() {
    let mut n = Node::new();
    let out = n.txn(intents(T1, far(), &[(b"\x05x", put(b"v"))]));
    assert_eq!(out, Err(RangeError::OutOfRange));
    // A value that is no record at a record's key.
    n.write(&record_key(T1), b"garbage");
    let r = n.txn(TxnOp::PushTxn { txn: T1, push: Push::To(far()) });
    assert_eq!(r, Err(RangeError::Unavailable));
}

fn record(r: Result<Reply, RangeError>) -> TxnRecord {
    match r {
        Ok(Reply::Record(rec)) => rec,
        other => panic!("not a record: {other:?}"),
    }
}

fn begin(read_ts: Hlc) -> RecordOp {
    RecordOp::Begin { lsid: 1, txn_number: 1, read_ts }
}

impl Node {
    /// A mark above everything the node wrote so far.
    fn host_mark(&self) -> Hlc {
        Hlc { physical: self.wall() + 1_000, logical: 0 }
    }
}

#[test]
fn a_plain_write_that_finds_an_intent_stops_with_its_owner() {
    // 008 FR-002 (T032): the layer above waits for the owner (or pushes it if abandoned) and
    // writes again; the range never takes the intent's payload for the document.
    let mut n = Node::new();
    let first = n.write(A, b"old");
    let at = intents_mark(n.txn(intents(T1, first, &[(A, put(b"held"))])));
    let (now, wall) = (n.now, n.wall());
    let m = Box::new(Set(A.to_vec(), b"plain".to_vec()));
    let w = WriteReq { g: G, keys: vec![A.to_vec()], m, dedup: None, wc: MAJORITY };
    let id = n.host.mutate(&mut n.core, w, now, wall);
    let owner = IntentOwner { txn: T1, record_key: record_key(T1), key: A.to_vec(), ts: at };
    assert_eq!(n.answer(id), Err(RangeError::IntentConflict(owner)));
    // Once the owner aborts and its intent is resolved, the write goes through.
    let abort = TxnOp::ResolveIntents {
        txn: T1,
        status: TxnStatus::Aborted,
        commit_ts: Hlc { physical: 0, logical: 0 },
        keys: vec![A.to_vec()],
    };
    intents_mark(n.txn(abort));
    n.write(A, b"plain");
    assert_eq!(n.get(A), Some(b"plain".to_vec()));
}

/// A transaction's request the leader reaches only after its deadline writes nothing (009
/// FR-006): a late commit would make visible entries written with an old list of indexes.
#[test]
fn a_transaction_request_past_its_deadline_is_not_evaluated() {
    let mut n = Node::new();
    let req = |op| teseladb_range::host::TxnReq { g: G, op, wc: MAJORITY };
    let one_phase =
        TxnOp::OnePhase { txn: T1, read_ts: far(), writes: vec![(A.to_vec(), put(b"v"))] };
    let (now, wall) = (n.now, n.wall());
    let late = n.host.txn_until(&mut n.core, req(one_phase.clone()), wall - 1, (now, wall));
    assert_eq!(n.answer(late), Err(RangeError::Timeout));
    assert_eq!(n.get(A), None, "the late commit wrote nothing");
    let begin_late = TxnOp::EndTxn { txn: T2, op: begin(far()) };
    let (now, wall) = (n.now, n.wall());
    let late = n.host.txn_until(&mut n.core, req(begin_late), wall - 1, (now, wall));
    assert_eq!(n.answer(late), Err(RangeError::Timeout));
    let (now, wall) = (n.now, n.wall());
    let on_time = n.host.txn_until(&mut n.core, req(one_phase), wall + 60_000, (now, wall));
    assert!(n.answer(on_time).is_ok());
    assert_eq!(n.get(A), Some(b"v".to_vec()));
}

#[test]
fn the_record_mark_moves_only_with_entries_that_touch_the_records() {
    // 008 FR-007, H-040: the reaper reads no range whose mark did not move since it found it empty.
    let mut n = Node::new();
    let views = n.host.views();
    let start = views.txn_mark(G).expect("published as the range opened");
    n.write(A, b"x");
    assert_eq!(views.txn_mark(G), Some(start), "a document write leaves it");
    record(n.txn(TxnOp::EndTxn { txn: T1, op: begin(Hlc { physical: 5, logical: 0 }) }));
    let moved = views.txn_mark(G).expect("published");
    assert!(moved > start, "a record moves it: {start} {moved}");
    n.write(B, b"y");
    assert_eq!(views.txn_mark(G), Some(moved));
    assert_eq!(views.txn_mark(GroupId(999)), None, "not a range of the core");
}
