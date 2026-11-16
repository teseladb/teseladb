//! Awaiting a future without unwrapping its result.
//!
//! [`as_future`] waits for a future to resolve, successfully or with an error, and yields the
//! resolved future itself, so the coroutine can inspect a failure instead of propagating it.

use std::pin::Pin;
use std::task::{Context, Poll};

use crate::engine::core::coroutine::internal::Awaiter;
use crate::engine::core::future::Future;

/// The awaiter behind [`as_future`].
pub mod internal {
    use super::*;

    /// Awaits a future and yields the future itself (resolved or failed).
    ///
    /// It is the coroutine awaiter of the future with a different result: the promise writes
    /// into the pinned awaiter's state, and the result is returned as a ready future that
    /// owns it. The promise is not linked to the
    /// returned future, so the awaited coroutine or continuation is freed when it is done,
    /// not when the returned future is consumed.
    #[must_use]
    pub struct AsFutureAwaiter<const CHECK_PREEMPT: bool, T: 'static> {
        _future: Awaiter<CHECK_PREEMPT, T>,
    }

    impl<const CHECK_PREEMPT: bool, T: 'static> AsFutureAwaiter<CHECK_PREEMPT, T> {
        /// Wraps `f`.
        #[inline]
        pub fn new(f: Future<T>) -> Self {
            AsFutureAwaiter { _future: Awaiter::new(f) }
        }

        /// Returns whether the future can be returned without suspending: it is available
        /// and, with `CHECK_PREEMPT`, no preemption is requested.
        #[inline]
        pub fn await_ready(&self) -> bool {
            self._future.await_ready()
        }
    }

    impl<const CHECK_PREEMPT: bool, T: 'static> std::future::Future
        for AsFutureAwaiter<CHECK_PREEMPT, T>
    {
        type Output = Future<T>;

        /// Polls as the coroutine awaiter of the future does, except that the resolved
        /// future is returned instead of its value.
        #[inline(always)]
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Future<T>> {
            // SAFETY: the inner awaiter is pinned with this one (structural pinning: it is
            // never moved out, and this type has no Drop of its own).
            let inner = unsafe { self.map_unchecked_mut(|a| &mut a._future) };
            inner.poll_with(cx, |a| Future::from_state(a.resume_state()))
        }
    }
}
/// Awaits a [`Future`], returning it as the result.
///
/// Similar to `Future::then_wrapped`, [`as_future`] waits for the future to resolve either to
/// a ready future or to a failed one. It then returns it as the result of the `.await`.
///
/// For example:
/// ```no_run
/// # use teseladb::engine::core::coroutine::make_coroutine;
/// # use teseladb::engine::core::future::Future;
/// # use teseladb::engine::coroutine::as_future::as_future;
/// fn did_future_fail(fut: Future<()>) -> Future<bool> {
///     make_coroutine(async move {
///         let mut f = as_future(fut).await;
///         if f.failed() {
///             eprintln!("Future failed: {}", f.get_exception());
///             Ok(true)
///         } else {
///             Ok(false)
///         }
///     })
/// }
/// ```
///
/// Note that by default, `as_future` checks whether the task quota is depleted,
/// which means that it will yield if the future is ready and `need_preempt()`
/// returns true. Use [`as_future_without_preemption_check`] to disable preemption checking.
pub type AsFuture<T> = internal::AsFutureAwaiter<true, T>;

/// Builds an [`AsFuture`] awaiter for `f`.
#[inline]
pub fn as_future<T: 'static>(f: Future<T>) -> AsFuture<T> {
    AsFuture::new(f)
}

/// Awaits a [`Future`], returning it as the result, without checking if preemption is needed.
///
/// Like [`as_future`], awaiting it returns the input future as the result.
/// However, it bypasses checking if the task quota is depleted, which means that
/// a ready `future` will be handled immediately.
pub type AsFutureWithoutPreemptionCheck<T> = internal::AsFutureAwaiter<false, T>;

/// Builds an [`AsFutureWithoutPreemptionCheck`] awaiter for `f`.
#[inline]
pub fn as_future_without_preemption_check<T: 'static>(
    f: Future<T>,
) -> AsFutureWithoutPreemptionCheck<T> {
    AsFutureWithoutPreemptionCheck::new(f)
}
