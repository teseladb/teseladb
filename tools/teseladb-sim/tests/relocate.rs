//! `relocate` (007 T020, FR-011, FR-023): the seeds that found each defect of core changes pass.
//!
//! - Default 1013 and heavy 421: a replica parked its leader's snapshot while its core was frozen;
//!   the workload took the frozen core for a replica with nothing to wait for and forgot the wait,
//!   so the data never came, the replica voted for no one and, with a node lost, the range never
//!   elected again.

use teseladb_sim::faults::FaultPlan;
use teseladb_sim::{SimConfig, Verdict, run_seed};

fn cfg(faults: FaultPlan) -> SimConfig {
    SimConfig { workload: "relocate".into(), faults, ..SimConfig::default() }
}

#[test]
fn the_seeds_that_found_defects_pass() {
    for (plan, seed) in [(FaultPlan::Default, 1013), (FaultPlan::Heavy, 421)] {
        let r = run_seed(&cfg(plan), seed);
        assert_eq!(r.verdict, Verdict::Pass, "seed {seed} with {plan:?}");
        assert!(r.events > 10_000, "seed {seed}: {} events", r.events);
    }
}

#[test]
fn without_faults_every_operation_is_answered() {
    for seed in 1..=3 {
        let r = run_seed(&cfg(FaultPlan::None), seed);
        assert_eq!(r.verdict, Verdict::Pass, "seed {seed}");
    }
}
