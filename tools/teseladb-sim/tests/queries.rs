//! The `queries` workload (010 T020, FR-020): the distributed run of drawn pipelines over drawn
//! ranges gives what the reference evaluator gives, with and without faults.

use teseladb_sim::faults::FaultPlan;
use teseladb_sim::workloads::queries::Queries;
use teseladb_sim::{SimConfig, Verdict, run_workload};

fn cfg(faults: FaultPlan) -> SimConfig {
    SimConfig { workload: "queries".into(), faults, ..SimConfig::default() }
}

#[test]
fn distributed_pipelines_give_what_the_reference_gives() {
    for seed in 1..=5 {
        let r = run_workload(&cfg(FaultPlan::Default), seed, Queries::boxed(3, 2));
        assert_eq!(r.verdict, Verdict::Pass, "queries seed {seed}");
    }
    let r = run_workload(&cfg(FaultPlan::None), 9, Queries::boxed(3, 1));
    assert_eq!(r.verdict, Verdict::Pass);
}
