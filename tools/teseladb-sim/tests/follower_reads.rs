//! `follower-reads` (007 T025, FR-012, SC-006): reads on followers at closed marks pass the mvcc
//! rules with faults and without them, and some of them are served in every run.

use teseladb_sim::faults::FaultPlan;
use teseladb_sim::{SimConfig, Verdict, run_seed};

fn cfg(faults: FaultPlan) -> SimConfig {
    SimConfig { workload: "follower-reads".into(), faults, ..SimConfig::default() }
}

#[test]
fn reads_on_followers_pass_with_and_without_faults() {
    for (plan, seed) in [(FaultPlan::None, 1), (FaultPlan::None, 2), (FaultPlan::Default, 1)] {
        let r = run_seed(&cfg(plan), seed);
        assert_eq!(r.verdict, Verdict::Pass, "seed {seed}");
    }
}

/// Seeds chosen by coverage (01-11-2026): each takes the workload through a guard of the
/// harness (a dead, frozen or restarted replica, an export or ingestion that has to wait or start
/// again) that no other seed of these tests reaches.
#[test]
fn the_seeds_that_reach_the_harness_rare_paths_pass() {
    let seeds = [(FaultPlan::Default, 209)];
    for (plan, seed) in seeds {
        let r = run_seed(&cfg(plan), seed);
        assert_eq!(r.verdict, Verdict::Pass, "seed {seed} with {plan:?}");
    }
}
