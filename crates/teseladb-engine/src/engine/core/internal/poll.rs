//! Pollers: non-blocking functions the reactor calls on every iteration of its main loop.
//!
//! A [`Pollfn`] checks one event source (I/O completions, timers, inter-shard queues...). A
//! [`Poller`] registers a pollfn with the current shard's reactor for as long as the poller
//! lives. Registration and deregistration go through tasks ([`RegistrationTask`],
//! [`DeregistrationTask`]) because the poller may be created or dropped from inside another
//! poller, while the reactor is iterating its poller list. Pollers also take part in the
//! sleep protocol: before the reactor sleeps, every pollfn must agree to enter "interrupt
//! mode", in which its events wake the reactor.

use std::ptr::NonNull;

use crate::engine::core::reactor::engine;
use crate::engine::core::task::{Task, TaskBase, TaskPtr, task_ptr};
use crate::engine::net::node_ptr::NodePtr;

/// A non-blocking function that the reactor calls on every iteration of its main loop.
pub trait Pollfn {
    /// Does the poller's work; returns `true` if work was done (`false` = idle).
    fn poll(&mut self) -> bool;
    /// Checks whether work needs to be done, without doing any; returns `true` if work is
    /// needed (`false` = idle).
    fn pure_poll(&mut self) -> bool;
    /// Tries to enter interrupt mode.
    ///
    /// If it returns `true`, events from this poller will wake a sleeping idle loop, and
    /// [`exit_interrupt_mode`](Self::exit_interrupt_mode) must be called to return to normal
    /// polling. If it returns `false`, the sleeping idle loop may not be entered.
    fn try_enter_interrupt_mode(&mut self) -> bool;
    /// Returns to normal polling after a successful
    /// [`try_enter_interrupt_mode`](Self::try_enter_interrupt_mode).
    fn exit_interrupt_mode(&mut self);
}

/// The common case of a poller: `poll()` and `pure_poll()` are the same, the poller always (or
/// never) agrees to go to sleep and does nothing on wake-up. Every `SimplePollfn` is a
/// [`Pollfn`].
pub trait SimplePollfn {
    /// Whether the poller always agrees to go to sleep (`true`) or never does (`false`).
    const PASSIVE: bool;
    /// Does the poller's work; returns `true` if work was done.
    fn poll(&mut self) -> bool;
}

impl<T: SimplePollfn> Pollfn for T {
    #[inline]
    fn poll(&mut self) -> bool {
        SimplePollfn::poll(self)
    }
    #[inline]
    fn pure_poll(&mut self) -> bool {
        SimplePollfn::poll(self)
    }
    #[inline]
    fn try_enter_interrupt_mode(&mut self) -> bool {
        T::PASSIVE
    }
    #[inline]
    fn exit_interrupt_mode(&mut self) {}
}

/// A [`SimplePollfn`] that calls a closure and never lets the reactor sleep.
pub struct FnPollfn<Func: FnMut() -> bool> {
    func: Func,
}

impl<Func: FnMut() -> bool> SimplePollfn for FnPollfn<Func> {
    const PASSIVE: bool = false;
    #[inline]
    fn poll(&mut self) -> bool {
        (self.func)()
    }
}

/// Wraps `func` in a boxed [`Pollfn`] that never lets the reactor sleep.
#[inline]
pub fn make_pollfn<Func: FnMut() -> bool + 'static>(func: Func) -> Box<dyn Pollfn> {
    Box::new(FnPollfn { func })
}

/// The allocation of a [`Poller`]: the pollfn with a pointer to its pending registration task
/// in front of it. The registration task points here, at a heap address, so the `Poller`
/// handle can move freely without re-pointing the task, in a single allocation.
///
/// The state lives in a [`NodePtr`], not a `Box`: the registration task and the reactor's
/// poller list keep pointers into it, and every one of them (and every access of the
/// `Poller` itself) derives from the node's root pointer, so none invalidates another.
pub struct PollerState<P: ?Sized + Pollfn> {
    _registration_task: *mut RegistrationTask,
    _pollfn: P,
}

/// Registers a [`Pollfn`] with the current shard's reactor for as long as it lives.
///
/// Registration takes effect when the registration task runs; dropping the poller before
/// that cancels it. After the drop the pollfn is never called again.
pub struct Poller {
    _state: NodePtr<PollerState<dyn Pollfn>>,
}

impl Poller {
    /// A poller that calls `poll` on every reactor iteration and never lets the reactor
    /// sleep.
    #[inline]
    pub fn simple<Func: FnMut() -> bool + 'static>(poll: Func) -> Poller {
        Poller::new(FnPollfn { func: poll })
    }

    /// Registers `fn_` with the current shard's reactor; the pollfn is placed in the poller's
    /// single allocation.
    pub fn new<P: Pollfn + 'static>(fn_: P) -> Poller {
        let state: Box<PollerState<dyn Pollfn>> =
            Box::new(PollerState { _registration_task: std::ptr::null_mut(), _pollfn: fn_ });
        // SAFETY: the pointer comes from `Box::into_raw` and the node is its only owner.
        let mut p = Poller { _state: unsafe { NodePtr::from_raw(Box::into_raw(state)) } };
        p.do_register();
        p
    }

    /// The registered pollfn, at the stable address the reactor calls.
    #[inline]
    pub fn pollfn_ptr(&self) -> *mut dyn Pollfn {
        // SAFETY: the state is live while the node is; a place projection, no reference.
        unsafe { std::ptr::addr_of_mut!((*self._state.as_ptr())._pollfn) }
    }

    /// Schedules the task that adds the pollfn to the reactor's poller list.
    pub fn do_register(&mut self) {
        // We can't just insert a poller into reactor::_pollers, because we
        // may be running inside a poller ourselves, and so in the middle of
        // iterating reactor::_pollers itself.  So we schedule a task to add
        // the poller instead.
        let state = self._state.as_ptr();
        let task = Box::into_raw(Box::new(RegistrationTask {
            base: TaskBase::new_current::<RegistrationTask>(),
            // SAFETY: `as_ptr` is never null.
            _p: Some(unsafe { NonNull::new_unchecked(state) }),
        }));
        // SAFETY: a fresh heap task, handed to this shard's reactor, which is reached for
        // this call only (see `engine()`); the state is written through its root pointer,
        // the one the task keeps.
        unsafe {
            (*engine()).add_task(task_ptr(NonNull::new_unchecked(task)));
            (*state)._registration_task = task;
        }
    }
}

impl Drop for Poller {
    /// Unregisters the pollfn: cancels a pending registration, or replaces the pollfn in the
    /// reactor's list by an idle one at once and schedules a task to remove that one.
    fn drop(&mut self) {
        // We can't just remove the poller from reactor::_pollers, because we
        // may be running inside a poller ourselves, and so in the middle of
        // iterating reactor::_pollers itself.  So we schedule a task to remove
        // the poller instead.
        //
        // Since we don't want to call the poller after we exit the destructor,
        // we replace it atomically with another one, and schedule a task to
        // delete the replacement.
        let pollfn_ptr = self.pollfn_ptr();
        // SAFETY: the state is live; one field read through its root pointer.
        let registration_task = unsafe { (*self._state.as_ptr())._registration_task };
        if !registration_task.is_null() {
            // not added yet, so don't do it at all.
            // SAFETY: the task is pending (it clears this pointer when it runs).
            unsafe { (*registration_task).cancel() };
        // SAFETY: on this shard's reactor thread, `engine()` is its live reactor; a short
        // field read with no reference kept.
        } else if !unsafe { (*engine())._finished_running_tasks } {
            // If _finished_running_tasks, the call to add_task() below will just
            // leak it, since no one will call task::run_and_dispose(). Just leave
            // the poller there, the reactor will never use it.
            // SAFETY: the pointer comes from `Box::into_raw` and the node is its only owner;
            // the task keeps the node and `_pollers` its root pointer.
            let dummy = unsafe { NodePtr::from_raw(Box::into_raw(make_pollfn(|| false))) };
            let dummy_p = dummy.as_ptr();
            let task = Box::into_raw(Box::new(DeregistrationTask {
                base: TaskBase::new_current::<DeregistrationTask>(),
                _p: dummy,
            }));
            // SAFETY: a fresh heap task, handed to this shard's reactor, which is reached for
            // this call only (see `engine()`).
            unsafe { (*engine()).add_task(task_ptr(NonNull::new_unchecked(task))) };
            // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
            unsafe { (*engine()).replace_poller(pollfn_ptr, dummy_p) };
        }
    }
}

// Moving a poller needs no re-pointing: the registration task points at the heap state, so
// a Rust move is enough (and assignment drops the old poller first).

/// The task that adds a poller's pollfn to the reactor's poller list.
#[repr(C)]
pub struct RegistrationTask {
    // First field: the task header (see `Task`).
    base: TaskBase,
    _p: Option<NonNull<PollerState<dyn Pollfn>>>,
}

impl RegistrationTask {
    /// Cancels the registration (the poller was dropped before the task ran).
    #[inline]
    pub fn cancel(&mut self) {
        self._p = None;
    }
}

// SAFETY: RegistrationTask is repr(C) with the header built by TaskBase::new::<Self> first.
unsafe impl Task for RegistrationTask {
    /// Registers the pollfn unless cancelled, then frees the task.
    unsafe fn run_and_dispose(this: NonNull<Self>) {
        let me = this.as_ptr();
        // SAFETY: the task is live; a non-cancelled task points to the live state of its
        // poller; `delete this` with no reference to the task live (it was allocated by
        // `do_register`).
        unsafe {
            if let Some(p) = (*me)._p {
                let p = p.as_ptr();
                // On this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
                (*engine()).register_poller(std::ptr::addr_of_mut!((*p)._pollfn));
                (*p)._registration_task = std::ptr::null_mut();
            }
            drop(Box::from_raw(me));
        }
    }
    /// Always `None`: nothing waits for this task.
    #[inline]
    unsafe fn waiting_task(_this: NonNull<Self>) -> Option<TaskPtr> {
        None
    }
}

/// The task that removes a dropped poller's placeholder pollfn from the reactor's list.
#[repr(C)]
pub struct DeregistrationTask {
    // First field: the task header (see `Task`).
    base: TaskBase,
    _p: NodePtr<dyn Pollfn>,
}

// SAFETY: DeregistrationTask is repr(C) with the header built by TaskBase::new::<Self> first.
unsafe impl Task for DeregistrationTask {
    /// Unregisters the placeholder pollfn, then frees the task (and the placeholder).
    unsafe fn run_and_dispose(this: NonNull<Self>) {
        let me = this.as_ptr();
        // SAFETY: the task is live; `delete this` with no reference to it live (it was
        // allocated by `Poller::drop`).
        unsafe {
            // On this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
            (*engine()).unregister_poller((*me)._p.as_ptr());
            drop(Box::from_raw(me));
        }
    }
    /// Always `None`: nothing waits for this task.
    #[inline]
    unsafe fn waiting_task(_this: NonNull<Self>) -> Option<TaskPtr> {
        None
    }
}
