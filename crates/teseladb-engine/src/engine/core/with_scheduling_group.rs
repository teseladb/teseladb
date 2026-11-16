//! Running a function under a given scheduling group.
//!
//! A [`SchedulingGroup`] is a share of the shard's CPU time. [`with_scheduling_group`] runs a
//! callable so that the work it does, and the continuations of the future it returns, are accounted
//! to that group. If the group is already the active one the function runs at once; otherwise it is
//! wrapped in a task queued on the group, and runs when the reactor next schedules that group.

use std::ptr::NonNull;

use crate::engine::core::future::{Futurize, FuturizeT};
use crate::engine::core::make_task::{LambdaTask, make_task};
use crate::engine::core::scheduling::SchedulingGroup;
use crate::engine::core::task::{TaskPtr, schedule_checked};

/// Helper used by [`with_scheduling_group`](super::with_scheduling_group).
pub mod internal {
    use super::*;

    /// Queues `func` as a new task in scheduling group `sg` and returns the future of its result.
    ///
    /// The function never runs inline, even if `sg` is the active group. Allocates one task.
    pub fn schedule_in_group<F, R>(sg: SchedulingGroup, func: F) -> FuturizeT<R>
    where
        F: FnOnce() -> R + 'static,
        R: Futurize,
    {
        let tsk = make_task(sg, func);
        // SAFETY: make_task returns a non-null pointer to a heap task that nothing else owns yet;
        // the queue takes it, and the task frees itself after it runs. The future is taken from the
        // task before it can run, since running only happens on a later reactor poll of this shard.
        unsafe {
            schedule_checked(TaskPtr::from_task(NonNull::new_unchecked(tsk)));
            LambdaTask::get_future(tsk)
        }
    }
}

/// Runs `func` in scheduling group `sg` and returns a future of its result.
///
/// If `sg` is the currently active group the function is called immediately, with no allocation;
/// otherwise it is queued as a task of `sg` and runs when that group is next scheduled. Arguments are
/// passed by capturing them in `func`. A function that returns a plain value has it wrapped in a
/// ready future; a function that returns a future has that future returned.
#[inline]
pub fn with_scheduling_group<F, R>(sg: SchedulingGroup, func: F) -> FuturizeT<R>
where
    F: FnOnce() -> R + 'static,
    R: Futurize,
{
    if sg.active() {
        R::invoke(func)
    } else {
        internal::schedule_in_group(sg, move || R::invoke(func))
    }
}
