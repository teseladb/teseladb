//! The upper bound persisted in `hlc.bound` (FR-003, research R2, D-158): no mark goes out at or
//! above the bound on disk (the clock keeps it, `HlcClock::with_bound`), the bound moves one
//! second ahead half a second before it is reached (so writing it does not hold marks back), a
//! node that stops cleanly writes the exact bound ([`crate::HlcClock::seal`]), and after a
//! restart the node waits until its physical clock passes the bound, so a clock that went back
//! cannot repeat marks.

/// How far ahead of the current mark the bound is written.
pub const AHEAD_MS: u64 = 1_000;
/// How close to the bound a new one is written.
pub const RENEW_MS: u64 = 500;
/// How far ahead of the node's wall clock a received `$clusterTime` may be (DV-101): it is
/// taken into the clock, and the bound follows it.
pub const MAX_RECEIVED_AHEAD_MS: u64 = 1_000;

/// After a restart with the bound `bound` on disk: the physical time to wait for, if the clock at
/// `physical` has not passed it.
pub fn wait_on_start(bound: u64, physical: u64) -> Option<u64> {
    (physical <= bound).then_some(bound + 1)
}
