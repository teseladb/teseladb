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
    let late = inc(&mut c, l, Some(K));
    c.ticks(2);
    assert_eq!(mutated(c.reply(l, late)).1, vec![2], "the older one went");
}
