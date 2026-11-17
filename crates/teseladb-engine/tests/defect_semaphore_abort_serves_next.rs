#![cfg(target_os = "linux")]
//! A waiter that leaves the head of a semaphore's wait list because it timed out or was
//! aborted lets the semaphore serve the waiters behind it whose units are available, without
//! waiting for another signal. A waiter that leaves from the middle of the list does not let
//! the ones behind it overtake the one still waiting ahead of it.

#![allow(unsafe_code)]

use std::pin::Pin;
use std::time::Duration;

use teseladb::engine::core::abort_source::AbortSource;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::semaphore::{
    Semaphore, SemaphoreAborted, SemaphoreTimedOut, get_units, get_units_abortable, get_units_for,
};
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine_test_case;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

// With 40 of 100 units available, a request for 50 that times out must not leave a request
// for 10 behind it waiting.
engine_test_case!(test_semaphore_timeout_of_first_waiter_serves_next, {
    thread_async(move || -> Result<(), ExceptionPtr> {
        let mut sem = Semaphore::new(100);
        // SAFETY (all the waits below): the semaphore stays alive and in place in this frame
        // until the units are gone and the futures have resolved.
        // SAFETY: the waits above, the semaphore stays in place in this frame.
        let held = unsafe { get_units(&mut sem, 60) }.get()?; // 40 units left
        // SAFETY: the waits above, the semaphore stays in place in this frame.
        let big = unsafe { get_units_for(&mut sem, 50, ms(10)) }; // can't be served
        // SAFETY: the waits above, the semaphore stays in place in this frame.
        let small = unsafe { get_units(&mut sem, 10) }; // fits, but is behind big
        sleep(ms(50)).get()?; // big has timed out
        assert!(matches!(big.get(), Err(ref e) if e.is::<SemaphoreTimedOut>()));
        assert_eq!(sem.waiters(), 0usize);
        let small_units = small.get()?;
        assert_eq!(sem.available_units(), 30);
        drop(small_units);
        drop(held);
        Ok(())
    })
});

// The same, when the waiter at the head of the wait list is aborted with an abort source.
engine_test_case!(test_semaphore_abort_of_first_waiter_serves_next, {
    thread_async(move || -> Result<(), ExceptionPtr> {
        let mut sem = Semaphore::new(100);
        let mut as_: Pin<Box<AbortSource>> = Box::pin(AbortSource::new());
        // SAFETY (all the waits below): the semaphore and the abort source stay alive and in
        // place in this frame until the units are gone and the futures have resolved.
        // SAFETY: the waits above, the semaphore stays in place in this frame.
        let held = unsafe { get_units(&mut sem, 60) }.get()?; // 40 units left
        // SAFETY: the waits above, the semaphore stays in place in this frame.
        let big = unsafe { get_units_abortable(&mut sem, 50, as_.as_mut()) }; // can't be served
        // SAFETY: the waits above, the semaphore stays in place in this frame.
        let small = unsafe { get_units(&mut sem, 10) }; // fits, but is behind big
        as_.as_mut().request_abort();
        assert!(matches!(big.get(), Err(ref e) if e.is::<SemaphoreAborted>()));
        assert_eq!(sem.waiters(), 0usize);
        let small_units = small.get()?;
        assert_eq!(sem.available_units(), 30);
        drop(small_units);
        drop(held);
        Ok(())
    })
});

// A waiter that times out in the middle of the wait list, with a waiter ahead of it that still
// can't be served, must not let the waiters behind it overtake the one ahead.
engine_test_case!(test_semaphore_timeout_in_the_middle_keeps_order, {
    thread_async(move || -> Result<(), ExceptionPtr> {
        let mut sem = Semaphore::new(100);
        // SAFETY (all the waits below): the semaphore stays alive and in place in this frame
        // until the units are gone and the futures have resolved.
        // SAFETY: the waits above, the semaphore stays in place in this frame.
        let mut held = unsafe { get_units(&mut sem, 60) }.get()?; // 40 units left
        // SAFETY: the waits above, the semaphore stays in place in this frame.
        let first = unsafe { get_units(&mut sem, 50) }; // can't be served
        // SAFETY: the waits above, the semaphore stays in place in this frame.
        let middle = unsafe { get_units_for(&mut sem, 50, ms(10)) }; // will time out
        // SAFETY: the waits above, the semaphore stays in place in this frame.
        let last = unsafe { get_units(&mut sem, 10) }; // fits, but is behind first
        sleep(ms(50)).get()?;
        assert!(matches!(middle.get(), Err(ref e) if e.is::<SemaphoreTimedOut>()));
        assert_eq!(sem.waiters(), 2usize);
        assert!(!first.available());
        assert!(!last.available());
        held.return_all();
        let first_units = first.get()?;
        let last_units = last.get()?;
        drop(last_units);
        drop(first_units);
        Ok(())
    })
});
