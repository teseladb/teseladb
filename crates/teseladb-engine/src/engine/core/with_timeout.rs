//! Waiting for a future with a deadline.
//!
//! [`with_timeout`] races a future against a timer on the current shard and fails the result
//! with [`TimedOutError`] if the deadline comes first; [`with_timeout_factory`] lets the caller
//! choose the error through a [`TimeoutExceptionFactory`]. Timing out does not cancel the
//! underlying work: the original future keeps running and its eventual result is discarded.

use std::pin::Pin;

use crate::engine::core::future::{
    ExceptionPtr, Future, HostBox, HostedPromise, PromiseHost, make_exception_ptr,
};
use crate::engine::core::timed_out_error::{DefaultTimeoutExceptionFactory, TimedOutError};
use crate::engine::core::timer::{Timer, TimerClock};

/// Builds the error that [`with_timeout_factory`] delivers when the deadline passes.
pub trait TimeoutExceptionFactory {
    /// Returns the timeout error, as an exception pointer.
    fn timeout() -> ExceptionPtr;
}

impl TimeoutExceptionFactory for DefaultTimeoutExceptionFactory {
    #[inline]
    fn timeout() -> ExceptionPtr {
        make_exception_ptr(TimedOutError)
    }
}

/// Waits for either the future `f` or the deadline `timeout` of clock `C`, whichever comes
/// first.
///
/// If `f` is already available it is returned unchanged. Otherwise the result resolves with
/// the outcome of `f`, or fails with `E::timeout()` when the deadline passes first.
///
/// Timing out does not cancel the tasks behind `f`, nor the continuation attached to it; the
/// late result of `f` is ignored. One heap allocation holds the timer and the promise.
pub fn with_timeout_factory<E: TimeoutExceptionFactory, C: TimerClock, T: 'static>(
    timeout: C::TimePoint,
    f: Future<T>,
) -> Future<T> {
    if f.available() {
        return f;
    }
    // The timer is pinned once armed, so it shares the promise's allocation, which also
    // embeds the promise node; the continuation owns that allocation.
    let st = HostBox::new(TimeoutState::<T, C> { pr: HostedPromise::new(), timer: Timer::new() });
    let sp = st.as_ptr();
    // SAFETY: the state is boxed and owned by the continuation below, which cancels the
    // timer before dropping it; the boxed timer never moves, so it may be pinned. The
    // promise and the timer are reached through `sp`, one field at a time.
    let result = unsafe {
        let prp = std::ptr::addr_of_mut!((*sp).pr);
        let result = HostedPromise::get_future(prp);
        let mut timer = Pin::new_unchecked(&mut *std::ptr::addr_of_mut!((*sp).timer));
        timer.as_mut().set_callback(move || HostedPromise::set_exception(prp, E::timeout()));
        timer.arm(timeout, None);
        result
    };
    // Future is returned indirectly.
    let _ = f.then_wrapped(move |mut f: Future<T>| {
        let st = st;
        let sp = st.as_ptr();
        // SAFETY: as above.
        unsafe {
            if Pin::new_unchecked(&mut *std::ptr::addr_of_mut!((*sp).timer)).cancel() {
                if let Some(pr) = HostedPromise::take_writer(std::ptr::addr_of_mut!((*sp).pr)) {
                    f.forward_to_base(pr);
                }
            } else {
                f.ignore_ready_future();
            }
        }
        drop(st);
    });
    result
}

/// The promise and the timer of `with_timeout`, in one allocation.
struct TimeoutState<T: 'static, C: TimerClock> {
    pr: HostedPromise<T>,
    timer: Timer<C>,
}

// SAFETY: hosted_promise returns the `pr` field, derived from `this`.
unsafe impl<T: 'static, C: TimerClock> PromiseHost for TimeoutState<T, C> {
    type Value = T;
    #[inline]
    unsafe fn hosted_promise(this: *mut Self) -> *mut HostedPromise<T> {
        // SAFETY: guaranteed by the caller.
        unsafe { std::ptr::addr_of_mut!((*this).pr) }
    }
}

/// [`with_timeout_factory`] with the default factory: the result fails with [`TimedOutError`]
/// when the deadline passes first.
#[inline]
pub fn with_timeout<C: TimerClock, T: 'static>(timeout: C::TimePoint, f: Future<T>) -> Future<T> {
    with_timeout_factory::<DefaultTimeoutExceptionFactory, C, T>(timeout, f)
}
