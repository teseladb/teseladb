//! `ExpiringFifo` paths the other tests leave out: the default expiry (nothing happens to the
//! expired value), the promise expiry (the waiter fails with a timeout), `front_ref`,
//! `reserve`, a deadline at the clock's maximum (never expires) and `take_front`, which skips
//! expired entries behind the one it takes.

#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use std::time::Duration;

use teseladb::engine::core::expiring_fifo::{DummyExpiry, ExpiringFifo, PromiseExpiry};
use teseladb::engine::core::future::{ExceptionPtr, Promise};
use teseladb::engine::core::manual_clock::ManualClock;
use teseladb::engine::core::reactor::yield_;
use teseladb::engine::core::thread::async_;
use teseladb::engine::core::timed_out_error::TimedOutError;
use teseladb::engine::core::timer::Clock;
use teseladb::engine_test_case;

engine_test_case!(default_and_promise_expiry_front_ref_take_front, {
    async_(|| -> Result<(), ExceptionPtr> {
        let s1 = Duration::from_secs(1);
        let mut fifo =
            ExpiringFifo::<i32, DummyExpiry<i32>, ManualClock>::new(DummyExpiry::default());
        fifo.reserve(8);
        // SAFETY: the fifo stays in place until the end of the thread, after its entries.
        unsafe {
            fifo.push_back_with_timeout(1, ManualClock::now() + s1);
            fifo.push_back_with_timeout(2, ManualClock::max_time_point());
            fifo.push_back_with_timeout(3, ManualClock::now() + s1);
            fifo.push_back_with_timeout(4, ManualClock::max_time_point());
        }
        assert_eq!(*fifo.front_ref(), 1);
        assert_eq!(fifo.take_front(), Some(1));
        // 3 expires behind 2, which never expires: take_front() of 2 drops it.
        ManualClock::advance(s1);
        yield_().get()?;
        assert_eq!(fifo.size(), 2);
        assert_eq!(*fifo.front_ref(), 2);
        assert_eq!(fifo.take_front(), Some(2));
        assert_eq!(fifo.take_front(), Some(4));
        assert_eq!(fifo.take_front(), None);
        assert!(fifo.empty());

        // A waiting promise that expires fails with a timeout.
        let mut waiters = ExpiringFifo::<Promise<()>, PromiseExpiry<()>, ManualClock>::new(
            PromiseExpiry::default(),
        );
        let mut p = Promise::<()>::new();
        let f = p.get_future();
        // SAFETY: as above.
        unsafe { waiters.push_back_with_timeout(p, ManualClock::now() + s1) };
        ManualClock::advance(s1);
        yield_().get()?;
        assert!(waiters.empty());
        let e = f.get().expect_err("timed out");
        assert!(e.is::<TimedOutError>(), "{e}");
        Ok(())
    })
});
