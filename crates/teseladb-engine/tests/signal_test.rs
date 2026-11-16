//! Tests of signal handling: a handler registered for SIGHUP runs when the process sends
//! itself the signal.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::{Cell, RefCell};

use teseladb::engine::core::do_with::do_with;
use teseladb::engine::core::future::Promise;
use teseladb::engine::core::shared_ptr::make_lw_shared;
use teseladb::engine::core::signal::handle_signal;
use teseladb::engine::util::noncopyable_function::NoncopyableFunction;
use teseladb::engine_test_case;

const SIGHUP: i32 = 1;

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn getpid() -> i32;
}

engine_test_case!(test_sighup, {
    do_with(
        (make_lw_shared(RefCell::new(Promise::<()>::new())), Cell::new(false)),
        |(p, signaled)| {
            let signaled_ptr: *const Cell<bool> = signaled;
            let pc = p.clone();
            handle_signal(
                SIGHUP,
                NoncopyableFunction::<fn()>::new(move || {
                    // SAFETY: `signaled` lives in the do_with frame until the returned future resolves.
                    unsafe { (*signaled_ptr).set(true) };
                    pc.borrow_mut().set_value(());
                }),
                false,
            )
            .expect("handle_signal");

            // SAFETY: plain libc calls.
            unsafe { kill(getpid(), SIGHUP) };

            let fut = p.borrow_mut().get_future();
            fut.then(move |()| {
                // SAFETY: as above.
                assert!(unsafe { (*signaled_ptr).get() });
            })
        },
    )
});
