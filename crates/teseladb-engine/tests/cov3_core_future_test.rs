//! The promise entry points that fail a future from a typed error or from the error being
//! handled (the original's `promise::set_exception(E&&)` and `set_to_current_exception()`), on
//! a plain promise, on its typed writer handle and on a promise hosted in a heap object, plus
//! the pinned access to such a host and `futurize::current_exception_as_future`.
#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use std::marker::PhantomPinned;
use std::pin::Pin;

use teseladb::engine::core::future::{
    ExceptionPtr, Futurize, HostBox, HostedPromise, Promise, PromiseHost, RuntimeError,
};

fn what<T: 'static>(f: teseladb::engine::core::future::Future<T>) -> String {
    match f.get() {
        Ok(_) => panic!("the future resolved"),
        Err(e) => e.get().map(|x| x.what()).unwrap_or_default(),
    }
}

#[test]
fn promises_fail_from_a_typed_error_or_the_current_one() {
    let mut p = Promise::<u32>::new();
    let f = p.get_future();
    p.set_exception_from(RuntimeError::new("typed"));
    assert_eq!(what(f), "typed");

    let mut p = Promise::<u32>::new();
    let f = p.get_future();
    p.set_to_current_exception(ExceptionPtr::new(RuntimeError::new("current")));
    assert_eq!(what(f), "current");
}

/// A heap object holding a promise, as the tasks and waiters do.
struct Waiter {
    tag: u32,
    pr: HostedPromise<u32>,
    _pin: PhantomPinned,
}

// SAFETY: `hosted_promise` returns the address of the `pr` field, derived from `this`.
unsafe impl PromiseHost for Waiter {
    type Value = u32;
    unsafe fn hosted_promise(this: *mut Self) -> *mut HostedPromise<u32> {
        // SAFETY: by the caller, `this` points to a live host.
        unsafe { std::ptr::addr_of_mut!((*this).pr) }
    }
}

fn waiter(tag: u32) -> HostBox<Waiter> {
    HostBox::new(Waiter { tag, pr: HostedPromise::default(), _pin: PhantomPinned })
}

#[test]
fn hosted_promises_fail_from_a_typed_error_or_the_current_one() {
    let mut w = waiter(1);
    // SAFETY: the host is live; the pointer is derived from it.
    let hp = unsafe { Waiter::hosted_promise(w.as_ptr()) };
    // SAFETY: the promise of the live host.
    let f = unsafe { HostedPromise::get_future(hp) };
    // SAFETY: as above.
    unsafe { HostedPromise::set_exception_from(hp, RuntimeError::new("hosted typed")) };
    assert_eq!(what(f), "hosted typed");
    // The pinned host can be changed in place.
    let pinned: Pin<&mut Waiter> = w.as_mut();
    // SAFETY: the tag is not structurally pinned.
    unsafe { pinned.get_unchecked_mut().tag = 2 };
    assert_eq!(w.tag, 2);

    let w = waiter(3);
    // SAFETY: the host is live; the pointer is derived from it.
    let hp = unsafe { Waiter::hosted_promise(w.as_ptr()) };
    // SAFETY: as above.
    let f = unsafe { HostedPromise::get_future(hp) };
    // SAFETY: as above.
    unsafe {
        HostedPromise::set_to_current_exception(hp, ExceptionPtr::new(RuntimeError::new("hc")))
    };
    assert_eq!(what(f), "hc");
    drop(w);
}

#[test]
fn writer_handles_fail_from_a_typed_error_or_the_current_one() {
    let w = waiter(4);
    // SAFETY: the host is live; the pointer is derived from it.
    let hp = unsafe { Waiter::hosted_promise(w.as_ptr()) };
    // SAFETY: the promise of the live host.
    let f = unsafe { HostedPromise::get_future(hp) };
    // SAFETY: as above.
    let mut writer = unsafe { HostedPromise::take_writer(hp) }.expect("a bound promise");
    assert!(writer.waiting_task().is_none());
    writer.set_exception_from(RuntimeError::new("writer typed"));
    assert_eq!(what(f), "writer typed");
    drop(w);

    let w = waiter(5);
    // SAFETY: the host is live; the pointer is derived from it.
    let hp = unsafe { Waiter::hosted_promise(w.as_ptr()) };
    // SAFETY: as above.
    let f = unsafe { HostedPromise::get_future(hp) };
    // SAFETY: as above.
    let mut writer = unsafe { HostedPromise::take_writer(hp) }.unwrap();
    writer.set_to_current_exception(ExceptionPtr::new(RuntimeError::new("writer current")));
    assert_eq!(what(f), "writer current");
    drop(w);
}

#[test]
fn current_exception_becomes_a_failed_future_of_the_converted_type() {
    let f = <Result<u8, ExceptionPtr> as Futurize>::current_exception_as_future(ExceptionPtr::new(
        RuntimeError::new("converted"),
    ));
    assert!(f.failed());
    assert_eq!(what(f), "converted");
}
