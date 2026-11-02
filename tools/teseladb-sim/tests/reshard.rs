//! The `reshard` workload (018 T002, FR-002, FR-013): a collection rewritten in a new key order
//! with the steps of `teseladb-wire`'s resharding while clients write it, every read of the new
//! order checked against the committed writes.

use teseladb_sim::faults::FaultPlan;
use teseladb_sim::workloads::reshard::Reshard;
use teseladb_sim::{SimConfig, Verdict, run_workload};

fn cfg(faults: FaultPlan) -> SimConfig {
    SimConfig { workload: "reshard".into(), faults, ..SimConfig::default() }
}

#[test]
fn a_resharding_loses_no_write_under_faults() {
    for seed in 1..=5 {
        let r = run_workload(&cfg(FaultPlan::Default), seed, Reshard::boxed(3, 2));
        assert_eq!(r.verdict, Verdict::Pass, "reshard seed {seed}: {:?}", r.verdict);
    }
}

#[test]
fn without_faults_the_resharding_commits() {
    let r = run_workload(&cfg(FaultPlan::None), 7, Reshard::boxed(3, 2));
    assert_eq!(r.verdict, Verdict::Pass, "{:?}", r.verdict);
}
