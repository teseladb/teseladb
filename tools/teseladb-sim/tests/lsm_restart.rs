//! `lsm` (004): a node whose disk fails while its restart checks the LSM stops again, and the
//! supervisor restarts it; that is the fail-stop of the core, not a defect of the LSM. Seed 509
//! (heavy plan) had a `SyncError` take the LSM's sync while the restart read it back.

use teseladb_sim::faults::FaultPlan;
use teseladb_sim::{SimConfig, Verdict, run_seed};

#[test]
fn a_disk_error_during_the_restart_check_restarts_the_node() {
    let cfg =
        SimConfig { workload: "lsm".into(), faults: FaultPlan::Heavy, ..SimConfig::default() };
    let r = run_seed(&cfg, 509);
    assert_eq!(r.verdict, Verdict::Pass);
}
