//! Canonical cases of the Raft paper. Figure 7: followers with missing entries, extra
//! entries or entries of other terms end up with the leader's log; a candidate with a stale log
//! does not win; and a property with random crashes, restarts and partitions in which the harness
//! checks one leader per term and the same entry applied at each index at every step.

mod common;

use common::*;
use teseladb_raft::Role;

#[test]
fn figure_7_followers_converge_to_the_leaders_log() {
    // Leader (term 8) and followers a-f of figure 7 (without `b`, `c`, `d`, which do not fit in 7
    // nodes with the others: a, d, e, f and a fifth one as the leader are used).
    let leader = [1, 1, 1, 4, 4, 5, 5, 6, 6, 6];
    let logs: [&[u64]; 6] = [
        &leader,
        &[1, 1, 1, 4, 4, 5, 5, 6, 6],          // a: one missing
        &[1, 1, 1, 4],                         // b: many missing
        &[1, 1, 1, 4, 4, 5, 5, 6, 6, 6, 7, 7], // d: two of an uncommitted term
        &[1, 1, 1, 4, 4, 4, 4],                // e: from another term
        &[1, 1, 1, 2, 2, 2, 3, 3, 3, 3, 3],    // f: from terms that were never committed
    ];
    let mut net = Net::new(6, 0, 77);
    for (i, log) in logs.iter().enumerate() {
        net.set_log(id(i as u64 + 1), log, 7);
    }
    // Only node 1 can win (its log is the most up to date among those with term 6 ... except
    // `d`, which has term 7): `d` is isolated for the election, as in the figure.
    net.isolate(id(4));
    net.ticks(40);
    let l = net.elect(500);
    assert!(l == id(1) || l == id(2), "a node with an up-to-date log wins: {l:?}");
    net.heal();
    net.propose(l, b"new").unwrap();
    net.ticks(100);
    let reference = net.log_terms(l);
    for n in 1..=6 {
        assert_eq!(net.log_terms(id(n)), reference, "node {n}");
        assert_eq!(net.data(id(n)).last().unwrap(), b"new");
    }
    // `d`'s term-7 entries and `f`'s term-2 and term-3 entries disappeared.
    assert!(!reference.contains(&7) && !reference.contains(&2));
}

#[test]
fn stale_candidate_cannot_win() {
    let mut net = Net::new(3, 0, 5);
    net.set_log(id(1), &[1], 3);
    net.set_log(id(2), &[1, 2, 3], 3);
    net.set_log(id(3), &[1, 2, 3], 3);
    net.ticks(40);
    // Node 1 gets no votes: the others have a more up-to-date log.
    net.isolate(id(2));
    for _ in 0..200 {
        net.tick();
        assert_ne!(net.role(id(1)), Role::Leader);
    }
    net.heal();
    let l = net.elect(400);
    assert_ne!(l, id(1));
}

#[test]
fn random_crashes_and_partitions_are_safe() {
    for seed in 0..40 {
        let mut net = Net::new(5, 0, 5000 + seed);
        net.elect(400);
        for step in 0..120 {
            match net.rng.range(0, 10) {
                0 => {
                    let n = id(net.rng.range(1, 6));
                    net.restart(n, cfg);
                }
                1 => {
                    net.heal();
                    let n = id(net.rng.range(1, 6));
                    net.isolate(n);
                }
                2 => net.heal(),
                _ => {
                    if let Some(l) = net.leader() {
                        let _ = net.propose(l, format!("{seed}/{step}").as_bytes());
                    }
                }
            }
            let n = net.rng.range(1, 6) as u32;
            net.ticks(n);
        }
        net.heal();
        net.ticks(200);
        let l = net.leader().expect("leader after healing");
        let reference = net.data(l);
        for n in 1..=5 {
            assert_eq!(net.data(id(n)), reference, "seed {seed}, node {n}");
        }
    }
}
