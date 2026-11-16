//! Timed sleeps on the current shard.
//!
//! [`sleep`] returns a future that resolves after at least the given duration of a
//! [`TimerClock`]; [`sleep_steady`] uses the steady clock. The abortable variants,
//! [`sleep_abortable`] and [`sleep_abortable_with`], also fail early with [`SleepAborted`] when
//! the application stops or an abort source fires. Each sleep costs one heap allocation that
//! holds both the timer and the promise.

use crate::engine::core::abort_source::AbortRequestedException;
use std::pin::Pin;

use crate::engine::core::future::{Future, HostBox, HostedPromise, PromiseHost};
use crate::engine::core::timer::{SteadyClockType, Timer, TimerClock};

/// Returns a future which completes after `dur` has elapsed on clock `C`.
///
/// `dur` is the minimum amount of time before the returned future becomes ready. The future
/// never fails.
pub fn sleep<C: TimerClock>(dur: std::time::Duration) -> Future<()> {
    // One allocation holds the promise node and the timer. The fields are declared so that
    // the timer is dropped (and so cancelled) before the promise.
    struct Sleeper<C: TimerClock> {
        tmr: Timer<C>,
        done: HostedPromise<()>,
    }
    // SAFETY: the promise is a field of the sleeper, reached from its pointer.
    unsafe impl<C: TimerClock> PromiseHost for Sleeper<C> {
        type Value = ();
        #[inline]
        unsafe fn hosted_promise(this: *mut Self) -> *mut HostedPromise<()> {
            // SAFETY: guaranteed by the caller.
            unsafe { std::ptr::addr_of_mut!((*this).done) }
        }
    }
    let s = HostBox::new(Sleeper::<C> { tmr: Timer::new(), done: HostedPromise::new() });
    let me = s.as_ptr();
    // SAFETY: the sleeper is heap-allocated and kept alive by the continuation below until
    // the future resolves; its timer never moves out of it and is dropped in place (which
    // cancels it), so it may be pinned there. The promise and the timer are reached through
    // `me`, one field at a time, with no reference to the whole sleeper live.
    let fut = unsafe {
        let done = std::ptr::addr_of_mut!((*me).done);
        let fut = HostedPromise::get_future(done);
        let mut tmr = Pin::new_unchecked(&mut *std::ptr::addr_of_mut!((*me).tmr));
        tmr.as_mut().set_callback(move || HostedPromise::set_value(done, ()));
        tmr.arm_in(dur);
        fut
    };
    // The continuation owns the sleeper and frees it once the timer has fired.
    fut.then(move |()| drop(s))
}

/// [`sleep`] on the steady clock ([`SteadyClockType`]).
#[inline]
pub fn sleep_steady(dur: std::time::Duration) -> Future<()> {
    sleep::<SteadyClockType>(dur)
}

/// Error delivered by an abortable sleep that was aborted, for example because the application
/// is stopping.
#[derive(Debug, Clone, Copy, Default)]
pub struct SleepAborted;

impl SleepAborted {
    /// Returns the error message, `"Sleep is aborted"`.
    pub fn what(&self) -> &'static str {
        "Sleep is aborted"
    }
}

impl std::fmt::Display for SleepAborted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.what())
    }
}

impl std::error::Error for SleepAborted {}

impl From<SleepAborted> for AbortRequestedException {
    fn from(_: SleepAborted) -> Self {
        AbortRequestedException
    }
}

/// Abortable sleeps, defined in [`future_util`](crate::engine::core::future_util).
///
/// `sleep_abortable(dur)` completes after `dur` has elapsed, or fails with [`SleepAborted`] if
/// the application is stopping. `sleep_abortable_with(dur, as)` completes after `dur`, or fails
/// when the abort source `as` requests an abort first, with the abort's error or
/// [`SleepAborted`] if it carries none.
pub use crate::engine::core::future_util::{sleep_abortable, sleep_abortable_with};
