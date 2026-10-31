//! Same seed → same trace (and a different one with another seed), also with faults and
//! with collections whose hash order is random per process (002 FR-006, SC-001).

use teseladb_sim::faults::FaultPlan;
use teseladb_sim::{SimConfig, Verdict, run_seed};
use teseladb_types::Nanos;

/// 8 simulated seconds: 3 s of faults and 5 s to heal.
fn cfg(faults: FaultPlan) -> SimConfig {
    SimConfig { sim_time: Nanos::from_secs(8), faults, ..SimConfig::default() }
}

#[test]
fn same_seed_same_trace() {
    for plan in [FaultPlan::None, FaultPlan::Default, FaultPlan::Heavy] {
        for seed in [1, 7, 12] {
            let a = run_seed(&cfg(plan), seed);
            let b = run_seed(&cfg(plan), seed);
            assert_eq!(a.trace_hash, b.trace_hash, "seed {seed} with {plan:?}");
            assert_eq!(a.events, b.events);
            assert!(a.events > 1000);
            assert_eq!(a.verdict, Verdict::Pass, "seed {seed} with {plan:?}");
        }
    }
    let a = run_seed(&cfg(FaultPlan::Default), 1);
    let c = run_seed(&cfg(FaultPlan::Default), 2);
    assert_ne!(a.trace_hash, c.trace_hash);
}

#[test]
fn replay_stops_at_the_event_and_keeps_the_prefix() {
    let full = run_seed(&SimConfig { trace: true, ..cfg(FaultPlan::Default) }, 3);
    let part =
        run_seed(&SimConfig { trace: true, until_event: Some(500), ..cfg(FaultPlan::Default) }, 3);
    assert_eq!(part.events, 500);
    let (full, part) = (full.trace_lines.unwrap(), part.trace_lines.unwrap());
    assert_eq!(part.len(), 500);
    assert_eq!(&full[..500], &part[..]);
}

/// Runs that should give one trace: SC-001 asks for a hundred.
const RUNS: usize = 100;

#[test]
fn a_hundred_processes_of_a_seed_give_one_trace() {
    // Separate processes: anything random outside the seeded generator (the hash order of a
    // `HashMap`, the address of an allocation, the clock) would change the trace between them.
    let traces: std::collections::BTreeSet<String> = (0..RUNS)
        .map(|_| {
            let out = std::process::Command::new(env!("CARGO_BIN_EXE_teseladb-sim"))
                .args(["--seed", "7", "--plan", "heavy", "--sim-time", "8s", "--workload", "all"])
                .output()
                .unwrap();
            assert_eq!(out.status.code(), Some(0));
            let text = String::from_utf8(out.stdout).unwrap();
            text.lines()
                .filter(|l| l.starts_with("ok "))
                .map(|l| l.split(" trace=").nth(1).unwrap().to_owned())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    assert_eq!(traces.len(), 1, "{traces:?}");
}
