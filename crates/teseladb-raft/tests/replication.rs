//! Replication, majority commit, lagging followers, divergent logs and flow control.

mod common;

use common::*;
use teseladb_raft::{ProposeError, RaftConfig};

#[test]
fn entries_replicate_and_commit_on_majority() {
    let mut net = Net::new(3, 0, 1);
    let l = net.elect(200);
    for i in 0..50u32 {
        net.propose(l, &i.to_le_bytes()).expect("propose succeeds");
    }
    net.ticks(5);
    let want: Vec<Vec<u8>> = (0..50u32).map(|i| i.to_le_bytes().to_vec()).collect();
    for n in 1..=3 {
        assert_eq!(net.data(id(n)), want, "node {n}");
    }
    // With one follower down it keeps committing (majority 2 of 3).
    let f = (1..=3).map(id).find(|&n| n != l).expect("find succeeds");
    net.isolate(f);
    net.propose(l, b"without one").expect("propose succeeds");
    net.ticks(3);
    assert_eq!(net.data(l).last().expect("last succeeds"), b"without one");
    assert_ne!(net.data(f).last().expect("last succeeds"), b"without one");
    // When it comes back it catches up.
    net.heal();
    net.ticks(20);
    assert_eq!(net.data(f), net.data(l));
}

#[test]
fn nothing_commits_without_majority() {
    let mut net = Net::new(3, 0, 2);
    let l = net.elect(200);
    for n in 1..=3 {
        if id(n) != l {
            net.isolate(id(n));
        }
    }
    net.propose(l, b"lost").expect("propose succeeds");
    net.ticks(5);
    assert!(!net.data(l).contains(&b"lost".to_vec()));
}

#[test]
fn far_behind_follower_catches_up_through_probing() {
    let mut net = Net::with_cfg(3, 0, 3, |i| RaftConfig { max_msg_bytes: 64, ..cfg(i) });
    let l = net.elect(200);
    let f = (1..=3).map(id).find(|&n| n != l).expect("find succeeds");
    net.isolate(f);
    for i in 0..300u32 {
        net.propose(l, &i.to_le_bytes()).expect("propose succeeds");
    }
    net.heal();
    net.ticks(60);
    assert_eq!(net.data(f).len(), 300);
}

#[test]
fn proposals_too_large_or_while_not_leader_are_rejected() {
    let mut net = Net::with_cfg(3, 0, 4, |i| RaftConfig { max_entry_bytes: 10, ..cfg(i) });
    let l = net.elect(200);
    assert_eq!(net.propose(l, &[0; 11]), Err(ProposeError::TooLarge));
    let f = (1..=3).map(id).find(|&n| n != l).expect("find succeeds");
    assert_eq!(net.propose(f, b"x"), Err(ProposeError::NotLeader { hint: l }));
}

#[test]
fn randomized_partitions_keep_logs_consistent() {
    // Random partitions while proposing: the harness checks one leader per term and the same
    // entry applied at each index by everyone at every step.
    for seed in 0..30 {
        let mut net = Net::new(5, 0, 1000 + seed);
        net.elect(400);
        let mut proposed = 0;
        for round in 0..60 {
            if round % 10 == 0 {
                net.heal();
                let victim = id(net.rng.range(1, 6));
                net.isolate(victim);
            }
            if let Some(l) = net.leader()
                && net.propose(l, format!("{seed}-{round}").as_bytes()).is_ok()
            {
                proposed += 1;
            }
            net.ticks(3);
        }
        net.heal();
        net.ticks(100);
        let l = net.leader().expect("leader succeeds");
        let reference = net.data(l);
        for n in 1..=5 {
            assert_eq!(net.data(id(n)), reference, "seed {seed}, node {n}");
        }
        assert!(proposed > 0);
    }
}

#[test]
fn entries_of_an_earlier_term_are_committed_only_with_one_of_the_current_term() {
    // One entry per message, so that followers acknowledge the old entries one by one: counting
    // replicas never commits an entry of an earlier term (Raft §5.4.2), the leader's first entry
    // of its own term commits them all.
    let one_per_message = |i| RaftConfig { max_msg_bytes: 1, ..cfg(i) };
    let mut net = Net::with_cfg(3, 0, 71, one_per_message);
    net.set_log(id(1), &[1, 1, 1], 1);
    net.set_log(id(2), &[], 1);
    net.set_log(id(3), &[], 1);
    for n in 1..=3 {
        net.restart(id(n), one_per_message);
    }
    // With 3 cut off, only 1 can win: 2 has an empty log and 1 does not vote for it.
    net.isolate(id(3));
    let l = net.elect(500);
    assert_eq!(l, id(1));
    net.ticks(10);
    net.heal();
    net.ticks(10);
    for n in (1..=3).map(id) {
        assert_eq!(net.data(n), vec![b"1.1".to_vec(), b"1.2".to_vec(), b"1.3".to_vec()], "{n:?}");
        assert_eq!(net.log_terms(n)[..3], [1, 1, 1]);
    }
}

#[test]
fn the_status_counts_the_replicas_with_an_entry_on_disk() {
    // 005 FR-010: `w: 1, j: true` waits for the leader's disk and `w: <n>` for n replicas.
    let mut net = Net::new(3, 0, 4);
    let l = net.elect(200);
    net.propose(l, b"a").expect("propose succeeds");
    net.ticks(5);
    let st = |net: &Net| {
        let node = &net.nodes[&l];
        node.raw.status(&node.store)
    };
    let first = st(&net).last_index;
    assert_eq!((st(&net).persisted, st(&net).durable_replicas(first)), (first, 3));
    for n in (1..=3).map(id).filter(|&n| n != l) {
        net.isolate(n);
    }
    net.propose(l, b"b").expect("propose succeeds");
    net.ticks(3);
    let second = st(&net).last_index;
    assert!(second > first);
    assert_eq!(st(&net).persisted, second, "on the leader's disk");
    assert_eq!(st(&net).durable_replicas(second), 1, "only the leader has it");
    assert_eq!(st(&net).durable_replicas(first), 3);
    net.heal();
    net.ticks(20);
    assert_eq!(st(&net).durable_replicas(second), 3);
}
