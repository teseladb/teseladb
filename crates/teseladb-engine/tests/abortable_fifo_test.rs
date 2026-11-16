//! Tests of `AbortableFifo`: entries added with an abort source are removed, and handed to
//! the aborter, when that source fires; the others keep their order.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::pin::Pin;
use std::time::Duration;

use teseladb::engine::core::abort_on_expiry::AbortOnExpiry;
use teseladb::engine::core::abort_source::AbortSource;
use teseladb::engine::core::abortable_fifo::internal::{AbortableFifo, Aborter};
use teseladb::engine::core::future::{ExceptionPtr, make_ready_future};
use teseladb::engine::core::manual_clock::ManualClock;
use teseladb::engine::core::reactor::yield_;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::timed_out_error::TimedOutError;
use teseladb::engine_test_case;

engine_test_case!(test_no_abortable_operations, {
    let mut fifo = AbortableFifo::<i32>::default();

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

/// An aborter that records the value of each aborted entry in a vector.
struct MyExpiry {
    e: *mut Vec<i32>,
}

impl Aborter<i32> for MyExpiry {
    fn on_abort(&mut self, v: &mut i32, _ex: Option<&ExceptionPtr>) {
        // SAFETY: the vector outlives the fifo in the test.
        unsafe { (*self.e).push(*v) };
    }
}

fn new_as() -> Pin<Box<AbortSource>> {
    Box::pin(AbortSource::new())
}

engine_test_case!(test_abortable_operations, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut expired: Vec<i32> = Vec::new();
        let ep: *mut Vec<i32> = &mut expired;
        // SAFETY: `expired` lives until the end of the test function.
        let ex = || unsafe { &mut *ep };
        let mut fifo = AbortableFifo::<i32, MyExpiry>::new(MyExpiry { e: ep });
        let mut as_ = new_as();

        // SAFETY: the fifo stays in place, and outlives its entries, until the end of the test.
        unsafe {
            fifo.push_back_abortable(1, as_.as_mut());
        }

        assert!(!fifo.empty());
        assert_eq!(fifo.size(), 1usize);
        assert!(fifo.is_some());
        assert_eq!(*fifo.front(), 1);

        as_.as_mut().request_abort();
        yield_().get()?;

        assert!(fifo.empty());
        assert_eq!(fifo.size(), 0usize);
        assert!(!fifo.is_some());
        assert_eq!(ex().len(), 1usize);
        assert_eq!(ex()[0], 1);

        ex().clear();
        as_.set(AbortSource::new());
        fifo.push_back(1);
        // SAFETY: the fifo stays in place, and outlives its entries, until the end of the test.
        unsafe {
            fifo.push_back_abortable(2, as_.as_mut());
        }
        fifo.push_back(3);

        as_.as_mut().request_abort();
        yield_().get()?;

        assert!(!fifo.empty());
        assert_eq!(fifo.size(), 2usize);
        assert!(fifo.is_some());
        assert_eq!(ex().len(), 1usize);
        assert_eq!(ex()[0], 2);
        assert_eq!(*fifo.front(), 1);
        fifo.pop_front();
        assert_eq!(fifo.size(), 1usize);
        assert_eq!(*fifo.front(), 3);
        fifo.pop_front();
        assert_eq!(fifo.size(), 0usize);

        ex().clear();
        let mut as1 = new_as();
        let mut as2 = new_as();
        // SAFETY: the fifo stays in place, and outlives its entries, until the end of the test.
        unsafe {
            fifo.push_back_abortable(1, as1.as_mut());
        }
        // SAFETY: the fifo stays in place, and outlives its entries, until the end of the test.
        unsafe {
            fifo.push_back_abortable(2, as1.as_mut());
        }
        fifo.push_back(3);
        // SAFETY: the fifo stays in place, and outlives its entries, until the end of the test.
        unsafe {
            fifo.push_back_abortable(4, as2.as_mut());
        }

        as1.as_mut().request_abort();
        yield_().get()?;

        assert!(!fifo.empty());
        assert_eq!(fifo.size(), 2usize);
        assert!(fifo.is_some());
        assert_eq!(ex().len(), 2usize);
        ex().sort();
        assert_eq!(ex()[0], 1);
        assert_eq!(ex()[1], 2);
        assert_eq!(*fifo.front(), 3);
        fifo.pop_front();
        assert_eq!(fifo.size(), 1usize);
        assert_eq!(*fifo.front(), 4);
        fifo.pop_front();
        assert_eq!(fifo.size(), 0usize);

        ex().clear();
        as_.set(AbortSource::new());
        fifo.push_back(1);
        // SAFETY: the fifo stays in place, and outlives its entries, until the end of the test.
        unsafe {
            fifo.push_back_abortable(2, as_.as_mut());
        }
        // SAFETY: the fifo stays in place, and outlives its entries, until the end of the test.
        unsafe {
            fifo.push_back_abortable(3, as_.as_mut());
        }
        // SAFETY: the fifo stays in place, and outlives its entries, until the end of the test.
        unsafe {
            fifo.push_back_abortable(4, as_.as_mut());
        }

        as_.as_mut().request_abort();
        yield_().get()?;

        assert!(!fifo.empty());
        assert_eq!(fifo.size(), 1usize);
        assert!(fifo.is_some());
        assert_eq!(ex().len(), 3usize);
        ex().sort();
        assert_eq!(ex()[0], 2);
        assert_eq!(ex()[1], 3);
        assert_eq!(ex()[2], 4);
        assert_eq!(*fifo.front(), 1);
        fifo.pop_front();
        assert_eq!(fifo.size(), 0usize);

        ex().clear();
        as_.set(AbortSource::new());
        fifo.push_back(1);
        // SAFETY: the fifo stays in place, and outlives its entries, until the end of the test.
        unsafe {
            fifo.push_back_abortable(2, as_.as_mut());
        }
        // SAFETY: the fifo stays in place, and outlives its entries, until the end of the test.
        unsafe {
            fifo.push_back_abortable(3, as_.as_mut());
        }
        // SAFETY: the fifo stays in place, and outlives its entries, until the end of the test.
        unsafe {
            fifo.push_back_abortable(4, as_.as_mut());
        }
        fifo.push_back(5);

        as_.as_mut().request_abort();
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

struct Entry {
    value: i32,
    ex: ExceptionPtr,
}

/// An aborter that records each aborted entry together with the abort exception.
struct MyExpiryEx {
    e: *mut Vec<Entry>,
}

impl Aborter<i32> for MyExpiryEx {
    fn on_abort(&mut self, v: &mut i32, ex: Option<&ExceptionPtr>) {
        // SAFETY: the vector outlives the fifo in the test.
        unsafe { (*self.e).push(Entry { value: *v, ex: ex.cloned().unwrap_or_default() }) };
    }
}

engine_test_case!(
    /// The aborter receives the exception the abort source was fired with.
    test_abort_exception, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut expired: Vec<Entry> = Vec::new();
        let ep: *mut Vec<Entry> = &mut expired;
        let mut fifo = AbortableFifo::<i32, MyExpiryEx>::new(MyExpiryEx { e: ep });
        let mut aoe = std::pin::pin!(AbortOnExpiry::<ManualClock>::new(ManualClock::now() + Duration::from_secs(1)));

        // SAFETY: the fifo stays in place, and outlives its entries, until the end of the test.
        unsafe { fifo.push_back_abortable(1, aoe.as_mut().abort_source()); }

        assert!(!fifo.empty());
        assert_eq!(fifo.size(), 1usize);
        assert!(fifo.is_some());
        assert_eq!(*fifo.front(), 1);

        ManualClock::advance(Duration::from_secs(1));
        yield_().get()?;

        assert!(fifo.empty());
        assert_eq!(fifo.size(), 0usize);
        assert!(!fifo.is_some());
        drop(fifo);
        assert_eq!(expired.len(), 1usize);
        assert_eq!(expired[0].value, 1);
        assert!(!expired[0].ex.is_null());
        assert!(expired[0].ex.is::<TimedOutError>());
        Ok(())
    })
});
