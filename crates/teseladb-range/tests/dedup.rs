//! Retryable writes (005 FR-011): the range keeps `(lsid, txnNumber, stmtId) → result` in its local
//! keys at the write's mark. A retry after a lost answer, while the original is still in flight or
//! after a change of leader gets the original result and mark and writes nothing; the records go
//! 30 minutes of cluster time later; and a snapshot carries them.

mod common;

use common::*;
use teseladb_range::dedup::{DEDUP_TTL_MS, PURGE_EVERY_MS};
use teseladb_range::sm::dedup_key;
use teseladb_range::snapshot::local_interval;

/// Adds one to the byte of its document and answers what it found.
struct Inc;

impl Mutation for Inc {
    fn eval(&self, current: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        let found = current[0].map_or(0, |(_, v)| v[0]);
        Ok(Outcome {
            puts: vec![Put { key: KEY.to_vec(), value_type: 1, value: vec![found + 1] }],
            deletes: vec![],
            change: vec![b'+'],
            result: vec![found],
        })
    }
}

const K: DedupKey = DedupKey { lsid: 0xabcd, txn_number: 7, stmt_id: 0 };

fn inc(c: &mut Cluster, n: NodeId, dedup: Option<DedupKey>) -> ReqId {
    c.mutate_with(n, &[KEY], Box::new(Inc), dedup)
}

fn mutated(r: Option<Result<Reply, RangeError>>) -> (Hlc, Vec<u8>) {
    match r {
        Some(Ok(Reply::Mutated { ts, result, .. })) => (ts, result),
        other => panic!("not a write: {other:?}"),
    }
}

fn read(c: &mut Cluster, n: NodeId) -> Option<u8> {
    let r = c.get(n, KEY, ReadConcern::Majority);
    c.settle();
    value(c.reply(n, r))
}

#[test]
fn a_retry_after_a_lost_answer_gets_the_original_result() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let first = inc(&mut c, l, Some(K));
    c.ticks(2);
    let (ts, result) = mutated(c.reply(l, first));
    assert_eq!(result, vec![0]);
    // The answer was lost: the driver sends the same statement again.
    let again = inc(&mut c, l, Some(K));
    c.ticks(2);
    assert_eq!(mutated(c.reply(l, again)), (ts, vec![0]), "the original mark and result");
    match c.reply(l, again) {
        Some(Ok(Reply::Mutated { change, .. })) => assert!(change.is_empty(), "nothing written"),
        other => panic!("{other:?}"),
    }
    assert_eq!(read(&mut c, l), Some(1), "applied once");
    // Another statement of the same session is a new write.
    let next = inc(&mut c, l, Some(DedupKey { stmt_id: 1, ..K }));
    c.ticks(2);
    assert_eq!(mutated(c.reply(l, next)).1, vec![1]);
    // Without a key every send is a new write.
    let plain = inc(&mut c, l, None);
    c.ticks(2);
    assert_eq!(mutated(c.reply(l, plain)).1, vec![2]);
    assert_eq!(read(&mut c, l), Some(3));
}

#[test]
fn a_retry_while_the_original_is_in_flight_waits_for_it() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let followers = c.others(l);
    c.slow.extend(followers);
    let first = inc(&mut c, l, Some(K));
    let again = inc(&mut c, l, Some(K));
    c.settle();
    assert_eq!((c.reply(l, first), c.reply(l, again)), (None, None), "nothing committed yet");
    c.slow.clear();
    c.ticks(5);
    assert_eq!(mutated(c.reply(l, first)), mutated(c.reply(l, again)));
    assert_eq!(read(&mut c, l), Some(1));
}

#[test]
fn a_retry_on_a_new_leader_gets_the_original_result() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let first = inc(&mut c, l, Some(K));
    c.ticks(2);
    let original = mutated(c.reply(l, first));
    // The leader is cut off before the driver hears; another node takes over.
    c.isolate(l);
    let mut l2 = None;
    for _ in 0..1_000 {
        c.ticks(1);
        l2 = c
            .others(l)
            .into_iter()
            .find(|n| c.nodes[n].core.status(G).unwrap().role == Role::Leader);
        if l2.is_some() {
            break;
        }
    }
    let l2 = l2.expect("a new leader");
    c.ticks(5);
    let again = inc(&mut c, l2, Some(K));
    c.ticks(5);
    assert_eq!(mutated(c.reply(l2, again)), original);
    assert_eq!(read(&mut c, l2), Some(1));
}

#[test]
fn records_go_thirty_minutes_of_cluster_time_later() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let first = inc(&mut c, l, Some(K));
    c.ticks(2);
    assert_eq!(mutated(c.reply(l, first)).1, vec![0]);
    // Cluster time moves on 29 minutes: the record stays.
    c.wall_ahead = DEDUP_TTL_MS - 60_000;
    c.ticks(200);
    let l = c.leader().expect("a leader");
    let again = inc(&mut c, l, Some(K));
    c.ticks(2);
    assert_eq!(mutated(c.reply(l, again)).1, vec![0], "still recorded");
    // Past the 30 minutes and a purge round, the same key is a new write.
    c.wall_ahead = DEDUP_TTL_MS + 2 * PURGE_EVERY_MS;
    c.ticks(200);
    let l = c.leader().expect("a leader");
    let late = inc(&mut c, l, Some(K));
    c.ticks(2);
    assert_eq!(mutated(c.reply(l, late)).1, vec![1], "purged: evaluated again");
    assert_eq!(read(&mut c, l), Some(2));
}

#[test]
fn records_are_local_keys_of_the_range_so_its_snapshot_carries_them() {
    let (start, end) = local_interval(G);
    let k = dedup_key(G, K.lsid, K.txn_number, K.stmt_id);
    assert!(start <= k && k < end);
    let other = dedup_key(GroupId(G.0 + 1), K.lsid, K.txn_number, K.stmt_id);
    assert!(other >= end, "another range's records are not in it");
}

#[test]
fn a_purge_keeps_newer_records_and_a_later_round_skips_the_deleted_ones() {
    const NEWER: DedupKey = DedupKey { lsid: 0xabcd, txn_number: 8, stmt_id: 0 };
    let mut c = Cluster::standard();
    let l = c.elect();
    let first = inc(&mut c, l, Some(K));
    c.ticks(2);
    assert_eq!(mutated(c.reply(l, first)).1, vec![0]);
    // A second record 20 minutes later.
    c.wall_ahead = DEDUP_TTL_MS * 2 / 3;
    c.ticks(200);
    let l = c.leader().expect("a leader");
    let newer = inc(&mut c, l, Some(NEWER));
    c.ticks(2);
    assert_eq!(mutated(c.reply(l, newer)).1, vec![1]);
    // Past the first record's 30 minutes only: it goes and the newer one stays.
    c.wall_ahead = DEDUP_TTL_MS + 2 * PURGE_EVERY_MS;
    c.ticks(200);
    // Two more rounds go over the deleted record without proposing it again.
    for _ in 0..2 {
        c.wall_ahead += PURGE_EVERY_MS;
        c.ticks(200);
    }
    let l = c.leader().expect("a leader");
    let again = inc(&mut c, l, Some(NEWER));
    c.ticks(2);
    assert_eq!(mutated(c.reply(l, again)).1, vec![1], "the newer record stays");
    // The older one went, and its session has a newer `txnNumber` here: too old, as the reference
    // server answers it.
    let late = inc(&mut c, l, Some(K));
    c.ticks(2);
    assert_too_old(c.reply(l, late), 7, 8);
    assert_eq!(read(&mut c, l), Some(2));
}

fn assert_too_old(r: Option<Result<Reply, RangeError>>, txn: i64, newest: i64) {
    match r {
        Some(Err(RangeError::Mutation(e))) => assert_eq!(
            e.0,
            format!(
                "225\u{0}Retryable write with txnNumber {txn} is prohibited on session because a \
                 newer retryable write with txnNumber {newest} has already started on this session."
            )
        ),
        other => panic!("not too old: {other:?}"),
    }
}

#[test]
fn a_session_keeps_the_records_of_its_newest_txn_number_only() {
    // As a shard of the reference server keeps one record per session (`config.transactions`): a
    // purge round drops the records a newer `txnNumber` of their session replaced, long before 30
    // minutes.
    const NEWER: DedupKey = DedupKey { lsid: 0xabcd, txn_number: 8, stmt_id: 0 };
    const OTHER: DedupKey = DedupKey { lsid: 0x1234, txn_number: 3, stmt_id: 0 };
    let mut c = Cluster::standard();
    let l = c.elect();
    for k in [K, OTHER, NEWER] {
        let w = inc(&mut c, l, Some(k));
        c.ticks(2);
        mutated(c.reply(l, w));
    }
    // Before a purge the older record still answers its retry.
    let retry = inc(&mut c, l, Some(K));
    c.ticks(2);
    assert_eq!(mutated(c.reply(l, retry)).1, vec![0]);
    // One purge round later.
    c.wall_ahead = 2 * PURGE_EVERY_MS;
    c.ticks(200);
    let l = c.leader().expect("a leader");
    let old = inc(&mut c, l, Some(K));
    c.ticks(2);
    assert_too_old(c.reply(l, old), 7, 8);
    for (k, result) in [(NEWER, 2), (OTHER, 1)] {
        let again = inc(&mut c, l, Some(k));
        c.ticks(2);
        assert_eq!(mutated(c.reply(l, again)).1, vec![result], "{k:?} keeps its record");
    }
    assert_eq!(read(&mut c, l), Some(3), "no retry wrote again");
}

/// A record keeps the greatest key its statement wrote before its result (007 FR-022), and a
/// record written before records kept it still reads with its mark and result.
#[test]
fn a_record_keeps_the_greatest_key_its_statement_wrote() {
    use teseladb_range::command::CopiedRecord;
    use teseladb_range::local_keys::{reaches, record_of};
    use teseladb_range::sm::{
        parse_record_value, record_reach, record_value, record_value_reaching,
    };
    let mark = Hlc { physical: 1_794_000_000_000, logical: 7 };
    let kept = record_value_reaching(mark, b"\x01q", b"result");
    assert_eq!(parse_record_value(&kept), Some((mark, &b"result"[..])));
    assert_eq!(record_reach(&kept), Some(&b"\x01q"[..]));
    let old = record_value(mark, b"result");
    assert_eq!(parse_record_value(&old), Some((mark, &b"result"[..])));
    assert_eq!(record_reach(&old), None);
    // A copy keeps the key: a split copies it as it is stored.
    let key = dedup_key(G, K.lsid, K.txn_number, K.stmt_id);
    let copy: CopiedRecord = record_of(&key, &kept).expect("a record");
    assert_eq!((copy.mark, &copy.result[..]), (mark, &b"result"[..]));
    assert_eq!(copy.reach.as_deref(), Some(&b"\x01q"[..]));
    // The new range stores it as the parent did.
    let born = teseladb_range::local_keys::split_writes(
        GroupId(9),
        b"\x02",
        mark,
        mark,
        std::slice::from_ref(&copy),
    );
    assert!(born.iter().any(|(_, v)| *v == kept));
    assert!(reaches(&copy, b"\x01m") && reaches(&copy, b"\x01q") && !reaches(&copy, b"\x01r"));
    // One that does not keep it goes to the new range, as before.
    assert!(reaches(&record_of(&key, &old).expect("a record"), b"\x01z"));
}
