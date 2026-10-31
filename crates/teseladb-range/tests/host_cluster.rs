//! The range host on three replicas (005 FR-007 to FR-010, D-034, DV-010): followers answer
//! `NotLeader` with the leader; `w: <n>` waits for n copies on disk even after the entry is
//! applied; a read waits for a write proposed below its mark, also after that write failed with an
//! uncertain outcome; a leader that loses its term fails
//! what it had not proposed and what it proposed and did not commit; a full proposal queue and a
//! stopped LSM are reported; and a read that needs a block from disk waits for it.

mod common;

use common::*;
use teseladb_multiraft::{OpError, OpId};
use teseladb_raftlog::IoErrno;

#[test]
fn followers_answer_not_leader_with_the_leader() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let f = c.others(l)[0];
    let w = c.mutate(f, KEY, 1, MAJORITY);
    let r = c.get(f, KEY, ReadConcern::Majority);
    c.settle();
    assert_eq!(c.reply(f, w), Some(Err(RangeError::NotLeader { hint: l })));
    assert_eq!(c.reply(f, r), Some(Err(RangeError::NotLeader { hint: l })));
    // An operation that is not the host's is left to the embedder.
    let node = c.nodes.get_mut(&f).unwrap();
    assert!(!node.host.on_done(&mut node.core, OpId(u64::MAX), Err(OpError::Timeout)));
}

#[test]
fn w_n_waits_for_n_copies_on_disk_even_after_the_entry_is_applied() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let slow = c.others(l)[1];
    c.slow.insert(slow);
    let w3 = c.mutate(l, KEY, 1, WriteConcern { w: W::Nodes(3), j: false });
    let w2 = c.mutate(l, KEY, 2, WriteConcern { w: W::Nodes(2), j: false });
    let w1 = c.mutate(l, KEY, 3, WriteConcern { w: W::Nodes(1), j: true });
    c.ticks(20);
    assert_eq!(c.reply(l, w3), None, "the third copy is not on disk yet");
    assert!(matches!(c.reply(l, w2), Some(Ok(Reply::Mutated { .. }))));
    assert!(matches!(c.reply(l, w1), Some(Ok(Reply::Mutated { .. }))));
    let r = c.get(l, KEY, ReadConcern::Majority);
    c.settle();
    assert_eq!(value(c.reply(l, r)), Some(3), "applied by the majority");
    c.slow.clear();
    c.ticks(20);
    assert!(matches!(c.reply(l, w3), Some(Ok(Reply::Mutated { .. }))));
}

#[test]
fn a_read_waits_for_a_write_proposed_below_its_mark() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let w = c.mutate(l, KEY, 1, MAJORITY);
    c.settle();
    assert!(matches!(c.reply(l, w), Some(Ok(_))));
    // The followers stop answering: the next write is proposed and stays uncommitted.
    let followers = c.others(l);
    c.slow.extend(followers);
    let w = c.mutate(l, KEY, 2, MAJORITY);
    c.settle();
    assert_eq!(c.reply(l, w), None);
    // Within the lease the read is admitted at a mark above that write: it waits for it.
    let r = c.get(l, KEY, ReadConcern::Majority);
    c.settle();
    assert_eq!(c.reply(l, r), None, "it waits for the write below its mark");
    c.slow.clear();
    c.ticks(10);
    assert!(matches!(c.reply(l, w), Some(Ok(_))));
    assert_eq!(value(c.reply(l, r)), Some(2));
}

#[test]
fn a_write_with_an_uncertain_outcome_keeps_reads_above_it_waiting() {
    // Seed 210 of range-docs heavy: a delete failed without knowing whether it would commit, and a
    // read at a mark above it answered the version before it; the delete was applied later.
    let mut c = Cluster::standard();
    let l = c.elect();
    let w = c.mutate(l, KEY, 1, MAJORITY);
    c.settle();
    assert!(matches!(c.reply(l, w), Some(Ok(_))));
    let followers = c.others(l);
    c.slow.extend(followers);
    let w = c.mutate(l, KEY, 2, MAJORITY);
    // The core gives up on it after its deadline: the entry is still in the log.
    c.ticks(510);
    assert_eq!(c.reply(l, w), Some(Err(RangeError::Timeout)));
    let r = c.get(l, KEY, ReadConcern::Majority);
    c.settle();
    assert_eq!(c.reply(l, r), None, "the write below its mark may still be applied");
    c.slow.clear();
    c.ticks(10);
    assert_eq!(value(c.reply(l, r)), Some(2), "it was, and the read sees it");
    // Its latch went with it: the next writes go through.
    let w = c.mutate(l, KEY, 3, MAJORITY);
    c.ticks(5);
    assert!(matches!(c.reply(l, w), Some(Ok(_))));
}

#[test]
fn a_leader_that_loses_its_term_fails_what_it_had_not_committed() {
    let mut c = Cluster::standard();
    let l = c.elect();
    c.isolate(l);
    let proposed = c.mutate(l, KEY, 1, MAJORITY);
    let waiting = c.mutate(l, KEY, 2, MAJORITY);
    let reading = c.get(l, KEY, ReadConcern::Linearizable);
    c.settle();
    assert_eq!((c.reply(l, proposed), c.reply(l, waiting)), (None, None));
    // The other two elect a leader of a new term; the old one hears of it when the links come
    // back.
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
    c.cut.clear();
    c.ticks(50);
    for r in [proposed, waiting, reading] {
        assert!(matches!(c.reply(l, r), Some(Err(_))), "{:?}", c.reply(l, r));
    }
    // It steps down when it hears of the new term, maybe before it knows who leads it.
    assert!(matches!(c.reply(l, waiting), Some(Err(RangeError::NotLeader { .. }))));
    let w = c.mutate(l2, KEY, 9, MAJORITY);
    let r = c.get(l2, KEY, ReadConcern::Linearizable);
    c.ticks(5);
    assert!(matches!(c.reply(l2, w), Some(Ok(_))));
    assert_eq!(value(c.reply(l2, r)), Some(9));
}

#[test]
fn a_full_proposal_queue_answers_overloaded() {
    let mut c = Cluster::new(LsmConfig { dir: "core-00/lsm".into(), ..LsmConfig::default() }, 1);
    let l = c.elect();
    let followers = c.others(l);
    c.slow.extend(followers);
    let first = c.mutate(l, b"\x01doc", 1, MAJORITY);
    let second = c.mutate(l, b"\x01other", 2, MAJORITY);
    c.settle();
    assert_eq!(c.reply(l, first), None);
    assert_eq!(c.reply(l, second), Some(Err(RangeError::Overloaded)));
}

#[test]
fn a_stopped_lsm_is_reported_to_writes_and_reads() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let w = c.mutate(l, KEY, 1, MAJORITY);
    c.settle();
    assert!(matches!(c.reply(l, w), Some(Ok(_))));
    let node = c.nodes.get_mut(&l).unwrap();
    node.disk.inject_error("create", IoErrno::Io);
    node.host.lsm().borrow_mut().flush();
    c.settle();
    assert!(c.nodes[&l].host.lsm().borrow().health().is_err());
    let w = c.mutate(l, KEY, 2, MAJORITY);
    let r = c.get(l, KEY, ReadConcern::Majority);
    let s = c.scan(l, START, END, 10);
    c.settle();
    assert_eq!(c.reply(l, w), Some(Err(RangeError::Unavailable)));
    assert_eq!(c.reply(l, r), Some(Err(RangeError::Unavailable)));
    assert_eq!(c.reply(l, s), Some(Err(RangeError::Unavailable)));
}

#[test]
fn reads_and_writes_wait_for_blocks_from_disk() {
    let lsm = LsmConfig {
        dir: "core-00/lsm".into(),
        memtable_bytes: 16 << 10,
        block_bytes: 4096,
        cache_bytes: 8 << 10,
        ..LsmConfig::default()
    };
    let mut c = Cluster::new(lsm, 1024);
    let l = c.elect();
    let keys: Vec<Vec<u8>> = (0..200u32).map(|i| format!("\x01k{i:04}").into_bytes()).collect();
    for (i, k) in keys.iter().enumerate() {
        let (now, wall) = (c.now, c.wall());
        let node = c.nodes.get_mut(&l).unwrap();
        let w = WriteReq {
            g: G,
            keys: vec![k.clone()],
            m: Box::new(Put1(k.clone(), i as u8)),
            dedup: None,
            wc: MAJORITY,
        };
        node.host.mutate(&mut node.core, w, now, wall);
        c.settle();
    }
    c.nodes[&l].host.lsm().borrow_mut().flush();
    c.settle();
    // Old keys are on disk and out of the small cache.
    let r = c.get(l, &keys[3], ReadConcern::Majority);
    let (now, wall) = (c.now, c.wall());
    let node = c.nodes.get_mut(&l).unwrap();
    let w = WriteReq {
        g: G,
        keys: vec![keys[5].clone()],
        m: Box::new(Put1(keys[5].clone(), 99)),
        dedup: None,
        wc: MAJORITY,
    };
    let w = node.host.mutate(&mut node.core, w, now, wall);
    let s = c.scan(l, START, END, 1_000);
    c.settle();
    assert_eq!(value(c.reply(l, r)), Some(3));
    match c.reply(l, s) {
        Some(Ok(Reply::Batch(rows))) => assert_eq!(rows.len(), keys.len()),
        other => panic!("{other:?}"),
    }
    assert!(matches!(c.reply(l, w), Some(Ok(_))));
}

/// Writes one byte at a given key.
struct Put1(Vec<u8>, u8);

impl Mutation for Put1 {
    fn eval(&self, _: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        Ok(Outcome {
            puts: vec![Put { key: self.0.clone(), value_type: 1, value: vec![self.1] }],
            deletes: vec![],
            change: vec![],
            result: vec![],
        })
    }
}

#[test]
fn a_full_level_0_answers_overloaded_until_compaction_empties_it() {
    // 004 `lsm-api.md`: `Overloaded` is the leader's answer when the LSM's pressure is `Reject`
    // (audit A005-2).
    let lsm = LsmConfig {
        dir: "core-00/lsm".into(),
        l0_trigger: 1000,
        l0_slow: 1,
        l0_reject: 2,
        ..LsmConfig::default()
    };
    let mut c = Cluster::new(lsm, 1024);
    let l = c.elect();
    for (i, key) in [b"\x01a1", b"\x01a2"].into_iter().enumerate() {
        let w = c.mutate_key(l, key, i as u8, MAJORITY);
        c.settle();
        assert!(matches!(c.reply(l, w), Some(Ok(Reply::Mutated { .. }))));
        let now = c.now;
        let node = c.nodes.get_mut(&l).unwrap();
        let lsm = node.host.lsm().clone();
        lsm.borrow_mut().flush();
        lsm.borrow_mut().run_io(&mut node.disk, now);
    }
    let refused = c.mutate_key(l, b"\x01a3", 3, MAJORITY);
    c.settle();
    assert_eq!(c.reply(l, refused), Some(Err(RangeError::Overloaded)));
    let now = c.now;
    let node = c.nodes.get_mut(&l).unwrap();
    let lsm = node.host.lsm().clone();
    lsm.borrow_mut().compact_all();
    lsm.borrow_mut().run_io(&mut node.disk, now);
    let w = c.mutate_key(l, b"\x01a3", 3, MAJORITY);
    c.settle();
    assert!(matches!(c.reply(l, w), Some(Ok(Reply::Mutated { .. }))));
}

/// The clocks of a majority jump back together (005 FR-005, FR-006, D-044): within margin of
/// each other, the jumped nodes still stay aside until their wall clocks pass the highest time
/// they read before the jump, and then the new leader writes above the reads the old leader
/// served before it.
#[test]
fn a_majority_whose_clocks_went_back_leads_only_after_they_come_back() {
    let mut c = Cluster::standard();
    let old = c.elect();
    let w = c.mutate(old, KEY, 1, MAJORITY);
    c.settle();
    assert!(matches!(c.reply(old, w), Some(Ok(_))));
    // A second without writes: the last read is well above the marks of the log.
    c.ticks(100);
    let r = c.get(old, KEY, ReadConcern::Majority);
    c.settle();
    let Some(Ok(Reply::Value { ts: read, .. })) = c.reply(old, r) else { panic!("read") };
    // The other two clocks jump 1.5 s back, one after the other, and the old leader, out of
    // margin with them, is cut off: the jumped nodes hear only each other.
    let others = c.others(old);
    c.skew.insert(others[0], -1_500);
    c.ticks(10);
    c.skew.insert(others[1], -1_500);
    c.isolate(old);
    for _ in 0..120 {
        c.ticks(1);
        assert!(c.leader().is_none_or(|l| l == old), "no jumped node leads while it is behind");
    }
    let mut new = old;
    for _ in 0..JUMP_TICKS * 2 {
        c.ticks(1);
        if let Some(l) = c.leader().filter(|&l| l != old) {
            new = l;
            break;
        }
    }
    assert_ne!(new, old, "a node of the majority leads again");
    let mut written = None;
    for _ in 0..JUMP_TICKS {
        let w = c.mutate(new, KEY, 2, MAJORITY);
        c.ticks(1);
        if let Some(Ok(Reply::Mutated { ts, .. })) = c.reply(new, w) {
            written = Some(ts);
            break;
        }
    }
    let written = written.expect("the new leader writes");
    assert!(written > read, "write at {written:?} below the read at {read:?}");
}

/// Seed 12493 of range-docs heavy (005 FR-005): with every node's HLC well ahead of its wall
/// clock (a leader whose clock was ahead wrote before), cluster time stands still across terms. A
/// leader cut off right after its election raised its clock to its read cache with a write that
/// never left it, and served a read above it with the lease of its election; the next leader,
/// whose clock its voters had raised only as far as before that write, started its cache at the
/// same minimum and wrote below that read. The leader raises its clock to its cache before
/// anything of its term goes out, so its voters carry it to the next one.
#[test]
fn a_new_leader_writes_above_reads_served_while_cluster_time_stood_still() {
    let mut c = Cluster::standard();
    let ahead = Hlc { physical: c.wall() + 5_000, logical: 0 };
    for n in c.nodes.values() {
        n.clock.0.update(ahead);
    }
    let old = c.elect();
    // A round of heartbeats gives it the lease.
    c.ticks(5);
    c.isolate(old);
    let w = c.mutate(old, KEY, 1, MAJORITY);
    let r = c.get(old, b"\x01other", ReadConcern::Majority);
    c.settle();
    assert_eq!(c.reply(old, w), None, "cut off, the write does not commit");
    let Some(Ok(Reply::Value { ts: read, .. })) = c.reply(old, r) else {
        panic!("a read with the lease: {:?}", c.reply(old, r))
    };
    let mut new = old;
    for _ in 0..1_000 {
        c.ticks(1);
        if let Some(l) = c.others(old).into_iter().find(|n| {
            c.nodes[n].core.status(G).is_some_and(|s| s.role == Role::Leader && s.applied > 0)
        }) {
            new = l;
            break;
        }
    }
    assert_ne!(new, old, "the other two elect a leader");
    let mut written = None;
    for _ in 0..JUMP_TICKS {
        let w = c.mutate_key(new, b"\x01other", 2, MAJORITY);
        c.ticks(1);
        if let Some(Ok(Reply::Mutated { ts, .. })) = c.reply(new, w) {
            written = Some(ts);
            break;
        }
    }
    let written = written.expect("the new leader writes");
    assert!(written > read, "write at {written:?} below the read at {read:?}");
}
