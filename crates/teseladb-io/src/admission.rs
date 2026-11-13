//! Admission of a core's new requests by their queue wait (012 FR-029), as in CoDel: the wait of
//! each message from its entry into the core's mailbox until the core starts it. Once every
//! message for [`SUSTAINED`] has waited more than [`TARGET`], the core refuses its new requests
//! with `Overloaded` before spending CPU on them; the first message that waited less ends it.
//! Refusing is quick, so the queue empties and the waits fall under the target again.

use std::time::{Duration, Instant};

/// The queue wait a core tolerates.
pub const TARGET: Duration = Duration::from_millis(5);
/// How long the wait must stay above [`TARGET`] before the core refuses.
pub const SUSTAINED: Duration = Duration::from_millis(100);

/// The state of a core's admission.
#[derive(Debug, Clone, Default)]
pub struct Admission {
    /// Since when every wait has been above the target.
    above_since: Option<Instant>,
    /// Requests refused since the core started (for the statistics).
    refused: u64,
}

impl Admission {
    /// A core that admits everything.
    pub fn new() -> Admission {
        Admission::default()
    }

    /// A message that waited `waited` starts at `now`: whether the core is overloaded, so a new
    /// request in it must be refused.
    pub fn observe(&mut self, waited: Duration, now: Instant) -> bool {
        if waited <= TARGET {
            self.above_since = None;
            return false;
        }
        let since = *self.above_since.get_or_insert(now);
        now.saturating_duration_since(since) >= SUSTAINED
    }

    /// One request was refused.
    pub fn count_refusal(&mut self) {
        self.refused += 1;
    }

    /// Requests refused since the core started.
    pub fn refused(&self) -> u64 {
        self.refused
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn short_waits_never_refuse() {
        let t = Instant::now();
        let mut a = Admission::new();
        for i in 0..1000 {
            assert!(!a.observe(TARGET, t + ms(i)));
        }
    }

    #[test]
    fn waits_above_the_target_for_the_whole_interval_refuse() {
        let t = Instant::now();
        let mut a = Admission::new();
        assert!(!a.observe(ms(6), t));
        assert!(!a.observe(ms(50), t + ms(99)));
        assert!(a.observe(ms(6), t + SUSTAINED));
        assert!(a.observe(ms(6), t + ms(150)));
        // One short wait ends it, and the interval starts over.
        assert!(!a.observe(ms(1), t + ms(151)));
        assert!(!a.observe(ms(6), t + ms(152)));
        assert!(!a.observe(ms(6), t + ms(251)));
        assert!(a.observe(ms(6), t + ms(252)));
    }

    #[test]
    fn a_burst_shorter_than_the_interval_does_not_refuse() {
        let t = Instant::now();
        let mut a = Admission::new();
        for i in 0..90 {
            assert!(!a.observe(ms(20), t + ms(i)));
        }
        assert!(!a.observe(ms(2), t + ms(90)));
        assert!(!a.observe(ms(20), t + ms(120)));
        a.count_refusal();
        assert_eq!(a.refused(), 1);
    }
}
