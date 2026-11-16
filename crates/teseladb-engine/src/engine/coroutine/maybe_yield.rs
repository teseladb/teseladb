//! Letting the reactor preempt a long computation in a coroutine.
//!
//! Awaiting [`MaybeYield`] continues at once if the task quota has not expired; otherwise the
//! first poll schedules the coroutine's task at the back of the run queue and returns
//! `Pending`, and the next poll, when the reactor runs the task again, returns `Ready`.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::engine::core::coroutine::internal::{CoroutineCore, current_coroutine};
use crate::engine::core::preempt::need_preempt;
use crate::engine::core::task::schedule;

/// The awaiter behind [`MaybeYield`].
pub mod internal {
    use super::*;

    /// The awaiter of [`MaybeYield`].
    #[derive(Default)]
    pub struct MaybeYieldAwaiter {
        suspended: bool,
    }

    impl MaybeYieldAwaiter {
        /// Returns whether the coroutine can continue without yielding: no preemption is
        /// requested.
        #[inline]
        pub fn await_ready(&self) -> bool {
            !need_preempt()
        }
    }

    impl Future for MaybeYieldAwaiter {
        type Output = ();

        #[inline]
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.suspended || self.await_ready() {
                // Resumed, or no need to yield.
                return Poll::Ready(());
            }
            // Yield: put the coroutine's task back in the run queue.
            self.suspended = true;
            // SAFETY: the waker's coroutine is the live coroutine running this poll; its task
            // goes to the run queue.
            schedule(unsafe { CoroutineCore::hand_out_task(current_coroutine(cx)) });
            Poll::Pending
        }
    }
}

/// Preempt if the current task quota expired.
///
/// `maybe_yield()` can be used to break a long computation in a
/// coroutine and allow the reactor to preempt its execution. This
/// allows other tasks to gain access to the CPU. If the task quota
/// did not expire, the coroutine continues execution.
///
/// It should be used in long loops that do not contain other `.await`s.
///
/// Example
///
/// ```no_run
/// # use teseladb::engine::core::coroutine::make_coroutine;
/// # use teseladb::engine::core::future::Future;
/// # use teseladb::engine::coroutine::maybe_yield::MaybeYield;
/// fn long_loop(n: i32) -> Future<f32> {
///     make_coroutine(async move {
///         let mut acc = 0.0f32;
///         for i in 0..n {
///             acc += (i as f32).sin();
///             MaybeYield.await;
///         }
///         Ok(acc)
///     })
/// }
/// ```
#[must_use = "a MaybeYield does nothing unless awaited"]
#[derive(Clone, Copy, Default)]
pub struct MaybeYield;

impl std::future::IntoFuture for MaybeYield {
    type Output = ();
    type IntoFuture = internal::MaybeYieldAwaiter;

    /// Returns the awaiter.
    #[inline]
    fn into_future(self) -> internal::MaybeYieldAwaiter {
        internal::MaybeYieldAwaiter::default()
    }
}

/// Returns a [`MaybeYield`] to await.
#[inline]
pub fn maybe_yield() -> MaybeYield {
    MaybeYield
}
