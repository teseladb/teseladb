//! Closed marks (007 FR-012, plan «Marcas cerradas», `spec/tla/ClosedTimestamp.tla`). The leader
//! of a range closes the mark [`CLOSED_LAG_MS`] behind its clock every [`CLOSE_EVERY`] while its
//! lease holds, and its heartbeats carry it with the last index of its log then; every
//! [`LOG_EVERY`] it also writes it in the log (`SetClosedTs`), so that a restarted replica learns
//! it again. Every write takes its mark from the clock as it enters the log, so none after the
//! closing goes at or below the mark. A follower that applied up to a closed mark's index reads at
//! that mark or below without the leader.

use teseladb_hlc::Hlc;
use teseladb_types::Nanos;

/// How far behind the leader's clock its closed mark goes.
pub const CLOSED_LAG_MS: u64 = 3_000;
/// How often a leader closes a mark for its heartbeats.
pub const CLOSE_EVERY: Nanos = Nanos::from_millis(200);
/// How often a leader closes a mark while change streams read its range (014 SC-002): the
/// resolved mark trails the clock by [`STREAM_CLOSE_LAG_MS`] plus at most this.
pub const STREAM_CLOSE_EVERY: Nanos = Nanos::from_millis(20);
/// How often a leader writes its closed mark in the log.
pub const LOG_EVERY: Nanos = Nanos::from_secs(5);

/// How far behind its clock a leader closes marks while change streams read its range (014
/// FR-004, `ClosedTimestamp.tla` with `Streams`).
pub const STREAM_CLOSE_LAG_MS: u64 = 200;

/// The mark a leader whose clock reads `mark` closes `lag_ms` behind it ([`CLOSED_LAG_MS`], or
/// [`STREAM_CLOSE_LAG_MS`] with streams); none while the clock is within the lag of its start.
pub fn closing(mark: Hlc, lag_ms: u64) -> Option<Hlc> {
    let physical = mark.physical.checked_sub(lag_ms).filter(|p| *p > 0)?;
    Some(Hlc { physical, logical: 0 })
}

/// The mark a replica that applied up to `applied` reads at, from the closed marks it knows
/// (`(mark, index)`, from the heartbeats and from its log): the client's mark `at` if a known mark
/// covers it, or the highest mark it can read at without one. `None`: the read goes to the leader.
pub fn follower_mark(known: &[Option<(Hlc, u64)>], applied: u64, at: Option<Hlc>) -> Option<Hlc> {
    let readable = known.iter().flatten().filter(|(_, index)| *index <= applied);
    let best = readable.map(|(mark, _)| *mark).max()?;
    match at {
        None => Some(best),
        Some(t) if t <= best => Some(t),
        Some(_) => None,
    }
}
