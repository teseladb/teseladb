//! Semaphore paths beyond semaphore_test (the original's semaphore.hh semantics): a timed wait
//! whose deadline already passed fails at once with the timeout error; an abortable wait is
//! served at once when units are there, fails with the broken error on a broken semaphore
//! and with the factory's aborted error when the source was already aborted; `signal` and
//! `consume` do nothing on a broken semaphore; move-assignment takes the units and the
//! broken state; `consume_units` and `with_semaphore_for`; and the error messages.
#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use std::cell::Cell;
use std::pin::pin;
use std::rc::Rc;
use std::time::Duration;

use teseladb::engine::core::abort_source::AbortSource;
use teseladb::engine::core::future::{ExceptionPtr, make_ready_future};
use teseladb::engine::core::semaphore::{
    BrokenNamedSemaphore, BrokenSemaphore, NamedSemaphore, NamedSemaphoreAborted,
    NamedSemaphoreExceptionFactory, NamedSemaphoreTimedOut, Semaphore, SemaphoreAborted,
    SemaphoreTimedOut, consume_units, with_semaphore_for,
};
use teseladb::engine::core::thread::async_;
use teseladb::engine::core::timer::{Clock, SteadyClockType};
use teseladb::engine_test_case;

fn named(count: usize, name: &str) -> NamedSemaphore {
    NamedSemaphore::with_factory(count, NamedSemaphoreExceptionFactory { name: name.into() })
}

#[test]
fn error_messages() {
    assert_eq!(BrokenSemaphore.to_string(), "Semaphore broken");
    assert_eq!(SemaphoreTimedOut.to_string(), "Semaphore timedout");
    assert_eq!(SemaphoreAborted.to_string(), "Semaphore aborted");
    assert_eq!(NamedSemaphoreTimedOut::new("s").to_string(), "Semaphore timed out: s");
    assert_eq!(BrokenNamedSemaphore::new("s").to_string(), "Semaphore broken: s");
    assert_eq!(NamedSemaphoreAborted::new("s").to_string(), "Semaphore aborted: s");
    // An empty message falls back to the static one.
    assert_eq!(NamedSemaphoreTimedOut::default().to_string(), "Named semaphore timed out");
}

engine_test_case!(a_deadline_in_the_past_times_out_at_once, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut sem = named(0, "past");
        let past = <SteadyClockType as Clock>::now();
        // SAFETY: the semaphore stays in place until the future is resolved.
        let f = unsafe { sem.wait_until(past, 1) };
        assert!(f.available(), "no wait is queued");
        let e = f.get().expect_err("timed out");
        assert!(e.downcast_ref::<NamedSemaphoreTimedOut>().is_some(), "{e}");
        assert!(e.downcast_ref::<SemaphoreTimedOut>().is_some(), "a named timeout is a timeout");
        assert_eq!(sem.waiters(), 0);
        Ok(())
    })
});

engine_test_case!(abortable_waits_ready_broken_and_already_aborted, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut sem = named(2, "ab");
        let mut src = pin!(AbortSource::new());
        // SAFETY: the semaphore and the source stay in place until the futures resolve.
        unsafe { sem.wait_abortable(src.as_mut(), 1) }.get()?;
        assert_eq!(sem.current(), 1, "served at once");
        src.as_mut().request_abort();
        // SAFETY: as above.
        let e = unsafe { sem.wait_abortable(src.as_mut(), 2) }.get().expect_err("aborted");
        assert!(e.downcast_ref::<NamedSemaphoreAborted>().is_some(), "{e}");
        assert!(e.to_string().ends_with("(Semaphore aborted: ab)"), "{e}");
        assert_eq!(sem.waiters(), 0);
        sem.broken();
        let mut fresh = pin!(AbortSource::new());
        // SAFETY: as above.
        let e = unsafe { sem.wait_abortable(fresh.as_mut(), 5) }.get().expect_err("broken");
        assert!(e.downcast_ref::<BrokenNamedSemaphore>().is_some(), "{e}");
        Ok(())
    })
});

engine_test_case!(signal_and_consume_do_nothing_on_a_broken_semaphore, {
    let mut sem = Semaphore::new(3);
    sem.broken();
    let before = sem.available_units();
    sem.signal(10);
    sem.consume(2);
    assert_eq!(sem.available_units(), before);
    // SAFETY: the semaphore outlives the future, resolved right here.
    let e = sem.wait(1).get().expect_err("broken");
    assert!(e.downcast_ref::<BrokenSemaphore>().is_some());
    make_ready_future(())
});

engine_test_case!(move_assignment_takes_the_units_and_the_broken_state, {
    let mut a = Semaphore::new(1);
    let mut b = Semaphore::new(7);
    a.assign_from(&mut b);
    assert_eq!(a.current(), 7);
    let mut c = Semaphore::new(4);
    c.broken();
    a.assign_from(&mut c);
    // SAFETY: the semaphore outlives the future, resolved right here.
    let e = a.wait(1).get().expect_err("the broken state moved");
    assert!(e.downcast_ref::<BrokenSemaphore>().is_some());
    // SAFETY: as above.
    assert_eq!(c.available_units(), 0, "breaking emptied the counter");
    c.signal(1);
    assert!(c.wait(1).get().is_ok(), "the moved-from semaphore is not broken");
    make_ready_future(())
});

engine_test_case!(consumed_units_go_negative_and_come_back, {
    let mut sem = Semaphore::new(1);
    // SAFETY: the semaphore outlives the units and is not borrowed when they return.
    let units = unsafe { consume_units(&mut sem, 3) };
    assert_eq!(sem.available_units(), -2);
    drop(units);
    assert_eq!(sem.available_units(), 1);
    make_ready_future(())
});

engine_test_case!(with_semaphore_for_runs_or_times_out, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut sem = Semaphore::new(1);
        let ran = Rc::new(Cell::new(0));
        let r = ran.clone();
        // SAFETY: the semaphore outlives the future, resolved right here.
        let v = unsafe {
            with_semaphore_for(&mut sem, 1, Duration::from_secs(5), move || {
                r.set(r.get() + 1);
                Ok::<i32, ExceptionPtr>(7)
            })
        }
        .get()?;
        assert_eq!(v, 7);
        assert_eq!(sem.current(), 1, "the units came back");
        sem.consume(1);
        let r = ran.clone();
        // SAFETY: as above.
        let e = unsafe {
            with_semaphore_for(&mut sem, 1, Duration::from_millis(10), move || {
                r.set(r.get() + 1);
                Ok::<i32, ExceptionPtr>(8)
            })
        }
        .get()
        .expect_err("timed out");
        assert!(e.downcast_ref::<SemaphoreTimedOut>().is_some(), "{e}");
        assert_eq!(ran.get(), 1, "the function did not run");
        Ok(())
    })
});
