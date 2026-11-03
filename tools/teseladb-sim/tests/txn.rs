//! The transaction workloads (008 T004, FR-016): seeds that found defects.

use teseladb_sim::faults::FaultPlan;
use teseladb_sim::workloads::txn_append::Append;
use teseladb_sim::workloads::txn_bank::Bank;
use teseladb_sim::{SimConfig, Verdict, run_workload};

fn cfg(workload: &str) -> SimConfig {
    SimConfig { workload: workload.into(), faults: FaultPlan::Default, ..SimConfig::default() }
}

/// A node that is up again before its host reopens was taken for a range's leader, and the
/// driver sent it a request without a host (it aborted on `txn-append` 3 and `txn-bank` 2).
#[test]
fn a_restarted_node_without_its_host_is_not_a_leader() {
    for seed in 1..=3 {
        let r = run_workload(&cfg("txn-append"), seed, Append::boxed(3, 2));
        assert_eq!(r.verdict, Verdict::Pass, "txn-append seed {seed}");
    }
}

/// Committed reads of every account that did not add up to the total: reads at a mark ahead of
/// the leader's clock (`txn-bank` 2, 15, 29) and a read that found no leader and was skipped by
/// the script, whose transfer then wrote from the opening balance (`txn-bank` 21).
#[test]
fn every_committed_read_of_all_accounts_adds_up() {
    for seed in [2, 15, 21, 29] {
        let r = run_workload(&cfg("txn-bank"), seed, Bank::boxed(3, 2));
        assert_eq!(r.verdict, Verdict::Pass, "txn-bank seed {seed}");
    }
}

/// Lists that started again: an append whose read found no leader went on without it
/// (`txn-append` 76, 350).
#[test]
fn an_append_never_goes_on_without_its_read() {
    for seed in [76, 350] {
        let r = run_workload(&cfg("txn-append"), seed, Append::boxed(3, 2));
        assert_eq!(r.verdict, Verdict::Pass, "txn-append seed {seed}");
    }
}

/// A record that a writer's push created before its coordinator's `Begin` landed has no session;
/// the `Begin` timed out, the coordinator aborted, and record GC removed the aborted record by the
/// rule of a transaction without a session, five seconds after its intent's mark, while the
/// client's session had neither a later transaction nor its time over (`txn-bank` 996, `heavy`).
#[test]
fn a_record_created_by_a_push_lives_as_long_as_its_session() {
    let cfg = SimConfig { faults: FaultPlan::Heavy, ..cfg("txn-bank") };
    let r = run_workload(&cfg, 996, Bank::boxed(3, 2));
    assert_eq!(r.verdict, Verdict::Pass, "txn-bank heavy seed 996");
}
