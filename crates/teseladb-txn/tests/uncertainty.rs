//! The restart rule of a read that meets a version in its uncertainty window (008 FR-008, T023).

use teseladb_hlc::Hlc;
use teseladb_txn::uncertainty::{
    MAX_RESTARTS, OnUncertain, UNCERTAINTY_MS, on_uncertain, window_end,
};

fn ts(p: u64) -> Hlc {
    Hlc { physical: p, logical: 0 }
}

#[test]
fn the_first_statement_restarts_and_the_others_fail() {
    assert_eq!(on_uncertain(0, false, 0, ts(7)), OnUncertain::Restart(ts(7)));
    assert_eq!(on_uncertain(0, false, MAX_RESTARTS - 1, ts(7)), OnUncertain::Restart(ts(7)));
    // Past its restarts, after a write, or in a later statement: `WriteConflict`.
    assert_eq!(on_uncertain(0, false, MAX_RESTARTS, ts(7)), OnUncertain::Fail);
    assert_eq!(on_uncertain(0, true, 0, ts(7)), OnUncertain::Fail);
    assert_eq!(on_uncertain(1, false, 0, ts(7)), OnUncertain::Fail);
}

#[test]
fn the_window_ends_its_length_above_the_mark() {
    let end = window_end(Hlc { physical: 1_000, logical: 5 });
    assert_eq!(end, Hlc { physical: 1_000 + UNCERTAINTY_MS, logical: u32::MAX });
    assert!(end > Hlc { physical: 1_000 + UNCERTAINTY_MS, logical: 7 });
    assert_eq!(window_end(ts(u64::MAX)).physical, u64::MAX);
}
