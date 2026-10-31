//! `multirange` (007 T016, FR-023, SC-006): the seeds that found each defect of splits and merges
//! pass.
//!
//! - Default 7, 77 and 99: a replica that waited for a snapshot (it never applied its parent's
//!   cut) led the new range and served what an older life of the range left on its disk.
//! - Default 166: a left replica that got a merge in a snapshot could not install it while the
//!   right replica, absorbed elsewhere, still held its keys.
//! - Default 187: a snapshot past a split waited for a memtable flush that no write brought.
//! - Default 192: a restart replayed writes of keys a split had given away, flushed already.
//! - Heavy 152: a replica whose snapshot data was being cleared was taken as ready for a merge.
//! - Heavy 751: a write proposed in a range that a merge removed was answered as never applied.
//! - Heavy 695: a replica whose source crashed never asked another one.
//! - Heavy 865: a replica waiting for a snapshot no replica serves voted for no one.
//! - Heavy 408 and 329: a new leader proposed a write and a second split past a cut it had
//!   applied before its host followed it.
//! - Heavy 981: a merge started while a node was down kept the clients' writes waiting.
//! - Default 753 and 643, heavy 87 and 447: a range born from a split whose other born replica
//!   could not commit without the one that never applied the cut.
//! - Default 1388: a snapshot cleared a merged range's keys under the writes of the range that
//!   absorbed it.
//! - Default 1575, heavy 439 and 4046: the replica of a range frozen for a merge waited for its
//!   snapshot's data until the merge ended, and a merge the left range gave up ends with a thaw
//!   that needed that replica.
//! - Heavy 1119, 4358 and 4735: a replica restarted without its own snapshot's data, which no
//!   replica served any more, voted for no one, and with a node lost its range never elected;
//!   then the new leader's snapshot past its floor waited for an idle flush that the workload,
//!   unlike the server, did not run again after the restart.
//! - Heavy 1580 and 1799: a range born from a split whose leader never compacted its log had no
//!   snapshot for the replica that never applied the cut, and with a node lost it never committed.
//! - Heavy 2076: that replica then took the born range's snapshot, which had no write of its own,
//!   as if its data were already here, and served what an older replica of the parent had left.

use teseladb_sim::faults::FaultPlan;
use teseladb_sim::{SimConfig, Verdict, run_seed};

fn cfg(faults: FaultPlan) -> SimConfig {
    SimConfig { workload: "multirange".into(), faults, ..SimConfig::default() }
}

#[test]
fn the_seeds_that_found_defects_pass() {
    let seeds = [
        (FaultPlan::Default, 7),
        (FaultPlan::Default, 77),
        (FaultPlan::Default, 99),
        (FaultPlan::Default, 166),
        (FaultPlan::Default, 187),
        (FaultPlan::Default, 192),
        (FaultPlan::Default, 643),
        (FaultPlan::Default, 753),
        (FaultPlan::Default, 1388),
        (FaultPlan::Default, 1575),
        (FaultPlan::Heavy, 87),
        (FaultPlan::Heavy, 152),
        (FaultPlan::Heavy, 329),
        (FaultPlan::Heavy, 408),
        (FaultPlan::Heavy, 447),
        (FaultPlan::Heavy, 695),
        (FaultPlan::Heavy, 751),
        (FaultPlan::Heavy, 865),
        (FaultPlan::Heavy, 981),
        (FaultPlan::Heavy, 439),
        (FaultPlan::Heavy, 1119),
        (FaultPlan::Heavy, 4046),
        (FaultPlan::Heavy, 4358),
        (FaultPlan::Heavy, 4735),
        (FaultPlan::Heavy, 1580),
        (FaultPlan::Heavy, 1799),
        (FaultPlan::Heavy, 2076),
    ];
    for (plan, seed) in seeds {
        let r = run_seed(&cfg(plan), seed);
        assert_eq!(r.verdict, Verdict::Pass, "seed {seed} with {plan:?}");
        assert!(r.events > 10_000, "seed {seed}: {} events", r.events);
    }
}

#[test]
fn without_faults_every_operation_is_answered() {
    for seed in 1..=5 {
        let r = run_seed(&cfg(FaultPlan::None), seed);
        assert_eq!(r.verdict, Verdict::Pass, "seed {seed}");
    }
}
