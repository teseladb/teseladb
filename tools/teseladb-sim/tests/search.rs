//! `search` and `vector` (011 T004, SC-002, SC-006): searchers keep segments of a search or vector
//! index while the ranges split and merge under transactions; every search answers at or above its
//! mark what one segment over the committed documents answers, with the same scores.

use teseladb_sim::faults::FaultPlan;
use teseladb_sim::{SimConfig, Verdict, run_seed};

fn cfg(workload: &str, faults: FaultPlan) -> SimConfig {
    SimConfig { workload: workload.into(), faults, ..SimConfig::default() }
}

#[test]
fn searches_answer_as_one_segment_over_the_committed_documents() {
    for workload in ["search", "vector"] {
        for (plan, seed) in [(FaultPlan::None, 1), (FaultPlan::None, 2), (FaultPlan::Default, 3)] {
            let r = run_seed(&cfg(workload, plan), seed);
            assert_eq!(r.verdict, Verdict::Pass, "{workload} seed {seed} with {plan:?}");
        }
    }
}

/// Heavy seeds that found defects with the cluster's third node out for good: 2361 left a range
/// born from a split without the snapshot its other replica needed (the leader's snapshot waited
/// for a memtable flush that a core applying something every second never did); 3772 left a node
/// asleep until its old clock would have passed the HLC bound, after NTP had set it right; 3796
/// let a deposed leader whose clock ran ahead resolve a search index's read up to a mark it closed
/// out of its lease, below a write of the new leader; 986 left a leader without lease reads (a
/// transfer to the damaged node that never finished) closing no mark until its next log write.
#[test]
fn the_seeds_that_found_defects_pass() {
    for seed in [2361, 3772, 3796, 986] {
        let r = run_seed(&cfg("search", FaultPlan::Heavy), seed);
        assert_eq!(r.verdict, Verdict::Pass, "search seed {seed}");
    }
}

/// Seed 1541 of the `default` plan: a replica of the right range of a merge waited for the data
/// of the leader's snapshot while its core was frozen for 160 ms, and the workload, which plays
/// the server's snapshot sink, dropped the wait instead of keeping it until the core thawed. The
/// replica never got its data and never voted again; with the third node lost to a damaged core,
/// the range had no leader from then on and no write was answered after the heal.
#[test]
fn a_snapshot_wait_outlives_a_frozen_core() {
    let r = run_seed(&cfg("search", FaultPlan::Default), 1541);
    assert_eq!(r.verdict, Verdict::Pass);
}

/// Seed 3541 of the `default` plan (`search` and `vector`): it turned red with the same schedule
/// change as seed 883 of `indexes`, and its run restarts replicas from their own snapshots, older
/// than what their LSMs flushed, that replay transactions' intents over the documents it
/// searches. It passes once such a replay no longer writes again what the LSM holds.
#[test]
fn a_restart_from_an_older_snapshot_answers_the_committed_documents() {
    for workload in ["search", "vector"] {
        let r = run_seed(&cfg(workload, FaultPlan::Default), 3541);
        assert_eq!(r.verdict, Verdict::Pass, "{workload} seed 3541");
    }
}
