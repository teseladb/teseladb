//! A read that meets a version in its uncertainty window (008 FR-008): the version may have been
//! written before the read began on a node whose clock is ahead, so the read goes again above
//! it. The first statement of a transaction that has written nothing restarts transparently at a
//! new read mark; a later statement, or one after a write, fails with `WriteConflict` and
//! `TransientTransactionError`, and the driver repeats the transaction.

use teseladb_hlc::Hlc;

/// The window above a read mark taken from a clock: twice the clock offset the cluster
/// tolerates (`MAX_CLOCK_OFFSET_MS`, 125 ms).
pub const UNCERTAINTY_MS: u64 = 250;
/// Restarts of one statement before it gives up with `WriteConflict`.
pub const MAX_RESTARTS: u32 = 8;

/// The end of the uncertainty window of a read at `at`.
pub fn window_end(at: Hlc) -> Hlc {
    Hlc { physical: at.physical.saturating_add(UNCERTAINTY_MS), logical: u32::MAX }
}

/// What a statement does when one of its reads met a version at `ts` in its window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnUncertain {
    /// It runs again with the transaction reading at this mark.
    Restart(Hlc),
    /// It fails with `WriteConflict` and `TransientTransactionError`.
    Fail,
}

/// The rule for statement `statement` (0: the first) of a transaction that has `written` or not,
/// after `restarts` restarts of that statement.
pub fn on_uncertain(statement: u32, written: bool, restarts: u32, ts: Hlc) -> OnUncertain {
    if statement == 0 && !written && restarts < MAX_RESTARTS {
        OnUncertain::Restart(ts)
    } else {
        OnUncertain::Fail
    }
}
