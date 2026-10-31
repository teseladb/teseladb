//! Simulated clocks per node: the monotonic one with a fixed rate drift in
//! [−4.5 %, +4.5 %] and the wall one with an offset bounded to ±50 ms that does not accumulate
//! (NTP, as D-004 requires; D-044), plus the one-off jumps and the clock at the edge of the margin
//! the `heavy` plan injects.

use teseladb_types::{Nanos, Rng};

/// Maximum drift of the monotonic clock, in parts per million.
pub const MAX_DRIFT_PPM: i64 = 45_000;
/// Maximum offset of the wall clock without jumps: two nodes differ by at most 100 ms, inside
/// the 125 ms margin of the clock rule (D-004, D-044).
pub const MAX_WALL_OFFSET: Nanos = Nanos::from_millis(50);

/// Limits a world draws its nodes' clocks from. The default ones are NTP's; a workload widens them
/// to break the clock rule on purpose (research R3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockLimits {
    /// Maximum rate drift of the monotonic clock (ppm).
    pub max_drift_ppm: i64,
    /// Maximum offset of the wall clock without jumps.
    pub max_wall_offset: Nanos,
}

impl Default for ClockLimits {
    fn default() -> ClockLimits {
        ClockLimits { max_drift_ppm: MAX_DRIFT_PPM, max_wall_offset: MAX_WALL_OFFSET }
    }
}

/// Clock of a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimClock {
    /// Rate drift of the monotonic clock (ppm).
    pub drift_ppm: i64,
    /// Offset of the wall clock (ns, signed).
    pub wall_offset_ns: i64,
    /// Accumulated injected jumps (ns, signed).
    pub jump_ns: i64,
    /// Wall time (Unix ms) at instant 0 of the world.
    pub epoch_ms: u64,
}

impl SimClock {
    /// Clock with random drift and offset within the default limits.
    pub fn random(rng: &mut Rng, epoch_ms: u64) -> SimClock {
        SimClock::within(rng, epoch_ms, ClockLimits::default())
    }

    /// Clock with random drift and offset within `limits`.
    pub fn within(rng: &mut Rng, epoch_ms: u64, limits: ClockLimits) -> SimClock {
        let drift = limits.max_drift_ppm;
        let drift_ppm = rng.range(0, 2 * drift as u64 + 1) as i64 - drift;
        let offset = limits.max_wall_offset.0;
        let wall_offset_ns = rng.range(0, 2 * offset + 1) as i64 - offset as i64;
        SimClock { drift_ppm, wall_offset_ns, jump_ns: 0, epoch_ms }
    }

    /// Perfect clock.
    pub fn exact(epoch_ms: u64) -> SimClock {
        SimClock { drift_ppm: 0, wall_offset_ns: 0, jump_ns: 0, epoch_ms }
    }

    /// Monotonic clock of the node at the world's real instant `t`.
    pub fn mono(&self, t: Nanos) -> Nanos {
        let t = i128::from(t.0);
        let v = t + t * i128::from(self.drift_ppm) / 1_000_000;
        Nanos(u64::try_from(v.max(0)).unwrap_or(u64::MAX))
    }

    /// The first real instant at which the monotonic clock reads at least `local`, searched from
    /// real instant `now`. The rate is constant but [`SimClock::mono`] truncates, so the division
    /// can land a nanosecond on either side.
    pub fn reaches(&self, now: Nanos, local: Nanos) -> Nanos {
        if self.mono(now) >= local {
            return now;
        }
        let rate = 1_000_000 + i128::from(self.drift_ppm);
        let t = (i128::from(local.0) * 1_000_000 + rate - 1) / rate;
        let mut t = Nanos(u64::try_from(t).unwrap_or(u64::MAX).max(now.0));
        while self.mono(t) < local {
            t = Nanos(t.0 + 1);
        }
        while t > now && self.mono(Nanos(t.0 - 1)) >= local {
            t = Nanos(t.0 - 1);
        }
        t
    }

    /// Wall clock of the node (Unix ms) at real instant `t`.
    pub fn wall_ms(&self, t: Nanos) -> u64 {
        let ns = i128::from(t.0) + i128::from(self.wall_offset_ns) + i128::from(self.jump_ns);
        let ms = i128::from(self.epoch_ms) + ns.div_euclid(1_000_000);
        u64::try_from(ms.max(0)).unwrap_or(u64::MAX)
    }

    /// The first real instant, from real instant `now` on, at which the wall clock reads at least
    /// `ms` (Unix ms) if it does not jump.
    pub fn wall_reaches(&self, now: Nanos, ms: u64) -> Nanos {
        let ns = (i128::from(ms) - i128::from(self.epoch_ms)) * 1_000_000
            - i128::from(self.wall_offset_ns)
            - i128::from(self.jump_ns);
        Nanos(u64::try_from(ns.max(0)).unwrap_or(u64::MAX).max(now.0))
    }

    /// Offset of the wall clock now, jumps included (ns, signed).
    pub fn offset_ns(&self) -> i64 {
        self.wall_offset_ns + self.jump_ns
    }

    /// One-off jump of the wall clock (`heavy` plan).
    pub fn jump(&mut self, delta_ns: i64) {
        self.jump_ns += delta_ns;
    }

    /// NTP steps the wall clock back to its usual offset: the jumps are undone (D-004, D-044).
    pub fn resync(&mut self) {
        self.jump_ns = 0;
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn wall_reaches_finds_the_first_instant_the_wall_clock_reads_a_time() {
        let mut c = SimClock { wall_offset_ns: -1_234_567, ..SimClock::exact(1_000) };
        c.jump(-2_000_000_001);
        for ms in [5_000, 5_001, 123_456] {
            let t = c.wall_reaches(Nanos::ZERO, ms);
            assert!(c.wall_ms(t) >= ms && c.wall_ms(Nanos(t.0 - 1)) < ms, "{ms}");
        }
        let late = Nanos::from_secs(500);
        assert_eq!(c.wall_reaches(late, 5_000), late, "already past it");
    }

    #[test]
    fn reaches_finds_the_first_instant_the_clock_reads_a_local_time() {
        for drift_ppm in [-MAX_DRIFT_PPM, -1, 0, 7, MAX_DRIFT_PPM] {
            let c = SimClock { drift_ppm, ..SimClock::exact(0) };
            for local in [1, 999, 10_000_000, 3_600_000_000_123] {
                let local = Nanos(local);
                let t = c.reaches(Nanos::ZERO, local);
                assert!(
                    c.mono(t) >= local && c.mono(Nanos(t.0 - 1)) < local,
                    "{drift_ppm} {local:?}"
                );
            }
            // Already there: now.
            let now = Nanos::from_secs(5);
            assert_eq!(c.reaches(now, c.mono(now)), now);
        }
    }

    #[test]
    fn reaches_corrects_the_division_on_either_side() {
        // The first estimate can be a nanosecond early or late; the answer is always the first
        // instant whose monotonic reading reaches the local time.
        for drift_ppm in [-MAX_DRIFT_PPM, -333, -1, 1, 333, MAX_DRIFT_PPM] {
            let c = SimClock { drift_ppm, ..SimClock::exact(0) };
            for local in (1..20_000u64).map(|i| Nanos(i * 7_919)) {
                let t = c.reaches(Nanos::ZERO, local);
                assert!(c.mono(t) >= local, "drift {drift_ppm}, local {local:?}");
                assert!(c.mono(Nanos(t.0 - 1)) < local, "drift {drift_ppm}, local {local:?}");
            }
        }
    }

    #[test]
    fn bounded_without_jumps_after_an_hour() {
        let mut rng = Rng::new(7);
        let hour = Nanos::from_secs(3600);
        for _ in 0..1000 {
            let c = SimClock::random(&mut rng, 1_000_000);
            assert!(c.drift_ppm.abs() <= MAX_DRIFT_PPM);
            let err = c.wall_ms(hour) as i64 - (1_000_000 + 3_600_000);
            assert!(err.abs() <= 50, "offset {err} ms after 1 h");
            // The monotonic clock drifts, but within the rate.
            let m = c.mono(hour).0 as i128;
            let drift = (m - hour.0 as i128).abs();
            assert!(drift <= hour.0 as i128 * 45 / 1000 + 1);
        }
    }

    #[test]
    fn jumps_and_exact() {
        let mut c = SimClock::exact(0);
        assert_eq!(c.mono(Nanos::from_secs(5)), Nanos::from_secs(5));
        assert_eq!(c.wall_ms(Nanos::from_secs(5)), 5000);
        c.jump(-2_000_000_000);
        assert_eq!(c.offset_ns(), -2_000_000_000);
        assert_eq!(c.wall_ms(Nanos::from_secs(5)), 3000);
        assert_eq!(c.wall_ms(Nanos::ZERO), 0, "does not go below 0");
        c.resync();
        assert_eq!(c.wall_ms(Nanos::from_secs(5)), 5000, "NTP undoes the jumps");
        let slow = SimClock { drift_ppm: -1_000_000, ..SimClock::exact(0) };
        assert_eq!(slow.mono(Nanos::from_secs(1)), Nanos::ZERO);
    }
}
