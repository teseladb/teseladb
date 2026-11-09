//! `range-docs` (005 T016, FR-019, SC-001): the seeds that found each defect of the range pass.
//! Seed 22 (default plan) had two compare-and-swaps succeed from the same value; seeds 1080,
//! 1996 and 3032 (heavy plan) left the range without a leader (a replica waiting for data that
//! only a leader could export, an export whose end nobody was told, a snapshot install refused
//! by a stale release floor). Seed 124 (default) and seeds 383 and 1216 (heavy) left it without a
//! leader too: a replica restarted at its LSM's flushed index applied its log again, its floor
//! fell below what was durable and the leader's snapshot was refused, which stopped the group.
//! Seed 2422 (heavy) exported a snapshot twice into the same directory after its source
//! restarted, and the LSM stopped on the file already there.
//! Seed 4345 (heavy plan, twice the fault rate) left it without progress: a node restarted with
//! its LSM past the commit index it read back led again, could not commit without a follower that
//! needed a snapshot, and refused the snapshot it took during the replay as below its floor,
//! although that snapshot held the state past the floor. With the other changes of 007 T016 the
//! seed no longer reaches that state; `snapshot_floor.rs` of `teseladb-multiraft` checks it.

use std::cell::Cell;
use std::rc::Rc;

use teseladb_sim::faults::FaultPlan;
use teseladb_sim::workloads::range_docs::{RangeDocsWorkload, RetryStats};
use teseladb_sim::{SimConfig, Verdict, run_seed, run_workload};

fn cfg(faults: FaultPlan) -> SimConfig {
    SimConfig { workload: "range-docs".into(), faults, ..SimConfig::default() }
}

/// Twice the default fault rate.
const DOUBLE_FAULTS: u32 = 200;

#[test]
fn a_leader_restarted_past_its_commit_index_still_makes_progress() {
    let cfg = SimConfig { fault_rate_pct: DOUBLE_FAULTS, ..cfg(FaultPlan::Heavy) };
    let r = run_seed(&cfg, 4345);
    assert_eq!(r.verdict, Verdict::Pass);
    assert!(r.events > 10_000, "{} events", r.events);
}

#[test]
fn the_seeds_that_found_defects_pass() {
    for (plan, seed) in [
        (FaultPlan::Default, 22),
        (FaultPlan::Heavy, 1080),
        (FaultPlan::Heavy, 1996),
        (FaultPlan::Heavy, 3032),
        (FaultPlan::Default, 124),
        (FaultPlan::Heavy, 383),
        (FaultPlan::Heavy, 1216),
        (FaultPlan::Heavy, 2422),
    ] {
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

/// A retried write without an expected value that a stale leader acknowledges with `w: 1` found
/// the value of that leader's state, which a rollback takes back, while its original may have been
/// applied by the majority over another value: the history cannot name its predecessor. Seed 547
/// (default plan, twice the fault rate, before 007 T023 moved its schedule) recorded the value the
/// stale leader found, and the durability checker reported two writes acknowledged by the majority
/// as lost although the original had been applied over the newest of them.
#[test]
fn a_retried_write_acknowledged_with_w1_names_no_predecessor() {
    // The case comes in about one seed of fifty (measured on 09-11-2026, seeds 1 to 400): the seeds
    // run until it has come `WANTED` times, so that a change of the runs' order of events does not
    // leave it untested.
    const WANTED: u64 = 3;
    const MAX_SEEDS: u64 = 1_000;
    let mut total = RetryStats::default();
    let mut seed = 0;
    while total.w1_blind < WANTED && seed < MAX_SEEDS {
        seed += 1;
        let stats = Rc::new(Cell::new(RetryStats::default()));
        let wl = RangeDocsWorkload::counting(3, 2, Rc::clone(&stats));
        let cfg = SimConfig { fault_rate_pct: DOUBLE_FAULTS, ..cfg(FaultPlan::Heavy) };
        let r = run_workload(&cfg, seed, wl);
        assert_eq!(r.verdict, Verdict::Pass, "seed {seed}");
        total.w1_blind += stats.get().w1_blind;
        total.w1_blind_named += stats.get().w1_blind_named;
    }
    assert!(
        total.w1_blind >= WANTED,
        "only {} retried writes acknowledged with w: 1 in {seed} seeds: {total:?}",
        total.w1_blind
    );
    assert_eq!(total.w1_blind_named, 0, "{total:?}");
}

/// FR-011: writes whose answer is lost go again with the same key; some retries get a definite
/// answer, and some of those come from the record of an original that was applied, while every
/// checker still passes (a write applied twice would break the history).
#[test]
fn lost_writes_are_retried_with_their_key() {
    let mut total = RetryStats::default();
    for seed in 1..=6 {
        let stats = Rc::new(Cell::new(RetryStats::default()));
        let wl = RangeDocsWorkload::counting(3, 2, Rc::clone(&stats));
        let r = run_workload(&cfg(FaultPlan::Heavy), seed, wl);
        assert_eq!(r.verdict, Verdict::Pass, "seed {seed}");
        let s = stats.get();
        // A retry still waiting for its turn when the run ended was never sent.
        let ordered = s.from_record <= s.definite
            && s.definite <= s.retried
            && s.retried <= s.sent + s.unsent;
        assert!(ordered, "seed {seed}: {s:?}");
        total.retried += s.retried;
        total.sent += s.sent;
        total.definite += s.definite;
        total.from_record += s.from_record;
    }
    assert!(total.retried > 0 && total.definite > 0 && total.from_record > 0, "{total:?}");
}

/// Seeds chosen by coverage (01-11-2026): each takes the workload through a guard of the
/// harness (a dead, frozen or restarted replica, an export or ingestion that has to wait or start
/// again) that no other seed of these tests reaches.
#[test]
fn the_seeds_that_reach_the_harness_rare_paths_pass() {
    let seeds = [(FaultPlan::Heavy, 166)];
    for (plan, seed) in seeds {
        let r = run_seed(&cfg(plan), seed);
        assert_eq!(r.verdict, Verdict::Pass, "seed {seed} with {plan:?}");
    }
}
