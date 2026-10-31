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
