//! The removal of transaction records no one needs any more (008, record GC): a record goes only
//! once it is final, its intervals are forgotten because every intent is resolved, and its last
//! activity is old enough; the reaper finds the intents of an interval it cannot list key by key
//! (one of a key longer than the record keeps) with `FindIntents`, range by range.

mod txn_node;

use teseladb_hlc::Hlc;
use teseladb_range::host::{CONTINUES, RangeError, Reply, TxnOp};
use teseladb_range::intents::{RecordOp, TxnRecord, TxnStatus, record_key};
use txn_node::*;

fn record(r: Result<Reply, RangeError>) -> TxnRecord {
    match r {
        Ok(Reply::Record(rec)) => rec,
        other => panic!("no record: {other:?}"),
    }
}

fn begin(n: &mut Node, read_ts: Hlc) {
    let op = RecordOp::Begin { lsid: 5, txn_number: 1, read_ts };
    record(n.txn(TxnOp::EndTxn { txn: T1, op }));
}

#[test]
fn a_record_goes_once_final_resolved_and_old_enough() {
    let mut n = Node::new();
    let read_ts = n.write(A, b"v");
    begin(&mut n, read_ts);
    let forget = |before| TxnOp::ForgetRecord { txn: T1, before };
    assert_eq!(n.txn(forget(far())), Err(RangeError::WriteConflict), "running");
    let spans = vec![(A.to_vec(), B.to_vec())];
    let commit = RecordOp::Commit { ts: far(), spans };
    let done = record(n.txn(TxnOp::EndTxn { txn: T1, op: commit }));
    assert_eq!(done.status, TxnStatus::Committed);
    let after = Hlc { physical: done.commit_ts.physical + 1, logical: 0 };
    assert_eq!(n.txn(forget(after)), Err(RangeError::WriteConflict), "intervals left");
    let resolved = record(n.txn(TxnOp::EndTxn { txn: T1, op: RecordOp::Resolved }));
    assert!(resolved.spans.is_empty());
    assert_eq!(n.txn(forget(done.commit_ts)), Err(RangeError::WriteConflict), "too recent");
    let live = n.live();
    assert!(matches!(n.txn(forget(after)), Ok(Reply::Intents { .. })));
    assert_eq!(n.get(&record_key(T1)), None);
    assert_eq!(n.live(), live, "a record counts in no document's bytes");
    assert_eq!(n.txn(forget(after)), Err(RangeError::NoSuchTransaction), "gone");
}

/// A record is read without writing anything (the reaper looks for a session's next
/// transaction, 008 record GC).
#[test]
fn a_record_is_read_without_writing() {
    let mut n = Node::new();
    let read = TxnOp::ReadRecord { txn: T1 };
    assert_eq!(n.txn(read.clone()), Err(RangeError::NoSuchTransaction));
    assert_eq!(n.get(&record_key(T1)), None, "nothing written");
    let read_ts = n.write(A, b"v");
    begin(&mut n, read_ts);
    let rec = record(n.txn(read));
    assert_eq!((rec.txn, rec.lsid, rec.status), (T1, 5, TxnStatus::Pending));
}

#[test]
fn the_intents_of_an_interval_are_found_range_by_range() {
    let mut n = Node::new();
    let long: Vec<u8> = [b"\x01l".as_slice(), &[b'x'; 2_000]].concat();
    let longer = [long.as_slice(), b"y"].concat();
    let read_ts = n.write(A, b"v");
    let writes = [(long.as_slice(), put(b"1")), (longer.as_slice(), put(b"2")), (A, put(b"3"))];
    intents_mark(n.txn(intents(T1, read_ts, &writes)));
    intents_mark(n.txn(intents(T2, read_ts, &[(B, put(b"4"))])));
    let find = |start: &[u8], end: &[u8]| TxnOp::FindIntents {
        txn: T1,
        start: start.to_vec(),
        end: end.to_vec(),
    };
    let rows = |r: Result<Reply, RangeError>| match r {
        Ok(Reply::Batch(rows)) => rows,
        other => panic!("{other:?}"),
    };
    let found = rows(n.txn(find(b"\x01l", b"\x01m")));
    let keys: Vec<Vec<u8>> = found.iter().map(|(k, _)| k.clone()).collect();
    assert_eq!(keys, vec![long.clone(), longer.clone()]);
    assert!(rows(n.txn(find(b"\x01b", b"\x01c"))).is_empty(), "another transaction's");
    n.delete(b"\x01c");
    assert!(rows(n.txn(find(b"\x01c", b"\x01d"))).is_empty(), "a deletion is no intent");
    // Past the range's end: where to go on, once nothing is left here.
    let past = rows(n.txn(find(b"\x01m", b"\x05")));
    assert_eq!(past, vec![(END.to_vec(), (0, CONTINUES.to_vec()))]);
    assert_eq!(rows(n.txn(find(b"\x01", b"\x05"))).len(), 3, "the keys come first");
}
