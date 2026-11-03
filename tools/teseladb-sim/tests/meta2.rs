//! `meta2` (007 T028, FR-014, FR-023): the descriptors go from Group 0 to two Meta2 ranges in the
//! middle of the fault window and no acknowledged change is lost. A pass under a fault plan means
//! the end state was checked after the heal: the handoff happened, both Meta2 ranges applied
//! their seed, Group 0 dropped both intervals and each holder has every acknowledged term.

use teseladb_sim::faults::FaultPlan;
use teseladb_sim::{SimConfig, Verdict, run_seed};

fn cfg(faults: FaultPlan) -> SimConfig {
    SimConfig { workload: "meta2".into(), faults, ..SimConfig::default() }
}

#[test]
fn the_handoff_loses_no_change_under_faults() {
    for plan in [FaultPlan::Default, FaultPlan::Heavy] {
        for seed in 1..=6 {
            let r = run_seed(&cfg(plan), seed);
            assert_eq!(r.verdict, Verdict::Pass, "seed {seed} with {plan:?}");
            assert!(r.events > 10_000, "seed {seed}: {} events", r.events);
        }
    }
}

#[test]
fn without_faults_it_passes() {
    for seed in 1..=3 {
        assert_eq!(run_seed(&cfg(FaultPlan::None), seed).verdict, Verdict::Pass, "seed {seed}");
    }
}

/// Seeds 247 and 798 of the `light` plan: during a partition the cut-off leader of both Meta2
/// ranges took one change from every client, and after the heal those changes waited for their
/// deadline (5 s) instead of ending when the new leader's first entry was committed, so no change
/// was answered within 4.5 s of the heal.
#[test]
fn changes_a_deposed_leader_took_end_soon_after_the_heal() {
    for seed in [247, 798] {
        let r = run_seed(&cfg(FaultPlan::Light), seed);
        assert_eq!(r.verdict, Verdict::Pass, "seed {seed}");
    }
}
