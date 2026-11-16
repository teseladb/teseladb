//! Yielding to the reactor.
//!
//! Long-running work on a shard must give the reactor a chance to run other tasks and
//! poll for I/O. This module gathers the primitives for that: [`now`] (already ready),
//! [`yield_`] (resolve on a later task-queue pass), [`maybe_yield`] (yield only when the
//! task quota is exhausted) and [`check_for_io_immediately`] (force an I/O poll).

use crate::engine::core::future::{Future, make_ready_future};
use crate::engine::core::preempt::need_preempt;

/// Returns a ready future.
#[inline]
pub fn now() -> Future<()> {
    make_ready_future(())
}

/// Returns a future which is not ready but is scheduled to resolve soon.
///
/// Use it to break long-but-finite loops into pieces. If nothing else is runnable it
/// does not check for I/O, so an infinite loop of `yield_()` just burns CPU.
/// Implemented by the reactor.
pub use crate::engine::core::reactor::yield_;

/// Yields the CPU if the task quota is exhausted.
///
/// Checks whether the current continuation should be preempted and yields if so;
/// otherwise returns a ready future. Hot path: the check is one load of the preemption
/// flag.
///
/// Note: threads and coroutines have their own `maybe_yield` variants, and the
/// continuation-based loops (`do_for_each` and similar) yield automatically.
#[inline]
pub fn maybe_yield() -> Future<()> {
    if need_preempt() { yield_() } else { make_ready_future(()) }
}

/// Forces the reactor to check for pending I/O.
///
/// Schedules a check for new I/O completions (disk completions or network packet
/// arrival) immediately and returns a future that is ready when the I/O has been polled.
///
/// It is rarely needed; it is better to let the reactor schedule I/O polls itself. It has
/// no effect on I/O polling on other shards. Implemented by the reactor.
pub use crate::engine::core::reactor::check_for_io_immediately;

/// Returns a future which is not ready but is scheduled to resolve soon.
///
/// Deprecated: use [`yield_`] instead, or [`check_for_io_immediately`] if an I/O poll is
/// really needed. Implemented by the reactor.
pub use crate::engine::core::reactor::later;
