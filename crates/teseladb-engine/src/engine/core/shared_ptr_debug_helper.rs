//! A reference counter that detects use from the wrong core.
//!
//! Shared pointers in the engine are not thread-safe: each one belongs to the shard (OS thread)
//! that created it. When the crate is built with `--cfg engine_debug_shared_ptr`, the shared
//! pointers count references with [`DebugSharedPtrCounterType`], which aborts with an internal
//! error if any thread other than the creating one touches the count.

#![cfg(engine_debug_shared_ptr)]

use std::cell::Cell;
use std::thread::ThreadId;

use crate::engine::core::on_internal_error::on_fatal_internal_error;
use crate::engine::core::reactor::engine_logger;

/// Reference counter that checks it is only used on the OS thread that created it.
#[derive(Debug)]
pub struct DebugSharedPtrCounterType {
    counter: Cell<i64>,
    cpu: ThreadId,
}

impl DebugSharedPtrCounterType {
    /// A counter starting at `x`, owned by the calling thread.
    #[inline]
    pub fn new(x: i64) -> Self {
        Self { counter: Cell::new(x), cpu: std::thread::current().id() }
    }

    /// The current count.
    ///
    /// # Panics
    ///
    /// Aborts with a fatal internal error when called from a thread other than the owner.
    #[inline]
    pub fn get(&self) -> i64 {
        self.check();
        self.counter.get()
    }

    /// Increments the count; aborts like [`get`](Self::get) on the wrong thread.
    #[inline]
    pub fn inc(&self) {
        self.check();
        self.counter.set(self.counter.get() + 1);
    }

    /// Decrements the count and returns the new value; aborts like [`get`](Self::get) on the
    /// wrong thread.
    #[inline]
    pub fn dec(&self) -> i64 {
        self.check();
        let v = self.counter.get() - 1;
        self.counter.set(v);
        v
    }

    #[inline]
    fn check(&self) {
        if self.cpu != std::thread::current().id() {
            on_fatal_internal_error(engine_logger(), "shared_ptr accessed on non-owner cpu");
        }
    }
}
