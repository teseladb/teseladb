//! Moving a coroutine to another scheduling group.
//!
//! Awaiting [`SwitchTo`] re-schedules the running coroutine's task under the new scheduling
//! group (or continues at once if it already runs there) and yields the previous group.

use std::future::Future;
use std::marker::PhantomPinned;
use std::pin::Pin;
use std::ptr::NonNull;
use std::task::{Context, Poll};

use crate::engine::core::coroutine::internal::{CoroutineCore, current_coroutine};
use crate::engine::core::scheduling::{SchedulingGroup, current_scheduling_group};
use crate::engine::core::task::{Task, TaskBase, TaskPtr, schedule};

/// Switches the current task's scheduling group.
///
/// `SwitchTo::new(new_scheduling_group).await` changes the scheduling group of the
/// currently running coroutine.
///
/// If the new scheduling group is different than the current scheduling_group,
/// the coroutine is re-scheduled using the new scheduling group.
/// Otherwise, the coroutine just continues to run with
/// the current scheduling group.
///
/// `switch_to` returns the current scheduling group
/// to make it easy to switch back to it if needed.
///
/// Example
///
/// ```no_run
/// # use teseladb::engine::core::coroutine::make_coroutine;
/// # use teseladb::engine::core::future::Future;
/// # use teseladb::engine::core::scheduling::SchedulingGroup;
/// # use teseladb::engine::coroutine::switch_to::SwitchTo;
/// fn work_in(other_sg: SchedulingGroup) -> Future<()> {
///     make_coroutine(async move {
///         // do some preliminary work
///         let prev_sg = SwitchTo::new(other_sg).await;
///         // do some work using another scheduling group
///         SwitchTo::new(prev_sg).await;
///         // do some more work
///         Ok(())
///     })
/// }
/// ```
///
/// The awaiter is itself a task (it is never run) so that a backtrace walking
/// `waiting_task()` links passes through it; it is pinned while awaited.
#[must_use = "a SwitchTo does nothing unless awaited"]
#[repr(C)]
pub struct SwitchTo {
    _base: TaskBase,
    _prev_sg: SchedulingGroup,
    _switch_to_sg: SchedulingGroup,
    _task: Option<TaskPtr>,
    _pin: PhantomPinned,
}

impl SwitchTo {
    /// Prepares a switch to `new_sg`.
    #[inline]
    pub fn new(new_sg: SchedulingGroup) -> Self {
        SwitchTo {
            _base: TaskBase::new_current::<Self>(),
            _prev_sg: current_scheduling_group(),
            _switch_to_sg: new_sg,
            _task: None,
            _pin: PhantomPinned,
        }
    }

    /// Returns whether no switch is needed: the coroutine already runs in the target group.
    #[inline]
    pub fn await_ready(&self) -> bool {
        current_scheduling_group() == self._switch_to_sg
    }
}

// SAFETY: SwitchTo is `#[repr(C)]` with its TaskBase, built by `TaskBase::new::<Self>`, first.
unsafe impl Task for SwitchTo {
    /// Never called: the awaiter's task is never scheduled itself.
    unsafe fn run_and_dispose(_this: NonNull<Self>) {}

    unsafe fn waiting_task(this: NonNull<Self>) -> Option<TaskPtr> {
        // SAFETY: the caller's contract: `this` points at a live awaiter.
        unsafe { (*this.as_ptr())._task }
    }
}

impl Future for SwitchTo {
    type Output = SchedulingGroup;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<SchedulingGroup> {
        // SAFETY: no field is moved out of the pinned awaiter.
        let this = unsafe { self.get_unchecked_mut() };
        if this._task.is_some() || this.await_ready() {
            // Resumed (or no switch needed): yield the previous group.
            return Poll::Ready(this._prev_sg);
        }
        // Suspend: move the coroutine's task to the new group and reschedule it.
        // SAFETY: the waker's coroutine is the live coroutine running this poll; its task is
        // handed to a promise or to the run queue right below.
        let t = unsafe { CoroutineCore::hand_out_task(current_coroutine(cx)) };
        // SAFETY: the coroutine's task is alive while the coroutine is suspended on us, and
        // nothing else uses its header during this call.
        unsafe { (*t.as_ptr()).set_scheduling_group(this._switch_to_sg) };
        this._task = Some(t);
        schedule(t);
        Poll::Pending
    }
}
