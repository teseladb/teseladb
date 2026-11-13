//! The rate at which a core's leaders admit new writes while its LSM asks to slow down (005
//! FR-038, D-165).
//!
//! While the LSM answers [`Pressure::Slow`] the core admits at most `rate` writes per second.
//! The rate starts at what the core admitted in its last whole second, drops by a fifth each time
//! the LSM's debt grows and rises by a quarter each time it shrinks, never below
//! [`MIN_RATE`]. With [`Pressure::Normal`] there is no limit; [`Pressure::Reject`] keeps the limit
//! and the leader refuses on top of it.

use teseladb_lsm::Pressure;
use teseladb_types::Nanos;

/// The lowest rate, in writes per second.
pub const MIN_RATE: u64 = 16;

const SECOND: u64 = 1_000_000_000;

/// How far behind the clock the next free slot may fall: the writes a core may admit at once
/// after a quiet spell.
const BURST: u64 = 10_000_000;

/// Admission of a core's new writes (005 FR-038).
#[derive(Debug, Clone, Default)]
pub struct WriteThrottle {
    /// Writes per second while slowing down; `None` without a limit.
    rate: Option<u64>,
    /// The LSM's debt when the rate last changed.
    debt: u64,
    /// The instant from which the next write may be admitted.
    next_free: u64,
    /// Start of the current second and the writes admitted in it.
    window: (u64, u64),
    /// Writes admitted in the last whole second.
    last_second: u64,
}

impl WriteThrottle {
    /// The current rate in writes per second, `None` without a limit.
    pub fn rate(&self) -> Option<u64> {
        self.rate
    }

    /// Follows the LSM's pressure and debt (files in level 0 plus frozen memtables).
    pub fn observe(&mut self, now: Nanos, pressure: Pressure, debt: u64) {
        self.roll(now.0);
        match (pressure, self.rate) {
            (Pressure::Normal, _) => self.rate = None,
            (Pressure::Slow | Pressure::Reject, None) => {
                self.rate = Some(self.last_second.max(self.window.1).max(MIN_RATE));
                self.next_free = now.0;
                self.debt = debt;
            }
            (Pressure::Slow | Pressure::Reject, Some(rate)) => {
                if debt > self.debt {
                    self.rate = Some((rate - rate / 5).max(MIN_RATE));
                } else if debt < self.debt {
                    self.rate = Some(rate.saturating_add(rate / 4).max(rate + 1));
                }
                self.debt = debt;
            }
        }
    }

    /// Whether a new write may be admitted at `now`; an admitted write takes its slot.
    pub fn admit(&mut self, now: Nanos) -> bool {
        self.roll(now.0);
        if let Some(rate) = self.rate {
            let start = self.next_free.max(now.0.saturating_sub(BURST));
            if start > now.0 {
                return false;
            }
            self.next_free = start + SECOND / rate;
        }
        self.window.1 += 1;
        true
    }

    fn roll(&mut self, now: u64) {
        if now >= self.window.0 + SECOND {
            let whole = now >= self.window.0 + 2 * SECOND;
            self.last_second = if whole { 0 } else { self.window.1 };
            self.window = (now, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    fn admitted_in(t: &mut WriteThrottle, from: u64, to: u64, step: u64) -> u64 {
        (from..to).step_by(usize::try_from(step).unwrap()).filter(|n| t.admit(Nanos(*n))).count()
            as u64
    }

    #[test]
    fn without_pressure_every_write_goes_in() {
        let mut t = WriteThrottle::default();
        t.observe(Nanos(0), Pressure::Normal, 3);
        assert_eq!(admitted_in(&mut t, 0, SECOND, MS / 10), 10_000);
        assert_eq!(t.rate(), None);
    }

    #[test]
    fn slowing_starts_at_the_rate_of_the_last_second() {
        let mut t = WriteThrottle::default();
        assert_eq!(admitted_in(&mut t, 0, SECOND, MS), 1000);
        t.observe(Nanos(SECOND), Pressure::Slow, 20);
        assert_eq!(t.rate(), Some(1000));
        // At the same pace it admits all; at ten times the pace, a tenth plus the burst.
        let got = admitted_in(&mut t, SECOND, 2 * SECOND, MS / 10);
        assert!((1000..=1011).contains(&got), "{got}");
    }

    #[test]
    fn the_rate_drops_a_fifth_as_debt_grows_and_rises_a_quarter_as_it_shrinks() {
        let mut t = WriteThrottle::default();
        admitted_in(&mut t, 0, SECOND, MS);
        t.observe(Nanos(SECOND), Pressure::Slow, 20);
        t.observe(Nanos(SECOND + 1), Pressure::Slow, 21);
        assert_eq!(t.rate(), Some(800));
        t.observe(Nanos(SECOND + 2), Pressure::Slow, 21);
        assert_eq!(t.rate(), Some(800), "same debt, same rate");
        t.observe(Nanos(SECOND + 3), Pressure::Slow, 20);
        assert_eq!(t.rate(), Some(1000));
        t.observe(Nanos(SECOND + 4), Pressure::Normal, 4);
        assert_eq!(t.rate(), None);
    }

    #[test]
    fn the_rate_never_falls_below_the_minimum_and_always_climbs_back() {
        let mut t = WriteThrottle::default();
        t.observe(Nanos(0), Pressure::Slow, 20);
        assert_eq!(t.rate(), Some(MIN_RATE));
        for d in 21..60 {
            t.observe(Nanos(d), Pressure::Slow, d);
        }
        assert_eq!(t.rate(), Some(MIN_RATE));
        t.observe(Nanos(100), Pressure::Slow, 10);
        assert_eq!(t.rate(), Some(MIN_RATE + 4));
        let got = admitted_in(&mut t, 0, SECOND, MS);
        assert!((MIN_RATE..=MIN_RATE + 5).contains(&got), "{got}");
    }

    #[test]
    fn a_quiet_spell_lets_only_a_short_burst_through() {
        let mut t = WriteThrottle::default();
        t.observe(Nanos(0), Pressure::Slow, 20);
        t.observe(Nanos(1), Pressure::Slow, 10);
        t.observe(Nanos(2), Pressure::Slow, 5);
        let rate = t.rate().unwrap();
        let later = 5 * SECOND;
        let burst = (0..1000).take_while(|_| t.admit(Nanos(later))).count() as u64;
        assert!(burst <= rate * BURST / SECOND + 1, "{burst} at {rate}/s");
    }
}
