//! Awaitable helpers for engine coroutines.
//!
//! An engine coroutine is a Rust `async` block run as a task of the current shard (see
//! `make_coroutine` in [`crate::engine::core::coroutine`]); its body returns
//! `Result<T, ExceptionPtr>`. The helpers here are awaited inside such bodies:
//! [`all`](all::All) waits for several futures at once, [`as_future`](as_future::as_future)
//! yields a future without unwrapping it, [`exception`] fails the coroutine with an error,
//! [`generator`] produces a stream of values, [`maybe_yield`](maybe_yield::maybe_yield) lets the
//! reactor preempt a long computation, [`parallel_for_each`] runs a function on many items
//! concurrently, and [`switch_to`] moves the coroutine to another scheduling group.

pub mod all;
pub mod as_future;
pub mod exception;
pub mod generator;
pub mod maybe_yield;
pub mod parallel_for_each;
pub mod switch_to;
