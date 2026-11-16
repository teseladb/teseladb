#![cfg(target_os = "linux")]
//! A timer armed one tick after the time the timer set last expired at goes in the bucket of
//! the bit in which the two differ, as in the original, whose `count_leading_zeros` of a signed
//! timestamp does not count the sign bit (`bitset-iter.hh`). Counting it put such a timer one
//! bucket too high: in the bucket reserved for the timers already due, and when the last expiry
//! was at an even tick the set's assertion `index < N_BUCKETS - 1` aborted the process (seen
//! after minutes of a node's load, where steady-clock deadlines one nanosecond apart happen).

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::lowres_clock::TimePoint;
use teseladb::engine::core::manual_clock::{MANUAL_CLOCK_NOW, ManualClock};
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::timer::Timer;
use teseladb::engine_test_case;

engine_test_case!(a_timer_one_tick_after_the_last_expiry_fires_in_its_turn, {
    thread_async(move || -> Result<(), ExceptionPtr> {
        // An even tick ahead of the clock, where the first timer makes the set expire.
        let now = MANUAL_CLOCK_NOW.load(Ordering::SeqCst);
        let at = (now + 2) & !1;
        let first = Rc::new(Cell::new(false));
        let f = Rc::clone(&first);
        let mut a = std::pin::pin!(Timer::<ManualClock>::with_callback(move || f.set(true)));
        a.as_mut().arm(TimePoint::from_nanos(at), None);
        ManualClock::advance(Duration::from_nanos(u64::try_from(at - now).unwrap_or(2)));
        assert!(first.get(), "the first timer fired at its tick");
        // One tick after the expiry: it differs from it in the lowest bit only.
        let second = Rc::new(Cell::new(false));
        let s = Rc::clone(&second);
        let mut b = std::pin::pin!(Timer::<ManualClock>::with_callback(move || s.set(true)));
        b.as_mut().arm(TimePoint::from_nanos(at + 1), None);
        assert!(!second.get(), "not due yet");
        ManualClock::advance(Duration::from_nanos(1));
        assert!(second.get(), "the second timer fired one tick later");
        Ok(())
    })
});
