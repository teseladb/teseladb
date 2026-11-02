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
