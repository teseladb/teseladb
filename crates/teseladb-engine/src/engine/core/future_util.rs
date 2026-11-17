//! One import point for the future utilities, plus the state of `parallel_for_each` and the
//! abortable sleeps.
//!
//! The items of [`do_with`](crate::engine::core::do_with), [`loop_`](crate::engine::core::loop_),
//! [`map_reduce`](crate::engine::core::map_reduce), [`when_all`](crate::engine::core::when_all),
//! `with_scheduling_group`, [`with_timeout`](crate::engine::core::with_timeout) and `later`
//! are re-exported here. This module also defines [`ParallelForEachState`], which waits for
//! the futures of a parallel loop that were not ready, and [`sleep_abortable`] /
//! [`sleep_abortable_with`].

use std::pin::Pin;
use std::ptr::{self, NonNull};
use std::time::Duration;

use crate::engine::core::abort_source::{AbortSource, Subscription, SubscriptionCallback};
use crate::engine::core::condition_variable::ConditionVariableTimedOut;
use crate::engine::core::future::{
    ContinuationBase, ExceptionPtr, Future, FutureState, HostBox, HostedPromise, PromiseHost,
    internal::set_callback, make_exception_ptr,
};
use crate::engine::core::sleep::SleepAborted;
use crate::engine::core::task::{Task, TaskBase, TaskPtr};
use crate::engine::core::timer::{Timer, TimerClock};

pub use crate::engine::core::do_with::*;
pub use crate::engine::core::loop_::*;
pub use crate::engine::core::map_reduce::*;
pub use crate::engine::core::when_all::*;
pub use crate::engine::core::with_scheduling_group::*;
pub use crate::engine::core::with_timeout::*;
pub use crate::engine::util::later::*;

/// The `internal` modules of the re-exported modules, merged into one; the glob re-exports
/// above would otherwise leave the name ambiguous.
pub mod internal {
    pub use crate::engine::core::do_with::internal::*;
    pub use crate::engine::core::loop_::internal::*;
    pub use crate::engine::core::when_all::internal::*;
    pub use crate::engine::core::with_scheduling_group::internal::*;
}

/// The futures of `parallel_for_each` that were not ready, waited for one at a time.
///
/// One allocation holds the pending futures, the latest error seen, the continuation task and
/// the promise of the returned future.
#[repr(C)]
pub struct ParallelForEachState {
    // First field: the task header (see `Task`).
    task: TaskBase,
    state: FutureState<()>,
    incomplete: Vec<Future<()>>,
    result: HostedPromise<()>,
    ex: ExceptionPtr,
}

// SAFETY: hosted_promise returns the `result` field, derived from `this`.
unsafe impl PromiseHost for ParallelForEachState {
    type Value = ();
    #[inline]
    unsafe fn hosted_promise(this: *mut Self) -> *mut HostedPromise<()> {
        // SAFETY: guaranteed by the caller.
        unsafe { ptr::addr_of_mut!((*this).result) }
    }
}

// SAFETY: state_ptr returns the `state` field, derived from `this`.
unsafe impl ContinuationBase<()> for ParallelForEachState {
    #[inline]
    unsafe fn state_ptr(this: *mut Self) -> *mut FutureState<()> {
        // SAFETY: guaranteed by the caller.
        unsafe { ptr::addr_of_mut!((*this).state) }
    }
}

// SAFETY: ParallelForEachState is repr(C) with the header built by TaskBase::new::<Self> first.
unsafe impl Task for ParallelForEachState {
    /// Runs when the awaited future resolves: records its outcome and waits for the next one.
    unsafe fn run_and_dispose(this: NonNull<Self>) {
        let me = this.as_ptr();
        // SAFETY: the state is boxed (see new()) and owned by the task queue now; its
        // fields are reached through `me`, and wait_for_one may free it.
        unsafe {
            let st = ptr::addr_of_mut!((*me).state);
            if (*st).failed() {
                (*me).ex = (*st).get_exception();
            }
            *st = FutureState::new();
            Self::wait_for_one(me);
        }
    }
    #[inline]
    unsafe fn waiting_task(this: NonNull<Self>) -> Option<TaskPtr> {
        // SAFETY: the state is live.
        unsafe { HostedPromise::waiting_task(ptr::addr_of!((*this.as_ptr()).result)) }
    }
}

impl ParallelForEachState {
    /// Allocates the state with room for `n` pending futures.
    pub fn new(n: usize) -> *mut Self {
        HostBox::into_raw(HostBox::new(Self {
            task: TaskBase::new_current::<Self>(),
            state: FutureState::new(),
            incomplete: Vec::with_capacity(n),
            result: HostedPromise::new(),
            ex: ExceptionPtr::null(),
        }))
    }

    /// Starts waiting and returns the future of the whole loop, which fails with the last
    /// error recorded among the added futures. Takes ownership of the state.
    ///
    /// # Safety
    /// `this` must come from `new()` and not be used after the call.
    pub unsafe fn get_future(this: *mut Self) -> Future<()> {
        // SAFETY: guaranteed by the caller.
        unsafe {
            let ret = HostedPromise::get_future(ptr::addr_of_mut!((*this).result));
            Self::wait_for_one(this);
            ret
        }
    }

    /// Adds a pending future to wait for.
    ///
    /// # Safety
    /// `this` must come from `new()`, before `get_future()`.
    #[inline]
    pub unsafe fn add_future(this: *mut Self, f: Future<()>) {
        // SAFETY: guaranteed by the caller; the reference covers the vector only.
        unsafe { (*ptr::addr_of_mut!((*this).incomplete)).push(f) };
    }

    /// Waits for one of the pending futures to complete, and then decides what to do: wait
    /// for another one, or deliver the result if all are complete.
    ///
    /// # Safety
    /// `this` must be a live boxed state; it may be freed by the call.
    unsafe fn wait_for_one(this: *mut Self) {
        // SAFETY: guaranteed by the caller; every field is reached through `this`, so no
        // reference to the state is live when it is freed.
        unsafe {
            let incomplete = ptr::addr_of_mut!((*this).incomplete);
            // Process from back to front, on the assumption that the front
            // futures are likely to complete earlier than the back futures.
            // If that's indeed the case, then the front futures will be
            // available and we won't have to wait for them.

            // Skip over futures that happen to be complete already.
            while (*incomplete).last().is_some_and(Future::available) {
                if let Some(f) = (*incomplete).pop()
                    && f.failed()
                {
                    (*this).ex = f.get_exception();
                }
            }

            // If there's an incomplete future, wait for it.
            if let Some(f) = (*incomplete).pop() {
                // This future's state will be collected in run_and_dispose(), so we can drop it.
                set_callback(f, NonNull::new_unchecked(this));
                return;
            }

            // Everything completed, report a result.
            let result = ptr::addr_of_mut!((*this).result);
            if !(*this).ex.is_null() {
                let ex = std::mem::take(&mut (*this).ex);
                HostedPromise::set_exception(result, ex);
            } else {
                HostedPromise::set_value(result, ());
            }
            drop(HostBox::from_raw(this));
        }
    }
}
/// Returns a future which completes after `dur` has elapsed, or fails with [`SleepAborted`]
/// if the application is asked to stop first.
///
/// The wait is done by the reactor's stop condition, which measures `dur` on its own clock,
/// so `C` does not take part.
pub fn sleep_abortable<C: TimerClock>(dur: Duration) -> Future<()> {
    // SAFETY: called on a reactor thread; the reference to the reactor ends with the call,
    // which only queues a timer and returns a future.
    unsafe { (*crate::engine::core::reactor::engine()).wait_for_stop(dur) }
        .then(|()| -> Result<(), ExceptionPtr> { Err(make_exception_ptr(SleepAborted)) })
        .handle_exception(|ep: ExceptionPtr| -> Result<(), ExceptionPtr> {
            if ep.is::<ConditionVariableTimedOut>() {
                return Ok(());
            }
            Err(ep)
        })
}

/// The state of [`sleep_abortable_with`]. It is boxed, so its
/// timer and subscription are pinned in place.
struct Sleeper<C: TimerClock> {
    done: HostedPromise<()>,
    tmr: Timer<C>,
    sc: Subscription<SleeperAbort<C>>,
}

// SAFETY: hosted_promise returns the `done` field, derived from `this`.
unsafe impl<C: TimerClock> PromiseHost for Sleeper<C> {
    type Value = ();
    #[inline]
    unsafe fn hosted_promise(this: *mut Self) -> *mut HostedPromise<()> {
        // SAFETY: guaranteed by the caller.
        unsafe { ptr::addr_of_mut!((*this).done) }
    }
}

/// The subscription callback of a sleeper: cancels the timer and fails the sleep with the
/// abort error, or [`SleepAborted`] when the abort carries none.
struct SleeperAbort<C: TimerClock> {
    me: *mut Sleeper<C>,
}

impl<C: TimerClock> SubscriptionCallback for SleeperAbort<C> {
    fn call(&mut self, opt_ex: Option<&ExceptionPtr>) {
        // SAFETY: the subscription lives in the boxed sleeper, which outlives it.
        unsafe {
            let me = self.me;
            if Pin::new_unchecked(&mut (*me).tmr).cancel() {
                let ex = opt_ex.cloned().unwrap_or_else(|| make_exception_ptr(SleepAborted));
                HostedPromise::set_exception(ptr::addr_of_mut!((*me).done), ex);
            }
        }
    }
}

/// Returns a future which completes after `dur` has elapsed on clock `C`, or fails when `as_`
/// requests an abort first: with the abort's error, or [`SleepAborted`] if it carries none.
/// If an abort was already requested, the future fails at once, with the same error.
pub fn sleep_abortable_with<C: TimerClock>(
    dur: Duration,
    mut as_: Pin<&mut AbortSource>,
) -> Future<()> {
    // The one allocation: the promise node, the timer and the subscription are embedded in it.
    let s = HostBox::new(Sleeper::<C> {
        done: HostedPromise::new(),
        tmr: Timer::new(),
        sc: Subscription::default(),
    });
    let me: *mut Sleeper<C> = s.as_ptr();
    // SAFETY: the sleeper is boxed and kept alive by the continuation below until the
    // future resolves; its timer and subscription never move out of the box and are
    // dropped in place with it, so they may be pinned there. The references below cover
    // one field each.
    let fut = unsafe {
        let done = ptr::addr_of_mut!((*me).done);
        let mut tmr = Pin::new_unchecked(&mut *ptr::addr_of_mut!((*me).tmr));
        tmr.as_mut().set_callback(move || HostedPromise::set_value(done, ()));
        let sc = Pin::new_unchecked(&mut *ptr::addr_of_mut!((*me).sc));
        if as_.as_mut().subscribe(sc, SleeperAbort { me }) {
            tmr.arm_in(dur);
        } else {
            // Already aborted: fail as an abort during the sleep would, with the abort's own
            // error if it carried one.
            let ex = as_
                .abort_requested_custom_exception()
                .cloned()
                .unwrap_or_else(|| make_exception_ptr(SleepAborted));
            HostedPromise::set_exception(done, ex);
        }
        HostedPromise::get_future(done)
    };
    fut.finally(move || drop(s))
}
