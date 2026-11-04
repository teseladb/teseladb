//! The `indexes` workload (009 T003, FR-017): documents and their index entries in two ranges,
//! every write a transaction across them, checked by reading everything at one mark.

use teseladb_sim::faults::FaultPlan;
use teseladb_sim::workloads::indexes::{Indexes, entry_keys};
use teseladb_sim::{SimConfig, Verdict, run_workload};

fn cfg() -> SimConfig {
    SimConfig { workload: "indexes".into(), faults: FaultPlan::Default, ..SimConfig::default() }
}

#[test]
fn writes_across_ranges_keep_every_index_consistent() {
    for seed in 1..=5 {
        let r = run_workload(&cfg(), seed, Indexes::boxed(3, 2));
        assert_eq!(r.verdict, Verdict::Pass, "indexes seed {seed}");
    }
}

/// Seeds whose faults left no check committed (1083, 2513): once they stop, every turn checks
/// until one commits.
#[test]
fn every_seed_ends_checked() {
    for seed in [1083, 2513] {
        let r = run_workload(&cfg(), seed, Indexes::boxed(3, 2));
        assert_eq!(r.verdict, Verdict::Pass, "indexes seed {seed}");
    }
}

#[test]
fn every_entry_a_document_can_have_is_read_by_a_check() {
    // Each index of each document, over the values the workload writes.
    let keys = entry_keys();
    assert!(keys.len() > 4 * 5, "{} keys", keys.len());
    assert!(keys.iter().all(|k| k.as_slice() >= &[0x01, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 1][..]));
}

/// Seed 883 of the `heavy` plan: a node restarted from its own snapshot, 38 entries behind its
/// LSM, wrote again the intent of a transaction that had deleted a TTL index entry and stopped
/// before the resolution came again. Once it led the range, the intent hid the deletion and a
/// check read an entry no document needed.
#[test]
fn a_restart_from_an_older_snapshot_leaves_no_orphan_entry() {
    let cfg = SimConfig { faults: FaultPlan::Heavy, ..cfg() };
    let r = teseladb_sim::run_seed(&cfg, 883);
    assert_eq!(r.verdict, Verdict::Pass, "indexes seed 883 with the heavy plan");
}
