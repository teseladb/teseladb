//! [`StopSignal`], a helper that lets a test application wait for SIGINT or SIGTERM.

#![allow(dead_code)]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::pin::Pin;
use teseladb::engine::core::condition_variable::ConditionVariable;
use teseladb::engine::core::future::Future;
use teseladb::engine::core::signal::handle_signal;
use teseladb::engine::util::noncopyable_function::NoncopyableFunction;

const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

/// Futurized SIGINT/SIGTERM signals handler.
///
/// A helper that allows easy waiting for SIGINT/SIGTERM signals from an application.
/// from your app.
///
/// Example:
/// ```ignore
/// let th = teseladb::engine::core::thread::async_(|| {
///    let stop_signal = StopSignal::new();
///    // <some code>
///    stop_signal.wait().get();  // this will wait till we receive SIGINT or SIGTERM signal
/// });
/// ```
///
/// The handlers point to the object, so it lives in a box that does not move.
pub struct StopSignal {
    caught: bool,
    cond: ConditionVariable,
}

impl StopSignal {
    fn signaled(&mut self) {
        if self.caught {
            return;
        }
        self.caught = true;
        self.cond.broadcast();
    }

    /// Registers the SIGINT and SIGTERM handlers and returns the boxed, unmoving handler.
    pub fn new() -> Box<StopSignal> {
        let mut s = Box::new(StopSignal { caught: false, cond: ConditionVariable::new() });
        let this: *mut StopSignal = &mut *s;
        for signo in [SIGINT, SIGTERM] {
            handle_signal(
                signo,
                NoncopyableFunction::<fn()>::new(move || {
                    // SAFETY: the handlers are replaced by no-ops in drop(), before the box is freed.
                    unsafe { (*this).signaled() }
                }),
                false,
            )
            .expect("handle_signal");
        }
        s
    }

    /// Resolves once SIGINT or SIGTERM has been caught.
    pub fn wait(&mut self) -> Future<()> {
        let this: *const StopSignal = self;
        // SAFETY: the stop signal lives in the box `new()` returns and never moves, so its
        // condition variable stays pinned; the object outlives the wait (it is waited for
        // before it is dropped), which is the contract of `wait_pred` and what makes reading
        // `caught` through `this` valid.
        unsafe { Pin::new_unchecked(&self.cond).wait_pred(move || (*this).caught) }
    }

    /// Whether a signal has been caught.
    pub fn stopping(&self) -> bool {
        self.caught
    }
}

impl Drop for StopSignal {
    fn drop(&mut self) {
        // There's no way to unregister a handler yet, so register a no-op handler instead.
        let _ = handle_signal(SIGINT, NoncopyableFunction::<fn()>::new(|| {}), false);
        let _ = handle_signal(SIGTERM, NoncopyableFunction::<fn()>::new(|| {}), false);
    }
}
