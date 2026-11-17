#![cfg(target_os = "linux")]
//! A periodic high resolution timer that cancels itself in its callback must not leave the
//! timer descriptor off for the timers armed afterwards.
//!
//! After the expired timers ran, the reactor re-armed the timer descriptor only when the
//! timer set was not empty. A timer cancelled while it runs leaves the set empty but its
//! "next timeout" set, so the set believes the descriptor is armed and does not arm it for
//! the next timer, which then never fires. The fix of the upstream change "timer: Enable
//! highres timer based on next timeout value" (anotherproduct#3013): re-arm from the next timeout.
//! The test is the upstream regression test `test_highres_periodic_cancel_in_callback`.

use std::cell::Cell;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::time::{Duration, Instant};

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::reactor::yield_;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::timer::{SteadyClockType, Timer};
use teseladb::engine_test_case;

engine_test_case!(a_periodic_timer_cancelled_in_its_callback_lets_the_next_timer_fire, {
    thread_async(move || -> Result<(), ExceptionPtr> {
        let t1_fired = Rc::new(Cell::new(false));
        let t2_fired = Rc::new(Cell::new(false));

        let mut t2 = pin!(Timer::<SteadyClockType>::new());
        let f2 = Rc::clone(&t2_fired);
        t2.as_mut().set_callback(move || f2.set(true));
        // SAFETY: the timer stays pinned on this stack until the end of the test, after the
        // last callback that reaches it (the loop below runs for a second, the timers fire
        // within 15 ms); it is not moved through the pointer.
        let t2p: *mut Timer<SteadyClockType> = unsafe { t2.as_mut().get_unchecked_mut() };

        let mut t1 = pin!(Timer::<SteadyClockType>::new());
        // SAFETY: as for `t2p`.
        let t1p: *mut Timer<SteadyClockType> = unsafe { t1.as_mut().get_unchecked_mut() };
        let f1 = Rc::clone(&t1_fired);
        t1.as_mut().set_callback(move || {
            f1.set(true);
            // SAFETY: see `t1p` and `t2p`; the original cancels the timer in its own callback
            // the same way.
            unsafe { Pin::new_unchecked(&mut *t1p).cancel() };
            drop(yield_().then(move |()| {
                // SAFETY: see `t2p`.
                unsafe { Pin::new_unchecked(&mut *t2p).arm_in(Duration::from_millis(10)) };
            }));
        });

        t1.as_mut().arm_periodic(Duration::from_millis(5));

        let end = Instant::now() + Duration::from_secs(1);
        while Instant::now() < end {
            yield_().get()?;
        }

        assert!(t1_fired.get(), "the periodic timer fired");
        assert!(t2_fired.get(), "the timer armed after the cancelled one fired");
        Ok(())
    })
});
