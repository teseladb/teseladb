//! A clock that measures the CPU time consumed by the calling thread.
//!
//! [`ThreadCputimeClock::now`] reads `CLOCK_THREAD_CPUTIME_ID` with `clock_gettime`. The reactor
//! uses it to report the total CPU time of a shard, and the stall detector to measure how long
//! a task has run without yielding. Durations
//! and time points are signed nanosecond counts.

use std::ffi::c_int;
use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};
use std::time::Duration;

#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

unsafe extern "C" {
    fn clock_gettime(clk: c_int, tp: *mut Timespec) -> c_int;
}

/// The POSIX `CLOCK_THREAD_CPUTIME_ID` clock id on Linux.
#[cfg(not(target_vendor = "apple"))]
const CLOCK_THREAD_CPUTIME_ID: c_int = 3;
/// The POSIX `CLOCK_THREAD_CPUTIME_ID` clock id on Apple platforms.
#[cfg(target_vendor = "apple")]
const CLOCK_THREAD_CPUTIME_ID: c_int = 16;

/// The tick count type of [`ThreadCputimeClock`]: signed nanoseconds.
pub type ThreadCputimeRep = i64;

/// A signed count of nanoseconds of thread CPU time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadCputimeDuration(pub ThreadCputimeRep);

impl ThreadCputimeDuration {
    /// The zero duration.
    pub const ZERO: Self = ThreadCputimeDuration(0);

    /// A duration of `n` nanoseconds.
    #[inline]
    pub const fn from_nanos(n: ThreadCputimeRep) -> Self {
        ThreadCputimeDuration(n)
    }

    /// Returns the duration in nanoseconds.
    #[inline]
    pub const fn count(self) -> ThreadCputimeRep {
        self.0
    }

    /// The duration as a `std::time::Duration` (negative durations become zero).
    #[inline]
    pub fn to_std(self) -> Duration {
        Duration::from_nanos(u64::try_from(self.0).unwrap_or(0))
    }

    /// Converts a `std::time::Duration`, truncating to nanoseconds and saturating at the
    /// largest count.
    #[inline]
    pub fn from_std(d: Duration) -> Self {
        ThreadCputimeDuration(
            ThreadCputimeRep::try_from(d.as_nanos()).unwrap_or(ThreadCputimeRep::MAX),
        )
    }
}

impl Add for ThreadCputimeDuration {
    type Output = Self;
    #[inline]
    fn add(self, o: Self) -> Self {
        ThreadCputimeDuration(self.0 + o.0)
    }
}

impl Sub for ThreadCputimeDuration {
    type Output = Self;
    #[inline]
    fn sub(self, o: Self) -> Self {
        ThreadCputimeDuration(self.0 - o.0)
    }
}

impl AddAssign for ThreadCputimeDuration {
    #[inline]
    fn add_assign(&mut self, o: Self) {
        self.0 += o.0;
    }
}

impl SubAssign for ThreadCputimeDuration {
    #[inline]
    fn sub_assign(&mut self, o: Self) {
        self.0 -= o.0;
    }
}

impl Neg for ThreadCputimeDuration {
    type Output = Self;
    #[inline]
    fn neg(self) -> Self {
        ThreadCputimeDuration(-self.0)
    }
}

/// A point of [`ThreadCputimeClock`]: the CPU time the calling thread had used, in nanoseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadCputimeTimePoint(pub ThreadCputimeRep);

impl ThreadCputimeTimePoint {
    /// The time point `d` after the epoch (zero CPU time).
    #[inline]
    pub const fn new(d: ThreadCputimeDuration) -> Self {
        ThreadCputimeTimePoint(d.0)
    }

    /// Returns the CPU time since the epoch.
    #[inline]
    pub const fn time_since_epoch(self) -> ThreadCputimeDuration {
        ThreadCputimeDuration(self.0)
    }
}

impl Sub for ThreadCputimeTimePoint {
    type Output = ThreadCputimeDuration;
    #[inline]
    fn sub(self, o: Self) -> ThreadCputimeDuration {
        ThreadCputimeDuration(self.0 - o.0)
    }
}

impl Add<ThreadCputimeDuration> for ThreadCputimeTimePoint {
    type Output = Self;
    #[inline]
    fn add(self, d: ThreadCputimeDuration) -> Self {
        ThreadCputimeTimePoint(self.0 + d.0)
    }
}

impl Sub<ThreadCputimeDuration> for ThreadCputimeTimePoint {
    type Output = Self;
    #[inline]
    fn sub(self, d: ThreadCputimeDuration) -> Self {
        ThreadCputimeTimePoint(self.0 - d.0)
    }
}

/// The per-thread CPU time clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadCputimeClock;

impl ThreadCputimeClock {
    /// Returns the CPU time used so far by the calling thread.
    ///
    /// # Panics
    /// Panics if `clock_gettime` fails, which does not happen on supported systems.
    #[inline]
    pub fn now() -> ThreadCputimeTimePoint {
        let mut tp = Timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `tp` is a valid, writable timespec with the C layout; the call writes only
        // into it.
        let ret = unsafe { clock_gettime(CLOCK_THREAD_CPUTIME_ID, &mut tp) };
        // Checked in every build profile.
        assert!(ret == 0);
        ThreadCputimeTimePoint(tp.tv_nsec + tp.tv_sec * 1_000_000_000)
    }
}
