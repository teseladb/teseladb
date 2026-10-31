//! The `special-collections` workload (015 T002, FR-011): a capped collection written through the
//! code of a node, its limits and its state checked on every read.

use teseladb_sim::faults::FaultPlan;
use teseladb_sim::workloads::special_collections::SpecialCollections;
use teseladb_sim::{SimConfig, Verdict, run_workload};

fn cfg(faults: FaultPlan) -> SimConfig {
    SimConfig { workload: "special-collections".into(), faults, ..SimConfig::default() }
}

#[test]
fn a_capped_collection_keeps_its_limits_and_state_under_faults() {
    for seed in 1..=5 {
        let r = run_workload(&cfg(FaultPlan::Default), seed, SpecialCollections::boxed(3, 2));
        assert_eq!(r.verdict, Verdict::Pass, "special-collections seed {seed}");
    }
}

#[test]
fn without_faults_the_collection_fills_and_evicts() {
    let r = run_workload(&cfg(FaultPlan::None), 7, SpecialCollections::boxed(3, 2));
    assert_eq!(r.verdict, Verdict::Pass);
}
