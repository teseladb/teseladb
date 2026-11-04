//! Seeds of the final phase's integrity run (`--workload all --plan heavy --fault-rate 200
//! --seeds 1000`) that failed on main 175e112, each with its cause:
//!
//! - `multirange` 313, `splits` 712 and `relocate` 999: a node whose wall clock jumped ahead kept
//!   leading a range while cut off and raised the cluster time to its wall clock (samples of its
//!   peers from before the jump still vouched for it, and a leader carried its wall clock into
//!   the clock whether or not anything vouched for it). After the heal a node that restarted
//!   waited for its wall clock to pass a bound seconds ahead, and with a third node lost no write
//!   was answered (005 FR-003, FR-006).
//! - `changestream` 160 and 683, `search` 381: a merge froze a range and was then given up; the
//!   statements its frozen range refused waited for good for the route to change, so no
//!   transaction committed after the heal (008).
//! - `changestream` 87 and 304: cursors that could not read until the heal caught up one second
//!   of history per read and never reached the transactions committed after it; a range's
//!   answer is now bounded by bytes instead of a window of time (014 FR-003, T027).

use teseladb_sim::faults::FaultPlan;
use teseladb_sim::{SimConfig, Verdict, run_seed};

/// Twice the plan's rhythm of faults.
const DOUBLE_FAULTS: u32 = 200;

#[test]
fn the_seeds_of_the_integrity_run_pass() {
    let seeds = [
        ("multirange", 313),
        ("splits", 712),
        ("relocate", 999),
        ("changestream", 87),
        ("changestream", 304),
        ("changestream", 160),
        ("changestream", 683),
        ("search", 381),
    ];
    for (workload, seed) in seeds {
        let cfg = SimConfig {
            workload: workload.into(),
            faults: FaultPlan::Heavy,
            fault_rate_pct: DOUBLE_FAULTS,
            ..SimConfig::default()
        };
        assert_eq!(run_seed(&cfg, seed).verdict, Verdict::Pass, "{workload} {seed}");
    }
}
