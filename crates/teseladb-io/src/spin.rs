//! A bounded spin before a thread sleeps (H-013 of the native tests against RefDb). A message
//! between two threads that sleep in `kevent` took 9,3 µs to go and come back on the test
//! machine, against 0,75 µs while they spun; a read crosses threads more than once. Thread-per-core
//! systems keep polling for this reason (AnotherProduct), and adaptive locks spin about a context switch
//! round trip before they park: a thread that is about to wait spins this long first.

use std::time::{Duration, Instant};

/// How long a thread spins before it sleeps: about four times a wake-up of a thread measured on
/// the test machine (4,6 µs one way), in the range of a context switch round trip.
pub const SPIN_BEFORE_SLEEP: Duration = Duration::from_micros(20);

/// Longest an [`AdaptiveSpin`] spins: about ten wake-ups of a thread, past which sleeping costs
/// less than spinning.
pub const SPIN_CAP: Duration = Duration::from_micros(60);

/// A spin window that follows the waits it serves (H-042), as HotSpot's adaptive spinning
/// lengthens or shortens a monitor's spin by whether the last spins caught the lock: a wait that
/// ended a little past the window lengthens it to that wait with a margin, up to [`SPIN_CAP`]; a
/// wait longer than the cap (a write that waits for the disk) brings it back to
/// [`SPIN_BEFORE_SLEEP`], so a thread does not keep spinning for waits it never catches.
#[derive(Clone, Copy, Debug)]
pub struct AdaptiveSpin {
    window: Duration,
}

impl Default for AdaptiveSpin {
    fn default() -> AdaptiveSpin {
        AdaptiveSpin { window: SPIN_BEFORE_SLEEP }
    }
}

impl AdaptiveSpin {
    /// How long the next wait spins before it sleeps.
    pub fn window(&self) -> Duration {
        self.window
    }

    /// A wait took `waited`.
    pub fn observe(&mut self, waited: Duration) {
        self.window = if waited > SPIN_CAP {
            SPIN_BEFORE_SLEEP
        } else {
            // A quarter more than the wait, so its usual spread still ends inside the spin.
            (waited + waited / 4).clamp(SPIN_BEFORE_SLEEP, SPIN_CAP)
        };
    }
}

/// Spins until `ready` says so or `window` passes; whether it did.
pub fn spin_until(window: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if ready() {
            return true;
        }
        if start.elapsed() >= window {
            return false;
        }
        std::hint::spin_loop();
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn an_adaptive_spin_follows_short_waits_and_drops_after_a_long_one() {
        let mut s = AdaptiveSpin::default();
        assert_eq!(s.window(), SPIN_BEFORE_SLEEP);
        s.observe(Duration::from_micros(28));
        assert_eq!(s.window(), Duration::from_micros(35), "a quarter more than the wait");
        s.observe(Duration::from_micros(5));
        assert_eq!(s.window(), SPIN_BEFORE_SLEEP, "never below the base window");
        s.observe(Duration::from_micros(55));
        assert_eq!(s.window(), SPIN_CAP, "never above the cap");
        s.observe(Duration::from_millis(4));
        assert_eq!(s.window(), SPIN_BEFORE_SLEEP, "a wait for the disk is not spun for");
    }

    #[test]
    fn a_spin_ends_as_soon_as_its_condition_holds() {
        let mut looks = 0;
        assert!(spin_until(Duration::from_secs(5), || {
            looks += 1;
            looks == 3
        }));
        assert_eq!(looks, 3);
    }

    #[test]
    fn a_spin_gives_up_after_its_window() {
        let start = Instant::now();
        assert!(!spin_until(Duration::from_micros(200), || false));
        assert!(start.elapsed() >= Duration::from_micros(200));
    }

    #[test]
    fn a_spin_sees_what_another_thread_does_within_its_window() {
        let flag = Arc::new(AtomicBool::new(false));
        let set = Arc::clone(&flag);
        let t = std::thread::spawn(move || set.store(true, Ordering::Release));
        t.join().unwrap();
        assert!(spin_until(Duration::from_secs(1), || flag.load(Ordering::Acquire)));
    }
}
