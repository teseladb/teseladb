//! Transactions on the host of a range (008 T005, FR-002 to FR-005): `PutIntents` with its
//! conflicts, `EndTxn` and `PushTxn` over the record, and `ResolveIntents`, each taking its
//! latches, reading the newest versions and answering once applied. One replica that leads on
//! its own, driven as an embedder would.

mod txn_node;

use teseladb_hlc::Hlc;
use teseladb_lsm::ViewCache;
use teseladb_range::ack::{W, WriteConcern};
use teseladb_range::host::Reply;
use teseladb_range::host::{RangeError, TxnOp, TxnReq, WriteReq};
use teseladb_range::intents::{
    IntentOp, IntentOwner, Push, RecordOp, TxnId, TxnRecord, TxnStatus, record_key,
};
use teseladb_range::read::ReadConcern;
use teseladb_range::view::Part;
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

/// Intents answered once they are in the leader's log (FR-021, D-160) are no versions a read
/// sees, and their transaction commits after it checked that they are applied: a scan outside
/// the core does not wait for them (005 FR-031). Once applied, the intent itself sends the scan
/// of its key to the core.
#[test]
fn intents_answered_in_the_log_do_not_hold_back_a_scan_outside_the_core() {
    let mut n = Node::new();
    let base = n.write(A, b"v1");
    let w1 = WriteConcern { w: W::Nodes(1), j: false };
    let (now, wall) = (n.now, n.wall());
    let op = intents(T1, base, &[(B, put(b"x"))]);
    let id = n.host.txn(&mut n.core, TxnReq { g: G, op, wc: w1 }, now, wall);
    n.host.poll(&mut n.core, now, wall);
    let answered = n.host.take_replies();
    assert!(answered.iter().any(|(r, a)| *r == id && a.is_ok()), "in the log: {answered:?}");
    let view = n.host.views().get(G).expect("the view of a range this node leads");
    assert!(view.holds(&view.begin()), "no scan has to see an intent");
    let part = Part { docs: 10, backward: false, blocks: 64 };
    let mut cache = ViewCache::default();
    let read = view
        .scan(&mut cache, (A, END), part, || n.now, ReadConcern::Local, |_| false)
        .expect("served outside");
    assert_eq!(read.rows, vec![(A.to_vec(), (DOC, b"v1".to_vec()))]);
    n.settle();
    let view = n.host.views().get(G).expect("the view of a range this node leads");
    let read = view.scan(&mut cache, (A, END), part, || n.now, ReadConcern::Local, |_| false);
    assert!(read.is_none(), "{read:?}");
    // No evaluation is prepared on a key with an intent either (FR-035): its core decides it.
    let newest = view.newest(&mut cache, &[B.to_vec()]);
    assert!(matches!(newest, teseladb_range::view::Newest::No), "{newest:?}");
}

/// Before its commit a coordinator asks each range whether its intents were applied (FR-021,
/// D-160): the keys holding them answer with their highest mark; a key without one of the
/// transaction's intents (a change of leader lost it, or another transaction's is there) answers
/// `NoSuchTransaction`, and the commit fails. It writes nothing.
#[test]
fn check_intents_says_whether_every_intent_of_the_transaction_is_applied() {
    let mut n = Node::new();
    let base = n.write(A, b"v1");
    let at = intents_mark(n.txn(intents(T1, base, &[(A, put(b"x")), (B, IntentOp::Delete)])));
    let check = |txn, keys: &[&[u8]]| TxnOp::CheckIntents {
        txn,
        keys: keys.iter().map(|k| k.to_vec()).collect(),
    };
    assert_eq!(n.txn(check(T1, &[A, B])), Ok(Reply::Intents { ts: at }));
    // A key it did not write, or another transaction asking: something is missing.
    assert_eq!(n.txn(check(T1, &[A, b"\x01zz"])), Err(RangeError::NoSuchTransaction));
    assert_eq!(n.txn(check(T2, &[A])), Err(RangeError::NoSuchTransaction));
    // Nothing was written: the intents are still there.
    assert_eq!(n.txn(check(T1, &[A, B])), Ok(Reply::Intents { ts: at }));
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
    let one_phase = TxnOp::OnePhase {
        txn: T1,
        read_ts: far(),
        writes: vec![(A.to_vec(), put(b"v"))],
        session: None,
    };
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

/// The leader's commit of a transaction says how it changes each collection's documents, from
/// the version below each intent (007 FR-039): a replaced document adds none, a new one adds one
/// and a deleted one takes one away, and the counts stay exact with no recount (H-051 of the
/// native tests).
#[test]
fn a_commit_counts_the_documents_it_replaces_adds_and_deletes() {
    const COLL: (u32, u32) = (0x6100_0000, 4);
    let doc = |pk: &[u8]| {
        teseladb_keys::space::KeyBuilder::document_of_pk(COLL.0, COLL.1, pk).as_bytes().to_vec()
    };
    let (a, b, c) = (doc(b"a"), doc(b"b"), doc(b"c"));
    let mut n = Node::new();
    n.host.set_counted_writes(true);
    n.host.set_counted_resolutions(true);
    n.write(&a, b"a1");
    let first = n.write(&c, b"c1");
    let counts = |n: &Node| n.core.state_machine(G).unwrap().doc_counts().cloned();
    assert_eq!(counts(&n), Some([(COLL, 2)].into()));
    let writes =
        [(a.as_slice(), put(b"a2")), (b.as_slice(), put(b"b1")), (c.as_slice(), IntentOp::Delete)];
    intents_mark(n.txn(intents(T1, first, &writes)));
    record(n.txn(TxnOp::EndTxn { txn: T1, op: begin(first) }));
    let commit = RecordOp::Commit { ts: n.host_mark(), spans: vec![] };
    let done = record(n.txn(TxnOp::EndTxn { txn: T1, op: commit }));
    let keys = vec![a.clone(), b.clone(), c.clone()];
    let resolve = TxnOp::ResolveIntents {
        txn: T1,
        status: TxnStatus::Committed,
        commit_ts: done.commit_ts,
        keys,
    };
    intents_mark(n.txn(resolve));
    assert_eq!(n.get(&b), Some(b"b1".to_vec()));
    assert_eq!(counts(&n), Some([(COLL, 2)].into()), "a replaced, b added, c deleted");
}

/// A request reads all its keys in one pass: with none of their blocks in the cache it asks for
/// every one of them at once, and waits once (H-051 of the native tests: one block at a time,
/// a transaction of 600 keys waited more than 10 s behind the disk's other work).
#[test]
fn a_request_asks_for_every_block_its_keys_miss_at_once() {
    const KEYS: usize = 32;
    let lcfg = teseladb_lsm::LsmConfig {
        dir: "core-00/lsm".into(),
        cache_bytes: 4 << 10,
        block_bytes: 4 << 10,
        ..teseladb_lsm::LsmConfig::default()
    };
    let mut n = Node::with_lsm(lcfg);
    let keys: Vec<Vec<u8>> = (0..KEYS).map(|i| format!("\x01k{i:03}").into_bytes()).collect();
    let mut last = Hlc { physical: 0, logical: 0 };
    for k in &keys {
        // A block of its own for each document.
        last = n.write(k, &[7; 3000]);
    }
    n.host.lsm().borrow_mut().flush();
    n.settle();
    let writes: Vec<(&[u8], IntentOp)> = keys.iter().map(|k| (k.as_slice(), put(b"x"))).collect();
    let (now, wall) = (n.now, n.wall());
    let op = intents(T1, last, &writes);
    let req = teseladb_range::host::TxnReq { g: G, op, wc: MAJORITY };
    let id = n.host.txn(&mut n.core, req, now, wall);
    // The first batch of reads of the LSM, before any of them is done.
    let mut reads = 0;
    let mut done = Vec::new();
    while let Some(op) = n.host.lsm().borrow_mut().poll_io(now) {
        if matches!(op, teseladb_raftlog::IoOp::ReadAt { .. }) {
            reads += 1;
        }
        done.push(teseladb_raftlog::Disk::execute(&mut n.disk, op));
    }
    for d in done {
        n.host.lsm().borrow_mut().on_io_done(d, now);
    }
    assert!(reads >= KEYS, "{reads} blocks asked for at once, {KEYS} keys");
    assert!(matches!(n.answer(id), Ok(Reply::Intents { .. })));
}

/// A write reads each of its keys once while the blocks they miss come back one by one: what it
/// read stays read under its latch, and each step reads only the keys still missing (H-051 of the
/// native tests: re-reading every key at each block made the writes of many keys take half of a
/// busy core).
#[test]
fn a_write_reads_each_key_once_while_its_blocks_come_back_one_by_one() {
    const KEYS: usize = 32;
    let lcfg = teseladb_lsm::LsmConfig {
        dir: "core-00/lsm".into(),
        cache_bytes: 4 << 10,
        block_bytes: 4 << 10,
        ..teseladb_lsm::LsmConfig::default()
    };
    let mut n = Node::with_lsm(lcfg);
    let keys: Vec<Vec<u8>> = (0..KEYS).map(|i| format!("\x01k{i:03}").into_bytes()).collect();
    for k in &keys {
        n.write(k, &[7; 3000]);
    }
    n.host.lsm().borrow_mut().flush();
    n.settle();
    let gets = |n: &Node| n.host.lsm().borrow().metrics().gets_total;
    let before = gets(&n);
    // One write that reads every key and writes the first.
    let m = Box::new(Set(keys[0].clone(), b"x".to_vec()));
    let w = WriteReq { g: G, keys: keys.clone(), m, dedup: None, wc: MAJORITY };
    let (now, wall) = (n.now, n.wall());
    let id = n.host.mutate(&mut n.core, w, now, wall);
    // The blocks come back one at a time, and the host steps after each. Reading every key again
    // at each step, with a cache this small, asked for blocks without end.
    let mut reads = 0;
    loop {
        let op = n.host.lsm().borrow_mut().poll_io(now);
        let Some(op) = op else { break };
        reads += 1;
        assert!(reads <= 10 * KEYS, "{reads} reads of the disk for {KEYS} keys");
        let done = teseladb_raftlog::Disk::execute(&mut n.disk, op);
        n.host.lsm().borrow_mut().on_io_done(done, now);
        n.host.poll(&mut n.core, now, wall);
    }
    assert!(matches!(n.answer(id), Ok(Reply::Mutated { .. })));
    let read = gets(&n) - before;
    assert!(read <= 2 * KEYS as u64, "{read} point reads for {KEYS} keys");
}

fn lock(txn: TxnId, read_ts: Hlc, keys: &[&[u8]]) -> TxnOp {
    TxnOp::LockKeys { txn, read_ts, keys: keys.iter().map(|k| k.to_vec()).collect() }
}

/// A plain write of `key`, with whatever the range answers.
fn try_write(n: &mut Node, key: &[u8]) -> Result<Reply, RangeError> {
    let (now, wall) = (n.now, n.wall());
    let m = Box::new(Set(key.to_vec(), b"plain".to_vec()));
    let w = WriteReq { g: G, keys: vec![key.to_vec()], m, dedup: None, wc: MAJORITY };
    let id = n.host.mutate(&mut n.core, w, now, wall);
    n.answer(id)
}

/// A transaction that holds its writes until the commit locks its keys on the leader (008
/// FR-022): the lock is refused as an intent is, another transaction's writes and locks conflict
/// with it, a write outside a transaction is told to wait, and nothing reaches the log.
#[test]
fn a_lock_stops_other_writers_until_its_transaction_commits_in_one_phase() {
    let mut n = Node::new();
    let written = n.write(A, b"v1");
    let applied = n.core.status(G).unwrap().last_index;
    // A version newer than the read mark refuses the lock, as it refuses an intent.
    let before = Hlc { physical: written.physical - 1, logical: 0 };
    assert_eq!(n.txn(lock(T1, before, &[A])), Err(RangeError::WriteConflict));
    assert_eq!(n.txn(lock(T1, written, &[A, B])), Ok(Reply::Intents { ts: written }));
    // Asked again by its owner it is granted again.
    assert_eq!(n.txn(lock(T1, written, &[A])), Ok(Reply::Intents { ts: written }));
    // Another transaction can neither lock the keys nor write them, in intents or in one phase.
    assert_eq!(n.txn(lock(T2, far(), &[B])), Err(RangeError::WriteConflict));
    assert_eq!(n.txn(intents(T2, far(), &[(B, put(b"y"))])), Err(RangeError::WriteConflict));
    let one = |txn, read_ts, v: &[u8]| TxnOp::OnePhase {
        txn,
        read_ts,
        writes: vec![(A.to_vec(), put(v)), (B.to_vec(), put(v))],
        session: None,
    };
    assert_eq!(n.txn(one(T2, far(), b"theirs")), Err(RangeError::WriteConflict));
    // A write outside a transaction waits for the lock.
    assert_eq!(try_write(&mut n, A), Err(RangeError::KeyLocked));
    // None of this wrote an entry.
    assert_eq!(n.core.status(G).unwrap().last_index, applied);
    // The owner commits in one phase, and its locks go with the commit.
    intents_mark(n.txn(one(T1, written, b"mine")));
    assert_eq!(n.get(A), Some(b"mine".to_vec()));
    assert!(matches!(try_write(&mut n, A), Ok(Reply::Mutated { .. })));
    // What the other transaction read is older than the commit now.
    assert_eq!(n.txn(lock(T2, written, &[B])), Err(RangeError::WriteConflict));
}

/// The locks of a transaction go when it asks (an abort) and when it stays idle for as long as
/// a transaction without a heartbeat counts as abandoned.
#[test]
fn locks_go_with_an_unlock_and_after_five_idle_seconds() {
    let mut n = Node::new();
    let base = n.write(A, b"v1");
    assert_eq!(n.txn(lock(T1, base, &[A])), Ok(Reply::Intents { ts: base }));
    let unlock = TxnOp::UnlockKeys { txn: T1, keys: vec![A.to_vec()] };
    assert!(n.txn(unlock).is_ok());
    assert_eq!(n.txn(lock(T2, far(), &[A])), Ok(Reply::Intents { ts: far() }));
    // T2 stays idle: its lock stops counting, and T1 takes the key.
    n.now = n.now + teseladb_types::Nanos::from_millis(teseladb_range::intents::ABANDONED_AFTER_MS);
    n.settle();
    assert_eq!(n.txn(lock(T1, far(), &[A])), Ok(Reply::Intents { ts: far() }));
    assert_eq!(n.txn(lock(T2, far(), &[A])), Err(RangeError::WriteConflict));
}

/// The one-phase commit of a client transaction leaves its decision in its range (008 FR-022):
/// sent again it answers the commit's mark without writing twice, and a recovery finds it.
#[test]
fn a_client_one_phase_commit_is_answered_again_from_its_record() {
    let mut n = Node::new();
    let base = n.write(A, b"v1");
    let one = |v: &[u8]| TxnOp::OnePhase {
        txn: T1,
        read_ts: base,
        writes: vec![(A.to_vec(), put(v))],
        session: Some((7, 3)),
    };
    let at = intents_mark(n.txn(one(b"mine")));
    assert_eq!(n.get(A), Some(b"mine".to_vec()));
    let applied = n.core.status(G).unwrap().last_index;
    // The same commit again, even with other bytes: its record answers, and nothing is written.
    assert_eq!(n.txn(one(b"again")), Ok(Reply::Intents { ts: at }));
    assert_eq!(n.get(A), Some(b"mine".to_vec()));
    assert_eq!(n.core.status(G).unwrap().last_index, applied);
    // A recovery from another node reads the decision there.
    let decide = |number| TxnOp::DecideOnePhase { lsid: 7, txn_number: number, at: A.to_vec() };
    assert_eq!(n.txn(decide(3)), Ok(Reply::Intents { ts: at }));
    assert_eq!(n.core.status(G).unwrap().last_index, applied);
}

/// A recovery that finds no commit of a transaction in a range refuses it from then on: the
/// commit that comes later writes nothing, and the session's older transactions cannot commit
/// once a newer one left its record.
#[test]
fn a_decision_asked_first_refuses_the_one_phase_commit_that_comes_later() {
    let mut n = Node::new();
    let base = n.write(A, b"v1");
    let decide = |number| TxnOp::DecideOnePhase { lsid: 7, txn_number: number, at: A.to_vec() };
    let one = |number| TxnOp::OnePhase {
        txn: T1,
        read_ts: far(),
        writes: vec![(A.to_vec(), put(b"late"))],
        session: Some((7, number)),
    };
    assert_eq!(n.txn(decide(3)), Ok(Reply::Batch(Vec::new())));
    assert_eq!(n.txn(decide(3)), Err(RangeError::NoSuchTransaction));
    assert_eq!(n.txn(one(3)), Err(RangeError::NoSuchTransaction));
    assert_eq!(n.get(A), Some(b"v1".to_vec()));
    // The session's next transaction commits, and an older number is then too old to.
    intents_mark(n.txn(one(4)));
    assert_eq!(n.get(A), Some(b"late".to_vec()));
    assert_eq!(n.txn(one(2)), Err(RangeError::NoSuchTransaction));
    assert_eq!(n.txn(decide(2)), Err(RangeError::NoSuchTransaction));
    assert!(base < far());
}
