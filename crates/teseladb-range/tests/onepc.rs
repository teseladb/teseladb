//! The one-phase commit (008 T007, FR-006): a transaction whose keys are all in one range commits
//! with one `Write`, after the same conflict checks as `PutIntents`, with no record and no intents.

mod txn_node;

use teseladb_hlc::Hlc;
use teseladb_range::host::{RangeError, Reply, TxnOp};
use teseladb_range::intents::{IntentOp, IntentOwner, TxnId, record_key};
use txn_node::*;

fn one_phase(txn: TxnId, read_ts: Hlc, writes: &[(&[u8], IntentOp)]) -> TxnOp {
    let writes = writes.iter().map(|(k, op)| (k.to_vec(), op.clone())).collect();
    TxnOp::OnePhase { txn, read_ts, writes, session: None }
}

fn committed(r: Result<Reply, RangeError>) -> Hlc {
    match r {
        Ok(Reply::Intents { ts }) => ts,
        other => panic!("not committed: {other:?}"),
    }
}

#[test]
fn a_one_range_transaction_commits_with_one_write() {
    let mut n = Node::new();
    let first = n.write(A, b"old");
    let live = n.live();
    let ts = committed(n.txn(one_phase(T1, first, &[(A, put(b"new")), (B, put(b"b"))])));
    assert!(ts > first);
    assert_eq!(n.get(A), Some(b"new".to_vec()));
    assert_eq!(n.get(B), Some(b"b".to_vec()));
    // `new` is as long as `old`; `b` adds its key, type and value.
    assert_eq!(n.live(), live + B.len() as u64 + 1 + 1);
    // No record: nothing else knows the transaction.
    assert_eq!(n.get(&record_key(T1)), None);
    // A deletion.
    let at = committed(n.txn(one_phase(T2, ts, &[(B, IntentOp::Delete)])));
    assert!(at > ts);
    assert_eq!(n.get(B), None);
    assert_eq!(n.live(), live);
}

#[test]
fn a_one_phase_commit_checks_conflicts_like_put_intents() {
    let mut n = Node::new();
    let first = n.write(A, b"v1");
    let before = Hlc { physical: first.physical - 1, logical: 0 };
    let r = n.txn(one_phase(T1, before, &[(A, put(b"x"))]));
    assert_eq!(r, Err(RangeError::WriteConflict));
    // A deletion newer than the read too.
    let deleted = n.delete(A);
    assert_eq!(n.txn(one_phase(T1, first, &[(A, put(b"x"))])), Err(RangeError::WriteConflict));
    // Another transaction's intent: its owner, to push.
    let Ok(Reply::Intents { ts }) = n.txn(intents(T2, deleted, &[(B, put(b"y"))])) else {
        panic!("intents not written")
    };
    let owner = IntentOwner { txn: T2, record_key: record_key(T2), key: B.to_vec(), ts };
    assert_eq!(
        n.txn(one_phase(T1, far(), &[(B, put(b"z"))])),
        Err(RangeError::IntentConflict(owner))
    );
    // Nothing of the refused ones was written.
    assert_eq!(n.get(A), None);
    let mark = intents_mark(n.txn(intents(T2, deleted, &[(A, put(b"a"))])));
    assert!(mark > ts);
}
