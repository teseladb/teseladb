//! Futures whose result can be awaited by many fibers.
//!
//! A [`SharedFuture`] wraps a [`Future`] and hands out any number of independent futures that
//! resolve with clones of its result; a [`SharedPromise`] is the matching producer. Waiters can
//! give up early, at a deadline of clock `C` (failing with `TimedOutError`) or when an
//! [`AbortSource`] fires. All of it is shard-local: the shared state is reference counted with
//! a non-atomic shared pointer and, once the original future resolves, a task runs on the
//! shard to resolve every waiter.

use std::marker::PhantomData;
use std::pin::Pin;
use std::ptr::{self, NonNull};

use crate::engine::core::abort_on_expiry::AbortOnExpiry;
use crate::engine::core::abort_source::{AbortRequestedException, AbortSource};
use crate::engine::core::abortable_fifo::internal::{AbortableFifo, Aborter};
use crate::engine::core::future::{
    Exception, ExceptionPtr, Future, Promise, make_exception_future, make_exception_ptr,
    make_ready_future,
};
use crate::engine::core::lowres_clock::LowresClock;
use crate::engine::core::scheduling::{current_scheduling_group, default_scheduling_group};
use crate::engine::core::shared_ptr::{EnableLwSharedFromThis, LwSharedPtr, make_lw_shared};
use crate::engine::core::task::{Task, TaskBase, TaskPtr};
use crate::engine::core::timed_out_error::TimedOutError;
use crate::engine::core::timer::TimerClock;
use crate::engine::util::critical_alloc_section::memory::ScopedCriticalAllocSection;

/// Names a clock for the timeouts of shared futures.
///
/// The clock is the second type parameter of [`SharedFuture`] and [`SharedPromise`],
/// [`LowresClock`] by default; this marker type carries a clock where a type is expected.
#[derive(Debug, Default, Clone, Copy)]
pub struct WithClock<C>(PhantomData<C>);

struct Entry<T: 'static, C: TimerClock> {
    pr: Promise<T>,
    timer: Option<AbortOnExpiry<C>>,
}

#[derive(Default)]
struct EntryExpiry;

impl<T: 'static, C: TimerClock> Aborter<Entry<T, C>> for EntryExpiry {
    fn on_abort(&mut self, e: &mut Entry<T, C>, _: Option<&ExceptionPtr>) {
        if e.timer.is_some() {
            e.pr.set_exception(make_exception_ptr(TimedOutError));
        } else {
            e.pr.set_exception(make_exception_ptr(AbortRequestedException));
        }
    }
}

/// The state shared by the copies of a [`SharedFuture`]: the original future, the waiting
/// peers, and the task that resolves them once the original future is ready.
#[repr(C)]
pub struct SharedState<T: Clone + 'static, C: TimerClock> {
    // First field: the task header (see `Task`).
    task: TaskBase,
    original_future: Future<T>,
    // Ensures that the shared state is alive until run_and_dispose() runs (if the task was scheduled)
    keepaliver: LwSharedPtr<SharedState<T, C>>,
    peers: AbortableFifo<Entry<T, C>, EntryExpiry>,
}

impl<T: Clone + 'static, C: TimerClock> EnableLwSharedFromThis for SharedState<T, C> {}

impl<T: Clone + 'static, C: TimerClock> Drop for SharedState<T, C> {
    fn drop(&mut self) {
        // Don't warn if the shared future is exceptional. Any
        // warnings will be reported by the futures returned by
        // get_future.
        if self.original_future.failed() {
            self.original_future.ignore_ready_future();
        }
    }
}

// SAFETY: SharedState is repr(C) with the header built by TaskBase::new::<Self> first.
unsafe impl<T: Clone + 'static, C: TimerClock> Task for SharedState<T, C> {
    unsafe fn run_and_dispose(this: NonNull<Self>) {
        let me = this.as_ptr();
        // SAFETY: the task pointer comes from the keepaliver, so the state is alive until
        // the keepaliver is released below; its fields are reached through `me`, so no
        // reference to the state is live when that release destroys it.
        unsafe {
            let original = ptr::addr_of!((*me).original_future);
            let peers = ptr::addr_of_mut!((*me).peers);
            if (*original).state().failed() {
                let ex = (*original).state().get_exception_ref().clone();
                while (*peers).is_some() {
                    (*peers).front().pr.set_exception(ex.clone());
                    (*peers).pop_front();
                }
            } else {
                while (*peers).is_some() {
                    let v = (*original).state().get_value_ref().clone();
                    (*peers).front().pr.set_value(v);
                    (*peers).pop_front();
                }
            }
            // The peer list is now empty, but let's also make sure it releases any
            // memory it might hold in reserve.
            *peers = AbortableFifo::new(EntryExpiry);
            // Releasing the last reference destroys this object; nothing touches it after.
            let keepaliver = (*ptr::addr_of_mut!((*me).keepaliver)).release();
            drop(keepaliver);
        }
    }
    #[inline]
    unsafe fn waiting_task(_this: NonNull<Self>) -> Option<TaskPtr> {
        None
    }
}

impl<T: Clone + 'static, C: TimerClock> SharedState<T, C> {
    /// Creates the state for the original future `f`.
    fn new(f: Future<T>) -> Self {
        Self {
            task: TaskBase::new::<Self>(default_scheduling_group()), // SG is set later, when the task is scheduled
            original_future: f,
            keepaliver: LwSharedPtr::null(),
            peers: AbortableFifo::new(EntryExpiry),
        }
    }

    fn ready_copy(&self) -> Future<T> {
        let state = self.original_future.state();
        if state.failed() {
            make_exception_future(state.get_exception_ref().clone())
        } else {
            make_ready_future(state.get_value_ref().clone())
        }
    }

    /// `me` is the pointer of the shared pointer that owns this state (the provenance of
    /// the whole object created by make_lw_shared), from which the keepaliver is made.
    fn keep_alive(&mut self, me: *mut Self) {
        if !self.keepaliver.is_some() {
            self.task.set_scheduling_group(current_scheduling_group());
            // SAFETY: `me` is this state, from its LwSharedPtr's pointer.
            self.keepaliver = unsafe { LwSharedPtr::shared_from_this(me) };
            // The task pointer comes from the shared pointer (the provenance of the whole
            // object created by make_lw_shared); it is valid until run_and_dispose.
            // SAFETY: get() of a non-null shared pointer is non-null.
            let me = unsafe { NonNull::new_unchecked(self.keepaliver.get()) };
            self.original_future.set_task(TaskPtr::from_task(me));
        }
    }

    /// Returns a new future for the result; it fails with `TimedOutError` if the result is
    /// not ready by `timeout` (the clock's maximum time point means no timeout).
    ///
    /// `me` is the pointer of the shared pointer that owns this state.
    pub fn get_future(&mut self, me: *mut Self, timeout: C::TimePoint) -> Future<T> {
        // Some steps below can fail, like growing the peer list or cloning the ready value.
        // We would rather abort than propagate such errors, as the caller cannot distinguish
        // an error of the original future from a failure of `get_future` itself.
        let _g = ScopedCriticalAllocSection::new();
        if !self.original_future.available() {
            let e = self.peers.emplace_back(Entry { pr: Promise::new(), timer: None });
            let f = e.pr.get_future();
            if timeout != C::max_time_point() {
                let t = e.timer.insert(AbortOnExpiry::new(timeout));
                // SAFETY: the abort_on_expiry lives in the entry, which does not move
                // while it is in the fifo and is dropped in place, so it may be pinned
                // there (pinning it arms its timer); the abort source is pinned with it.
                unsafe {
                    let as_: *mut AbortSource =
                        Pin::new_unchecked(t).abort_source().get_unchecked_mut();
                    self.peers.make_back_abortable(Pin::new_unchecked(&mut *as_));
                }
            }
            self.keep_alive(me);
            f
        } else {
            self.ready_copy()
        }
    }

    /// Returns a new future for the result; it fails with the abort error if `as_` requests
    /// an abort first.
    ///
    /// `me` is the pointer of the shared pointer that owns this state.
    pub fn get_future_abortable(&mut self, me: *mut Self, as_: Pin<&mut AbortSource>) -> Future<T> {
        // Some steps below can fail, like growing the peer list or cloning the ready value.
        // We would rather abort than propagate such errors, as the caller cannot distinguish
        // an error of the original future from a failure of `get_future` itself.
        let _g = ScopedCriticalAllocSection::new();
        if !self.original_future.available() {
            let e = self.peers.emplace_back(Entry { pr: Promise::new(), timer: None });
            let f = e.pr.get_future();
            // SAFETY: the shared state lives in the allocation of make_lw_shared and never
            // moves; it is kept alive while it has peers (keep_alive below).
            unsafe { self.peers.make_back_abortable(as_) };
            self.keep_alive(me);
            f
        } else {
            self.ready_copy()
        }
    }

    /// Returns whether the original future has resolved (ready or failed).
    #[inline]
    pub fn available(&self) -> bool {
        self.original_future.available()
    }

    /// Returns whether the original future has failed.
    #[inline]
    pub fn failed(&self) -> bool {
        self.original_future.failed()
    }

    /// Returns whether the task that resolves the waiters is scheduled. Used only in tests.
    #[inline]
    pub fn has_scheduled_task(&self) -> bool {
        self.keepaliver.is_some()
    }
}
/// Like [`Future`] except the result can be waited for by many fibers.
///
/// Represents a value which may not yet be ready. A fiber can wait for the value using
/// the future obtained by calling [`SharedFuture::get_future`] or converting with `From`.
/// Multiple fibers are allowed to obtain a future for the result using the same
/// shared future.
///
/// All futures obtained from a shared future end up in the same state, each with its own
/// clone of the value.
///
/// The futures obtained from a shared future do not borrow it: they may outlive it.
///
/// A shared future can be cloned at any time and all clones resolve with the same value.
///
/// A default-constructed shared future is invalid ([`SharedFuture::valid`] is false) and
/// obtaining futures from it must not be attempted.
///
/// `T` must be `Clone`. `C` is the clock used for timeouts, [`LowresClock`] by default.
pub struct SharedFuture<T: Clone + 'static = (), C: TimerClock = LowresClock> {
    state: LwSharedPtr<SharedState<T, C>>,
}

impl<T: Clone + 'static, C: TimerClock> Default for SharedFuture<T, C> {
    fn default() -> Self {
        Self { state: LwSharedPtr::null() }
    }
}

impl<T: Clone + 'static, C: TimerClock> Clone for SharedFuture<T, C> {
    fn clone(&self) -> Self {
        Self { state: self.state.clone() }
    }
}

impl<T: Clone + 'static, C: TimerClock> SharedFuture<T, C> {
    /// Forwards the result of future `f` into a new shared future.
    pub fn new(f: Future<T>) -> Self {
        Self { state: make_lw_shared(SharedState::new(f)) }
    }

    /// The shared state, mutably.
    ///
    /// # Panics
    /// If this shared future is invalid (default-constructed).
    #[inline]
    #[allow(clippy::mut_from_ref)]
    fn st(&self) -> &mut SharedState<T, C> {
        assert!(
            self.state.is_some(),
            "shared_future used in an invalid (default-constructed) state"
        );
        // SAFETY: the state is engaged (checked above); shards are single-threaded and the
        // state is not otherwise borrowed during the call.
        unsafe { self.state.get_mut_unchecked() }
    }

    /// Creates a new future which will resolve with the result of this shared future.
    ///
    /// The returned future fails with `TimedOutError` if this shared future doesn't resolve
    /// before `timeout` is reached.
    ///
    /// # Panics
    /// Panics if this shared future is invalid.
    #[inline]
    pub fn get_future_until(&self, timeout: C::TimePoint) -> Future<T> {
        self.st().get_future(self.state.get(), timeout)
    }

    /// Creates a new future which will resolve with the result of this shared future, with
    /// no timeout.
    ///
    /// # Panics
    /// Panics if this shared future is invalid.
    #[inline]
    pub fn get_future(&self) -> Future<T> {
        self.get_future_until(C::max_time_point())
    }

    /// Creates a new future which will resolve with the result of this shared future.
    ///
    /// The returned future fails with the abort error (by default `AbortRequestedException`)
    /// if this shared future doesn't resolve before `as_` is aborted.
    ///
    /// # Panics
    /// Panics if this shared future is invalid.
    #[inline]
    pub fn get_future_abortable(&self, as_: Pin<&mut AbortSource>) -> Future<T> {
        self.st().get_future_abortable(self.state.get(), as_)
    }

    /// Returns true if the future is available (ready or failed).
    ///
    /// # Panics
    /// If it is not (see [`SharedFuture::valid`]).
    #[inline]
    pub fn available(&self) -> bool {
        self.state.available()
    }

    /// Returns true if the future has failed.
    ///
    /// # Panics
    /// If it is not (see [`SharedFuture::valid`]).
    #[inline]
    pub fn failed(&self) -> bool {
        self.state.failed()
    }

    /// Returns true if the shared future is valid (not default-constructed).
    #[inline]
    pub fn valid(&self) -> bool {
        self.state.is_some()
    }

    /// Returns the shared state, for tests.
    #[inline]
    pub fn shared_state(&self) -> &SharedState<T, C> {
        &self.state
    }

    /// Returns whether the task that resolves the waiters is scheduled; public only for tests.
    #[doc(hidden)]
    #[inline]
    pub fn has_scheduled_task(&self) -> bool {
        self.state.has_scheduled_task()
    }
}

impl<T: Clone + 'static, C: TimerClock> From<&SharedFuture<T, C>> for Future<T> {
    /// Equivalent to [`SharedFuture::get_future`].
    fn from(sf: &SharedFuture<T, C>) -> Future<T> {
        sf.get_future()
    }
}

/// Like a promise except that its counterpart is a [`SharedFuture`] instead of a [`Future`].
///
/// When the shared promise is made ready, every waiter is also made ready.
///
/// Like the shared future, `T` must be `Clone`.
pub struct SharedPromise<T: Clone + 'static = (), C: TimerClock = LowresClock> {
    promise: Promise<T>,
    shared_future: SharedFuture<T, C>,
}

impl<T: Clone + 'static, C: TimerClock> Default for SharedPromise<T, C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone + 'static, C: TimerClock> SharedPromise<T, C> {
    /// Creates an unresolved shared promise.
    pub fn new() -> Self {
        let mut promise = Promise::new();
        let f = promise.get_future();
        Self { promise, shared_future: SharedFuture::new(f) }
    }

    /// Gets a new future associated with this promise.
    /// If the promise is not resolved before `timeout` the returned future fails with `TimedOutError`.
    /// This instance doesn't have to be kept alive until the returned future resolves.
    #[inline]
    pub fn get_shared_future_until(&self, timeout: C::TimePoint) -> Future<T> {
        self.shared_future.get_future_until(timeout)
    }

    /// Gets a new future associated with this promise, with no timeout.
    #[inline]
    pub fn get_shared_future(&self) -> Future<T> {
        self.shared_future.get_future()
    }

    /// Gets a new future associated with this promise.
    /// If the promise is not resolved before the abort source is triggered the returned future
    /// fails with the abort error.
    /// This instance doesn't have to be kept alive until the returned future resolves.
    #[inline]
    pub fn get_shared_future_abortable(&self, as_: Pin<&mut AbortSource>) -> Future<T> {
        self.shared_future.get_future_abortable(as_)
    }

    /// Sets the shared promise's value, resolving every waiter with a clone of it.
    #[inline]
    pub fn set_value(&mut self, result: T) {
        self.promise.set_value(result);
    }

    /// Marks the shared promise as failed with `ex`, failing every waiter.
    #[inline]
    pub fn set_exception(&mut self, ex: ExceptionPtr) {
        self.promise.set_exception(ex);
    }

    /// Marks the shared promise as failed with the error `e`, failing every waiter.
    #[inline]
    pub fn set_exception_from<E: Exception>(&mut self, e: E) {
        self.set_exception(make_exception_ptr(e));
    }

    /// Returns true if the underlying future is available (ready or failed).
    #[inline]
    pub fn available(&self) -> bool {
        self.shared_future.available()
    }

    /// Returns true if the underlying future has failed.
    #[inline]
    pub fn failed(&self) -> bool {
        self.shared_future.failed()
    }
}
