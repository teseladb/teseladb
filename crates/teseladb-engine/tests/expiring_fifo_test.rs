//! Tests of `ExpiringFifo`: entries added with a deadline expire, and are handed to the
//! expiry callback, when the deadline passes; the others keep their order.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::time::Duration;

use teseladb::engine::core::expiring_fifo::{ExpiringFifo, OnExpiry};
use teseladb::engine::core::future::{ExceptionPtr, make_ready_future};
use teseladb::engine::core::manual_clock::ManualClock;
use teseladb::engine::core::reactor::yield_;
use teseladb::engine::core::thread::async_;
use teseladb::engine_test_case;

engine_test_case!(test_no_expiry_operations, {
    let mut fifo = ExpiringFifo::<i32>::default();

    assert!(fifo.empty());
    assert_eq!(fifo.size(), 0usize);
    assert!(!fifo.is_some());

    fifo.push_back(1);

    assert!(!fifo.empty());
    assert_eq!(fifo.size(), 1usize);
    assert!(fifo.is_some());
    assert_eq!(*fifo.front(), 1);

    fifo.push_back(2);
    fifo.push_back(3);

    assert!(!fifo.empty());
    assert_eq!(fifo.size(), 3usize);
    assert!(fifo.is_some());
    assert_eq!(*fifo.front(), 1);

    fifo.pop_front();

    assert!(!fifo.empty());
    assert_eq!(fifo.size(), 2usize);
    assert!(fifo.is_some());
    assert_eq!(*fifo.front(), 2);

    fifo.pop_front();

    assert!(!fifo.empty());
    assert_eq!(fifo.size(), 1usize);
    assert!(fifo.is_some());
    assert_eq!(*fifo.front(), 3);

    fifo.pop_front();

    assert!(fifo.empty());
    assert_eq!(fifo.size(), 0usize);
    assert!(!fifo.is_some());

    make_ready_future(())
});

/// An expiry callback that records the value of each expired entry in a vector.
struct MyExpiry {
    e: *mut Vec<i32>,
}

impl OnExpiry<i32> for MyExpiry {
    fn on_expiry(&mut self, v: &mut i32) {
        // SAFETY: the vector outlives the fifo in the test.
        unsafe { (*self.e).push(*v) };
    }
}

engine_test_case!(test_expiry_operations, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut expired: Vec<i32> = Vec::new();
        let ep: *mut Vec<i32> = &mut expired;
        let mut fifo = ExpiringFifo::<i32, MyExpiry, ManualClock>::new(MyExpiry { e: ep });
        let ex = |expired: *mut Vec<i32>| -> &'static mut Vec<i32> {
            // SAFETY: `expired` lives until the end of the thread function.
            unsafe { &mut *expired }
        };
        let s1 = Duration::from_secs(1);
        let s2 = Duration::from_secs(2);

        // SAFETY: the fifo stays in place in this thread until the end of the test, after its entries.

        unsafe { fifo.push_back_with_timeout(1, ManualClock::now() + s1) };

        assert!(!fifo.empty());
        assert_eq!(fifo.size(), 1usize);
        assert!(fifo.is_some());
        assert_eq!(*fifo.front(), 1);

        ManualClock::advance(s1);
        yield_().get()?;

        assert!(fifo.empty());
        assert_eq!(fifo.size(), 0usize);
        assert!(!fifo.is_some());
        assert_eq!(ex(ep).len(), 1usize);
        assert_eq!(ex(ep)[0], 1);

        ex(ep).clear();
        fifo.push_back(1);
        // SAFETY: the fifo stays in place in this thread until the end of the test, after its entries.
        unsafe { fifo.push_back_with_timeout(2, ManualClock::now() + s1) };
        fifo.push_back(3);

        ManualClock::advance(s1);
        yield_().get()?;

        assert!(!fifo.empty());
        assert_eq!(fifo.size(), 2usize);
        assert!(fifo.is_some());
        assert_eq!(ex(ep).len(), 1usize);
        assert_eq!(ex(ep)[0], 2);
        assert_eq!(*fifo.front(), 1);
        fifo.pop_front();
        assert_eq!(fifo.size(), 1usize);
        assert_eq!(*fifo.front(), 3);
        fifo.pop_front();
        assert_eq!(fifo.size(), 0usize);

        ex(ep).clear();
        // SAFETY: the fifo stays in place in this thread until the end of the test, after its entries.
        unsafe { fifo.push_back_with_timeout(1, ManualClock::now() + s1) };
        // SAFETY: the fifo stays in place in this thread until the end of the test, after its entries.
        unsafe { fifo.push_back_with_timeout(2, ManualClock::now() + s1) };
        fifo.push_back(3);
        // SAFETY: the fifo stays in place in this thread until the end of the test, after its entries.
        unsafe { fifo.push_back_with_timeout(4, ManualClock::now() + s2) };

        ManualClock::advance(s1);
        yield_().get()?;

        assert!(!fifo.empty());
        assert_eq!(fifo.size(), 2usize);
        assert!(fifo.is_some());
        assert_eq!(ex(ep).len(), 2usize);
        ex(ep).sort();
        assert_eq!(ex(ep)[0], 1);
        assert_eq!(ex(ep)[1], 2);
        assert_eq!(*fifo.front(), 3);
        fifo.pop_front();
        assert_eq!(fifo.size(), 1usize);
        assert_eq!(*fifo.front(), 4);
        fifo.pop_front();
        assert_eq!(fifo.size(), 0usize);

        ex(ep).clear();
        fifo.push_back(1);
        // SAFETY: the fifo stays in place in this thread until the end of the test, after its entries.
        unsafe { fifo.push_back_with_timeout(2, ManualClock::now() + s1) };
        // SAFETY: the fifo stays in place in this thread until the end of the test, after its entries.
        unsafe { fifo.push_back_with_timeout(3, ManualClock::now() + s1) };
        // SAFETY: the fifo stays in place in this thread until the end of the test, after its entries.
        unsafe { fifo.push_back_with_timeout(4, ManualClock::now() + s1) };

        ManualClock::advance(s1);
        yield_().get()?;

        assert!(!fifo.empty());
        assert_eq!(fifo.size(), 1usize);
        assert!(fifo.is_some());
        assert_eq!(ex(ep).len(), 3usize);
        ex(ep).sort();
        assert_eq!(ex(ep)[0], 2);
        assert_eq!(ex(ep)[1], 3);
        assert_eq!(ex(ep)[2], 4);
        assert_eq!(*fifo.front(), 1);
        fifo.pop_front();
        assert_eq!(fifo.size(), 0usize);

        ex(ep).clear();
        fifo.push_back(1);
        // SAFETY: the fifo stays in place in this thread until the end of the test, after its entries.
        unsafe { fifo.push_back_with_timeout(2, ManualClock::now() + s1) };
        // SAFETY: the fifo stays in place in this thread until the end of the test, after its entries.
        unsafe { fifo.push_back_with_timeout(3, ManualClock::now() + s1) };
        // SAFETY: the fifo stays in place in this thread until the end of the test, after its entries.
        unsafe { fifo.push_back_with_timeout(4, ManualClock::now() + s1) };
        fifo.push_back(5);

        ManualClock::advance(s1);
        yield_().get()?;

        assert_eq!(fifo.size(), 2usize);
        assert_eq!(*fifo.front(), 1);
        fifo.pop_front();
        assert_eq!(fifo.size(), 1usize);
        assert_eq!(*fifo.front(), 5);
        fifo.pop_front();
        assert_eq!(fifo.size(), 0usize);
        drop(fifo);
        Ok(())
    })
});
