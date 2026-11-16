//! The task abstraction the reactor schedules.
//!
//! A task is an object whose first word is a pointer to its [`TaskVTable`], so a task pointer
//! is one machine word: every task is a `#[repr(C)]` struct whose first field is a
//! [`TaskBase`], which holds the vtable pointer next to the task's scheduling group. A
//! [`TaskPtr`] is a thin pointer to that header. The vtable functions take the task as a raw
//! pointer, never as a reference, so `run_and_dispose()` can free the task without a live
//! reference to the freed memory.
//!
//! Tasks are queued with [`schedule`], [`schedule_checked`] and [`schedule_urgent`] and run
//! once, on the shard that queued them.

use std::ptr::NonNull;

use crate::engine::core::scheduling::{SchedulingGroup, current_scheduling_group};
use crate::engine::util::backtrace::SharedBacktrace;

/// The vtable that [`TaskBase`] points to: the functions every task type provides.
///
/// Every function receives the task as a [`TaskPtr`] to its header. The pointer carries the
/// provenance of the whole task object (it is derived from the pointer to the object, see
/// [`TaskPtr::from_task`]), so each function casts it back to its concrete type.
pub struct TaskVTable {
    /// Runs the task and disposes of it.
    pub run_and_dispose: unsafe fn(TaskPtr),
    /// Returns the task waiting for this one, if any.
    pub waiting_task: unsafe fn(TaskPtr) -> Option<TaskPtr>,
    /// The concrete type name, for the task traces and the queue dumps of the reactor.
    pub type_name: fn() -> &'static str,
}

impl TaskVTable {
    /// The vtable of the task type `T`.
    #[inline]
    pub const fn of<T: Task>() -> Self {
        Self {
            run_and_dispose: run_and_dispose_thunk::<T>,
            waiting_task: waiting_task_thunk::<T>,
            type_name: type_name_of::<T>,
        }
    }
}

/// Calls `T::run_and_dispose` on the task behind `t`.
///
/// # Safety
/// `t` must be the header of a live `T`, derived from a pointer to the whole object.
unsafe fn run_and_dispose_thunk<T: Task>(t: TaskPtr) {
    // SAFETY: guaranteed by the caller; the header is the first field of the repr(C) `T`,
    // so the header pointer is the object pointer.
    unsafe { T::run_and_dispose(t.0.cast::<T>()) }
}

/// Calls `T::waiting_task` on the task behind `t`.
///
/// # Safety
/// As [`run_and_dispose_thunk`].
unsafe fn waiting_task_thunk<T: Task>(t: TaskPtr) -> Option<TaskPtr> {
    // SAFETY: as in run_and_dispose_thunk.
    unsafe { T::waiting_task(t.0.cast::<T>()) }
}

fn type_name_of<T: Task>() -> &'static str {
    std::any::type_name::<T>()
}

/// A thin pointer to the [`TaskBase`] header at the start of a task object.
///
/// It is one machine word, so the run queues of the reactor, the promise's waiting task and
/// `waiting_task()` hold and copy a single word.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TaskPtr(NonNull<TaskBase>);

impl TaskPtr {
    /// The task pointer of a concrete task.
    ///
    /// `t` should be derived from the pointer to the whole object (`Box::into_raw`,
    /// `ptr::addr_of_mut!` on the owner), not from a reference to its header, so that the
    /// vtable functions can reach every field.
    #[inline]
    pub fn from_task<T: Task>(t: NonNull<T>) -> Self {
        TaskPtr(t.cast::<TaskBase>())
    }

    /// Rebuilds a task pointer from the raw header pointer.
    ///
    /// # Safety
    /// `p` must be non-null and point to the header of a live task.
    #[inline]
    pub unsafe fn from_raw(p: *mut TaskBase) -> Self {
        // SAFETY: guaranteed by the caller.
        TaskPtr(unsafe { NonNull::new_unchecked(p) })
    }

    /// The raw pointer to the header.
    #[inline]
    pub fn as_ptr(self) -> *mut TaskBase {
        self.0.as_ptr()
    }

    /// Runs the task. The task may free itself; `self` must not be used after.
    ///
    /// # Safety
    /// The task must be live and handed to the reactor exactly once.
    #[inline]
    pub unsafe fn run_and_dispose(self) {
        // SAFETY: guaranteed by the caller; the vtable pointer is set at construction.
        unsafe { ((*self.0.as_ptr()).vtable.run_and_dispose)(self) }
    }

    /// Returns the next task which is waiting for this task to complete execution, or `None`.
    ///
    /// # Safety
    /// The task must be live.
    #[inline]
    pub unsafe fn waiting_task(self) -> Option<TaskPtr> {
        // SAFETY: guaranteed by the caller.
        unsafe { ((*self.0.as_ptr()).vtable.waiting_task)(self) }
    }

    /// The scheduling group the task runs in.
    ///
    /// # Safety
    /// The task must be live.
    #[inline]
    pub unsafe fn group(self) -> SchedulingGroup {
        // SAFETY: guaranteed by the caller.
        unsafe { (*self.0.as_ptr()).sg }
    }

    /// The name of the concrete task type.
    ///
    /// # Safety
    /// The task must be live.
    #[inline]
    pub unsafe fn type_name(self) -> &'static str {
        // SAFETY: guaranteed by the caller.
        unsafe { ((*self.0.as_ptr()).vtable.type_name)() }
    }

    /// The backtrace captured when the task was created (empty unless task backtraces are
    /// enabled with `cfg(engine_task_backtrace)`).
    ///
    /// # Safety
    /// The task must be live.
    #[inline]
    pub unsafe fn get_backtrace(self) -> SharedBacktrace {
        // SAFETY: guaranteed by the caller.
        unsafe { (*self.0.as_ptr()).get_backtrace() }
    }
}

/// The header every task starts with.
///
/// It holds the vtable pointer and the scheduling group. A task type is a `#[repr(C)]` struct
/// whose first field is a `TaskBase` built with [`TaskBase::new::<Self>`](TaskBase::new), and
/// which implements [`Task`].
#[repr(C)]
pub struct TaskBase {
    vtable: &'static TaskVTable,
    pub(crate) sg: SchedulingGroup,
    #[cfg(engine_task_backtrace)]
    pub(crate) bt: SharedBacktrace,
}

impl TaskBase {
    // Task destruction is performed by run_and_dispose() via the concrete type, so the header
    // needs no destructor hook. A type that implements run_and_dispose() is the complete
    // object, so no type information is lost.

    /// A header for the task type `T` (whose first field this header is), running in `sg`.
    #[inline]
    pub fn new<T: Task>(sg: SchedulingGroup) -> Self {
        Self {
            vtable: const { &TaskVTable::of::<T>() },
            sg,
            #[cfg(engine_task_backtrace)]
            bt: SharedBacktrace::default(),
        }
    }

    /// A header for the task type `T`, running in the current scheduling group.
    #[inline]
    pub fn new_current<T: Task>() -> Self {
        Self::new::<T>(current_scheduling_group())
    }

    /// Moves the task to `new_sg`; returns the previous group.
    #[inline]
    pub(crate) fn set_scheduling_group(&mut self, new_sg: SchedulingGroup) -> SchedulingGroup {
        std::mem::replace(&mut self.sg, new_sg)
    }

    /// The scheduling group the task runs in.
    #[inline]
    pub fn group(&self) -> SchedulingGroup {
        self.sg
    }

    /// The name of the concrete task type, for the task traces and queue dumps.
    #[inline]
    pub fn type_name(&self) -> &'static str {
        (self.vtable.type_name)()
    }

    /// The backtrace captured when the task was created (empty unless task backtraces are
    /// enabled with `cfg(engine_task_backtrace)`).
    #[inline]
    pub fn get_backtrace(&self) -> SharedBacktrace {
        #[cfg(engine_task_backtrace)]
        {
            self.bt.clone()
        }
        #[cfg(not(engine_task_backtrace))]
        {
            SharedBacktrace::default()
        }
    }

    /// Records the creation backtrace; a no-op when task backtraces are disabled (the enabled
    /// version lives in the reactor module).
    #[cfg(not(engine_task_backtrace))]
    #[inline]
    pub fn make_backtrace(&mut self) {}
}

/// The functions of a concrete task type.
///
/// # Safety
/// The implementor must be `#[repr(C)]` with a [`TaskBase`] built by
/// `TaskBase::new::<Self>` as its first field, so that a [`TaskPtr`] to the header is a
/// pointer to the object.
pub unsafe trait Task: Sized + 'static {
    /// Runs the task and disposes of it.
    /// call the task must not be touched again by the caller.
    ///
    /// # Safety
    /// `this` must point to a live task, derived from a pointer to the whole object, that
    /// has been handed to the reactor exactly once; no reference to it may be live.
    unsafe fn run_and_dispose(this: NonNull<Self>);

    /// Returns the next task which is waiting for this task to complete execution, or `None`.
    ///
    /// # Safety
    /// `this` must point to a live task; no mutable reference to it may be live.
    unsafe fn waiting_task(this: NonNull<Self>) -> Option<TaskPtr>;
}

/// Erases a concrete task pointer into a [`TaskPtr`] (see [`TaskPtr::from_task`]).
#[inline]
pub fn task_ptr<T: Task>(t: NonNull<T>) -> TaskPtr {
    TaskPtr::from_task(t)
}

/// Queue a task on the current shard: [`schedule`] at the back of its scheduling group's queue,
/// [`schedule_urgent`] at the front, and [`schedule_checked`] like `schedule` after
/// rejecting (as an internal error) a task in the at-exit scheduling group.
pub use crate::engine::core::reactor::{schedule, schedule_checked, schedule_urgent};
