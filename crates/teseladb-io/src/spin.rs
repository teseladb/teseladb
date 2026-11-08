//! A bounded spin before a thread sleeps (H-013 of the native tests against RefDb). A message
//! between two threads that sleep in `kevent` took 9,3 µs to go and come back on the test
//! machine, against 0,75 µs while they spun; a read crosses threads more than once. Thread-per-core
//! systems keep polling for this reason (AnotherProduct), and adaptive locks spin about a context switch
//! round trip before they park: a thread that is about to wait spins this long first.

use std::time::{Duration, Instant};

/// How long a thread spins before it sleeps: about four times a wake-up of a thread measured on
/// the test machine (4,6 µs one way), in the range of a context switch round trip.
pub const SPIN_BEFORE_SLEEP: Duration = Duration::from_micros(20);

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
