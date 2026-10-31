//! `splits` (007 T016, FR-023, SC-006): the seeds that found each defect of splits with
//! transactions pass.
//!
//! - Default 4, 14, 18, 20 and 27: a replica that got its range as a snapshot over old data of
//!   its own cleared the interval, but the compaction kept the old intents under the clearing's
//!   tombstone; the ingestion found them and the replica cleared and ingested again forever, so
//!   its range never had a quorum after a node was lost. Default 32: the files of that loop grew
//!   until one read failed as corrupt.
//! - Heavy 1401, 2050 and 4277 (and 313, 1910, 2141, 3423, 3656): the workload's transactions
//!   took their marks from a random node's clock, which the plan had moved seconds ahead (from its
//!   wall clock, and then from a node cut off from its peers, whose clock rule hears nobody to
//!   compare with); the ranges they read carried their HLC and its bound on disk there, and a node
//!   restarted after the heal waited past the end of the run for its clock to pass the bound. A
//!   transaction takes its mark from the leader of the first range it touches, as a client takes
//!   the cluster time from the server it asks, under the clock rule (005 FR-006).
//! - Heavy 229: a crash cut a manifest edit's page after its records; the next edit, a page further,
//!   left zeros in that padding, and the next recovery took the edit for a broken one in the middle
//!   of the manifest. The padding carries nothing: zeros at its end are padding too.
//! - Heavy 17: a node restarted between a clearing and its snapshot; the clearing's tombstone,
//!   over the range's bounds before a split, outlived it (the clearings are not on disk) and no
//!   compaction removed it, so every later snapshot over part of it overlapped and cleared again.
//! - Heavy 1827: a replica restarted while the leader sent it a snapshot, and asked for one; the
//!   leader dropped the lost transfer's timeout on that ask while Raft still counted the transfer in
//!   flight and answered the ask with nothing, so no snapshot went to the replica again.
//! - Heavy 4086: each election raised the node's clock to its own reading plus the read cache's
//!   margin, so elections in a row pushed the HLC seconds past the wall clock, with its bound on
//!   disk; the nodes restarted after the heal waited past the end of the run (005 R12).

use teseladb_sim::faults::FaultPlan;
use teseladb_sim::{SimConfig, Verdict, run_seed};

fn cfg(faults: FaultPlan) -> SimConfig {
    SimConfig { workload: "splits".into(), faults, ..SimConfig::default() }
}

#[test]
fn the_seeds_that_found_defects_pass() {
    let seeds = [
        (FaultPlan::Default, 4),
        (FaultPlan::Default, 14),
        (FaultPlan::Default, 18),
        (FaultPlan::Default, 20),
        (FaultPlan::Default, 27),
        (FaultPlan::Default, 32),
        (FaultPlan::Heavy, 17),
        (FaultPlan::Heavy, 229),
        (FaultPlan::Heavy, 1401),
        (FaultPlan::Heavy, 2050),
        (FaultPlan::Heavy, 4277),
        (FaultPlan::Heavy, 313),
        (FaultPlan::Heavy, 1910),
        (FaultPlan::Heavy, 2141),
        (FaultPlan::Heavy, 3423),
        (FaultPlan::Heavy, 3656),
        (FaultPlan::Heavy, 1827),
        (FaultPlan::Heavy, 4086),
    ];
    for (plan, seed) in seeds {
        let r = run_seed(&cfg(plan), seed);
        assert_eq!(r.verdict, Verdict::Pass, "seed {seed}");
        assert!(r.events > 10_000, "seed {seed}: {} events", r.events);
    }
}

#[test]
fn without_faults_every_transaction_and_operation_is_answered() {
    for seed in 1..=3 {
        let r = run_seed(&cfg(FaultPlan::None), seed);
        assert_eq!(r.verdict, Verdict::Pass, "seed {seed}");
    }
}
