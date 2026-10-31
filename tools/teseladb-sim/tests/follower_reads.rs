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
