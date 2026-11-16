//! A test clock whose time moves only when a test advances it.
//!
//! [`ManualClock`] reads a process-wide nanosecond counter, [`MANUAL_CLOCK_NOW`], that starts at
//! zero. Tests move it forward with `ManualClock::advance`, which also expires the manual-clock
//! timers of every shard; `ManualClock::expire_timers` runs the expiry on the current shard only.
//! Both are defined next to the reactor (in `crate::engine::core::reactor`) because they need
//! the reactor's timer lists. Timers, sleeps and token buckets can be instantiated on this clock
//! to make time-dependent code deterministic.

use std::sync::atomic::{AtomicI64, Ordering};

use crate::engine::core::lowres_clock::{TimePoint, impl_nanos_clock};

/// A clock advanced only by `ManualClock::advance`, for tests.
#[derive(Debug, Default)]
pub struct ManualClock {
    _private: (),
}

/// A point in time of [`ManualClock`].
pub type ManualTimePoint = TimePoint<ManualClock>;

/// The current manual time, in nanoseconds since the clock's epoch, shared by all shards.
pub static MANUAL_CLOCK_NOW: AtomicI64 = AtomicI64::new(0);

impl ManualClock {
    /// The clock is not steady: tests may set it arbitrarily.
    pub const IS_STEADY: bool = false;

    /// Returns the current manual time. One relaxed atomic load.
    #[inline(always)]
    pub fn now() -> ManualTimePoint {
        TimePoint::from_nanos(MANUAL_CLOCK_NOW.load(Ordering::Relaxed))
    }
}

impl_nanos_clock!(ManualClock);

/// Lets a shared token bucket run on manual time: the bucket reads time points as nanoseconds
/// since the clock's epoch.
impl crate::engine::util::shared_token_bucket::internal::TokenBucketClock for ManualClock {
    #[inline]
    fn now() -> i64 {
        MANUAL_CLOCK_NOW.load(Ordering::Relaxed)
    }
}
