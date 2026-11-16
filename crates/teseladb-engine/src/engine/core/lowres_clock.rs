//! Cheap, low-resolution clocks refreshed by the reactor, and the nanosecond [`TimePoint`].
//!
//! [`LowresClock`] (steady) and [`LowresSystemClock`] (wall time) read thread-local values that
//! the reactor refreshes with [`LowresClock::update`] as it polls, so `now()` is a single
//! thread-local load instead of a system call; their granularity is roughly the task quota.
//! Outside a running reactor the values are stale (zero until the first update).
//!
//! [`TimePoint<C>`] is the time point type shared by these clocks and the manual test clock:
//! signed nanoseconds since the clock's epoch, tagged with the clock so that points of
//! different clocks cannot be mixed. `impl_nanos_clock!` implements the timer [`Clock`] trait
//! for such a clock.

use std::cell::Cell;
use std::marker::PhantomData;

/// The clock trait of the timers, re-exported where the clocks are defined.
pub use crate::engine::core::timer::Clock;

/// Implements [`Clock`] for a clock whose time points are [`TimePoint<Self>`] and
/// whose `now()` is an inherent function.
macro_rules! impl_nanos_clock {
    ($clock:ty) => {
        impl $crate::engine::core::timer::Clock for $clock {
            type TimePoint = $crate::engine::core::lowres_clock::TimePoint<$clock>;

            #[inline(always)]
            fn now() -> Self::TimePoint {
                <$clock>::now()
            }

            #[inline(always)]
            fn to_rep(tp: Self::TimePoint) -> i64 {
                tp.nanos_since_epoch()
            }

            #[inline(always)]
            fn from_rep(rep: i64) -> Self::TimePoint {
                $crate::engine::core::lowres_clock::TimePoint::from_nanos(rep)
            }

            #[inline(always)]
            fn add(tp: Self::TimePoint, d: std::time::Duration) -> Self::TimePoint {
                tp + d
            }

            #[inline(always)]
            fn max_time_point() -> Self::TimePoint {
                $crate::engine::core::lowres_clock::TimePoint::max()
            }
        }
    };
}
pub(crate) use impl_nanos_clock;

/// A point in time of clock `C`: signed nanoseconds since the clock's epoch.
#[repr(transparent)]
pub struct TimePoint<C> {
    ns: i64,
    _clock: PhantomData<fn() -> C>,
}

impl<C> TimePoint<C> {
    /// The time point `ns` nanoseconds after the epoch.
    #[inline(always)]
    pub const fn from_nanos(ns: i64) -> Self {
        Self { ns, _clock: PhantomData }
    }

    /// Returns the nanoseconds since the epoch.
    #[inline(always)]
    pub const fn nanos_since_epoch(self) -> i64 {
        self.ns
    }

    /// The latest representable time point.
    #[inline(always)]
    pub const fn max() -> Self {
        Self::from_nanos(i64::MAX)
    }

    /// The earliest representable time point.
    #[inline(always)]
    pub const fn min() -> Self {
        Self::from_nanos(i64::MIN)
    }

    /// Returns the time since the epoch as a `Duration` (negative values saturate to zero).
    #[inline]
    pub fn time_since_epoch(self) -> std::time::Duration {
        std::time::Duration::from_nanos(self.ns.max(0) as u64)
    }
}

impl<C> Clone for TimePoint<C> {
    #[inline(always)]
    fn clone(&self) -> Self {
        *self
    }
}

impl<C> Copy for TimePoint<C> {}

impl<C> PartialEq for TimePoint<C> {
    #[inline(always)]
    fn eq(&self, o: &Self) -> bool {
        self.ns == o.ns
    }
}

impl<C> Eq for TimePoint<C> {}

impl<C> PartialOrd for TimePoint<C> {
    #[inline(always)]
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}

impl<C> Ord for TimePoint<C> {
    #[inline(always)]
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.ns.cmp(&o.ns)
    }
}

impl<C> std::hash::Hash for TimePoint<C> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.ns.hash(state);
    }
}

impl<C> std::fmt::Debug for TimePoint<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}ns", self.ns)
    }
}

impl<C> std::ops::Add<std::time::Duration> for TimePoint<C> {
    type Output = Self;

    #[inline(always)]
    fn add(self, d: std::time::Duration) -> Self {
        Self::from_nanos(self.ns.saturating_add(d.as_nanos().min(i64::MAX as u128) as i64))
    }
}

impl<C> std::ops::Sub<std::time::Duration> for TimePoint<C> {
    type Output = Self;

    #[inline(always)]
    fn sub(self, d: std::time::Duration) -> Self {
        Self::from_nanos(self.ns.saturating_sub(d.as_nanos().min(i64::MAX as u128) as i64))
    }
}

impl<C> std::ops::Sub for TimePoint<C> {
    /// The signed difference in nanoseconds.
    type Output = i64;

    #[inline(always)]
    fn sub(self, o: Self) -> i64 {
        self.ns.wrapping_sub(o.ns)
    }
}

/// Low-resolution and efficient steady clock.
///
/// This is a monotonic clock with a granularity of about one task quota. Its time points do not
/// correspond to system time.
///
/// The primary benefit of this clock is that [`LowresClock::now`] is a thread-local load,
/// much cheaper than reading the steady clock of the operating system.
///
/// See [`LowresSystemClock`] for a low-resolution clock whose time points correspond to system
/// time.
#[derive(Debug, Clone, Copy, Default)]
pub struct LowresClock;

/// A point in time of [`LowresClock`].
pub type LowresTimePoint = TimePoint<LowresClock>;

thread_local! {
    // Const-initialized so that reads need no lazy-initialization guard.
    static LOWRES_NOW: Cell<i64> = const { Cell::new(0) };
    static LOWRES_SYSTEM_NOW: Cell<i64> = const { Cell::new(0) };
}

#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: std::ffi::c_long,
}

unsafe extern "C" {
    fn clock_gettime(clk: std::ffi::c_int, tp: *mut Timespec) -> std::ffi::c_int;
}

#[cfg(target_os = "linux")]
const CLOCK_MONOTONIC: std::ffi::c_int = 1;
#[cfg(target_os = "macos")]
const CLOCK_MONOTONIC: std::ffi::c_int = 6;
const CLOCK_REALTIME: std::ffi::c_int = 0;

#[inline]
fn clock_ns(clk: std::ffi::c_int) -> i64 {
    let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid out parameter.
    unsafe { clock_gettime(clk, &mut ts) };
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

/// Reads the monotonic clock (`CLOCK_MONOTONIC`) of the operating system, in nanoseconds.
#[inline]
pub fn steady_clock_now_ns() -> i64 {
    clock_ns(CLOCK_MONOTONIC)
}

/// Reads the wall clock (`CLOCK_REALTIME`) of the operating system, in nanoseconds since the
/// Unix epoch.
#[inline]
pub fn system_clock_now_ns() -> i64 {
    clock_ns(CLOCK_REALTIME)
}

impl LowresClock {
    /// The clock is steady: it never goes backwards.
    pub const IS_STEADY: bool = true;

    /// Returns the steady time stored by the last [`LowresClock::update`] on this thread.
    ///
    /// Outside a running reactor the result is meaningless (zero until the first update).
    #[inline(always)]
    pub fn now() -> LowresTimePoint {
        TimePoint::from_nanos(LOWRES_NOW.with(|n| n.get()))
    }

    /// Refreshes both low-resolution clocks of this thread.
    #[inline]
    pub fn update() {
        LOWRES_NOW.with(|n| n.set(steady_clock_now_ns()));
        LOWRES_SYSTEM_NOW.with(|n| n.set(system_clock_now_ns()));
    }
}

impl_nanos_clock!(LowresClock);

/// Low-resolution and efficient system clock.
///
/// This clock has the same granularity as [`LowresClock`], but it is not monotonic and its time
/// points correspond to system time.
///
/// The primary benefit of this clock is that [`LowresSystemClock::now`] is a thread-local load,
/// much cheaper than reading the wall clock of the operating system.
#[derive(Debug, Clone, Copy, Default)]
pub struct LowresSystemClock;

/// A point in time of [`LowresSystemClock`].
pub type LowresSystemTimePoint = TimePoint<LowresSystemClock>;

impl LowresSystemClock {
    /// The clock is not steady: wall time may jump.
    pub const IS_STEADY: bool = false;

    /// Returns the system time stored by the last [`LowresClock::update`] on this thread.
    ///
    /// Outside a running reactor the result is meaningless (zero until the first update).
    #[inline(always)]
    pub fn now() -> LowresSystemTimePoint {
        TimePoint::from_nanos(LOWRES_SYSTEM_NOW.with(|n| n.get()))
    }

    /// Seconds since the epoch of `t`.
    #[inline]
    pub fn to_time_t(t: LowresSystemTimePoint) -> i64 {
        t.nanos_since_epoch().div_euclid(1_000_000_000)
    }

    /// The time point `t` seconds after the epoch.
    #[inline]
    pub fn from_time_t(t: i64) -> LowresSystemTimePoint {
        TimePoint::from_nanos(t.saturating_mul(1_000_000_000))
    }
}

impl_nanos_clock!(LowresSystemClock);
