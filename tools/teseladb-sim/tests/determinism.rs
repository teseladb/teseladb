//! Same seed → same trace (and a different one with another seed), also with faults and
//! with collections whose hash order is random per process (002 FR-006, SC-001).

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

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

/// The arguments of the runs: every workload, seed 7, heavy faults.
const ALL: [&str; 8] = ["--seed", "7", "--plan", "heavy", "--sim-time", "8s", "--workload", "all"];

/// The traces of `runs` separate processes of the simulator at `bin` with `args`, each one the
/// traces of its workloads in order; `between` runs before each process. The processes run a
/// copy of `bin` taken first, in a directory of this test's own named by `label`: a build in the
/// same target directory may replace `bin` while they run, and the runs would then compare two
/// simulators instead of one.
fn traces(
    bin: &Path,
    args: &[&str],
    runs: usize,
    label: &str,
    mut between: impl FnMut(usize),
) -> BTreeSet<String> {
    let dir = std::env::temp_dir().join(format!("teseladb-sim-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pinned = dir.join("teseladb-sim");
    std::fs::copy(bin, &pinned).unwrap();
    let traces = (0..runs)
        .map(|i| {
            between(i);
            let out = Command::new(&pinned).args(args).output().unwrap();
            assert_eq!(out.status.code(), Some(0));
            let text = String::from_utf8(out.stdout).unwrap();
            text.lines()
                .filter(|l| l.starts_with("ok "))
                .map(|l| l.split(" trace=").nth(1).unwrap().to_owned())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    std::fs::remove_dir_all(&dir).unwrap();
    traces
}

#[test]
fn a_hundred_processes_of_a_seed_give_one_trace() {
    // Separate processes: anything random outside the seeded generator (the hash order of a
    // `HashMap`, the address of an allocation, the clock) would change the trace between them.
    let bin = Path::new(env!("CARGO_BIN_EXE_teseladb-sim"));
    let traces = traces(bin, &ALL, RUNS, "hundred", |_| {});
    assert_eq!(traces.len(), 1, "{traces:?}");
}

#[test]
fn a_simulator_rebuilt_during_the_runs_does_not_change_them() {
    // A build in the same target directory replaces the binary while the runs above go on: they
    // once gave three traces, which differed only in the three workloads that the change being
    // written in that tree touched.
    let dir = std::env::temp_dir().join(format!("teseladb-sim-replaced-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("teseladb-sim");
    std::fs::copy(env!("CARGO_BIN_EXE_teseladb-sim"), &bin).unwrap();
    let args = ["--seed", "7", "--sim-time", "500ms", "--workload", "testkv"];
    let got = traces(&bin, &args, 3, "rebuilt", |i| {
        if i == 1 {
            // Another program in its place, written beside it and renamed as a build does: it
            // exits with 0 and prints no trace.
            let next = dir.join("next");
            std::fs::copy("/usr/bin/true", &next).unwrap();
            std::fs::rename(&next, &bin).unwrap();
        }
    });
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(!got.first().unwrap().is_empty(), "the runs are the simulator's");
}
