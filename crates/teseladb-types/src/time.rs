//! Monotonic time handed in by the caller. State machines never read the clock.

use core::fmt;
use core::ops::{Add, Sub};

/// Instant in nanoseconds since an arbitrary origin (chosen by the caller).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Nanos(pub u64);

impl Nanos {
    /// Instant zero.
    pub const ZERO: Nanos = Nanos(0);

    /// From microseconds.
    pub const fn from_micros(us: u64) -> Nanos {
        Nanos(us * 1_000)
    }

    /// From milliseconds.
    pub const fn from_millis(ms: u64) -> Nanos {
        Nanos(ms * 1_000_000)
    }

    /// From seconds.
    pub const fn from_secs(s: u64) -> Nanos {
        Nanos(s * 1_000_000_000)
    }

    /// Value in nanoseconds.
    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    /// Value in milliseconds (truncated).
    pub const fn as_millis(self) -> u64 {
        self.0 / 1_000_000
    }

    /// Difference with `earlier`, zero if `earlier` is later.
    pub const fn saturating_since(self, earlier: Nanos) -> Nanos {
        Nanos(self.0.saturating_sub(earlier.0))
    }
}

impl Add for Nanos {
    type Output = Nanos;
    fn add(self, rhs: Nanos) -> Nanos {
        Nanos(self.0.saturating_add(rhs.0))
    }
}

impl Sub for Nanos {
    type Output = Nanos;
    fn sub(self, rhs: Nanos) -> Nanos {
        self.saturating_since(rhs)
    }
}

impl fmt::Debug for Nanos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}ns", self.0)
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn conversions_and_saturation() {
        assert_eq!(Nanos::from_micros(2), Nanos(2_000));
        assert_eq!(Nanos::from_millis(3).as_millis(), 3);
        assert_eq!(Nanos::from_secs(1).as_nanos(), 1_000_000_000);
        assert_eq!(Nanos(5) - Nanos(9), Nanos::ZERO);
        assert_eq!(Nanos(9) - Nanos(5), Nanos(4));
        assert_eq!(Nanos(u64::MAX) + Nanos(1), Nanos(u64::MAX));
        assert_eq!(format!("{:?}", Nanos(7)), "7ns");
        assert_eq!(Nanos::default(), Nanos::ZERO);
    }
}
