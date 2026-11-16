//! Condition variables for fibers of one shard.
//!
//! A [`ConditionVariable`] lets fibers wait until another fiber signals it. There are two
//! families of waits: the `wait*` methods return an engine [`Future`] and allocate one waiter
//! per call, while the `when*` methods return a [`CvAwaiter`], a Rust future meant to be
//! `.await`ed in a coroutine, which allocates nothing. Both support a deadline of a
//! [`TimerClock`] (failing with [`ConditionVariableTimedOut`]) and a predicate re-checked
//! after each wake-up. [`ConditionVariable::broken`] fails all current and future waiters.
//!
//! Every waiter starts with a header holding the list hook and a small vtable (`signal`,
//! `set_exception`), so the wait list can hold both kinds. The awaiters link themselves into
//! the wait list on their first poll, when they are pinned, and unlink themselves when
//! dropped. They hold the outcome and the waker of the awaiting task themselves.
//!
//! The waiters are linked into a list whose sentinel is inside the condition variable,
//! so a condition variable is `!Unpin`, and the operations that add waiters take it
//! pinned (`Pin<&Self>`; several fibers wait on it at once, so the state is interior).

use std::cell::{Cell, UnsafeCell};
use std::fmt;
use std::future::Future as StdFuture;
use std::marker::{PhantomData, PhantomPinned};
use std::pin::Pin;
use std::ptr::{self, NonNull};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use crate::engine::core::api::intrusive::{List, ListHook};
use crate::engine::core::future::{
    ExceptionPtr, Future, HostBox, HostedPromise, PromiseHost, make_exception_ptr,
    make_ready_future,
};
use crate::engine::core::loop_::{StopCondition, do_until};
use crate::engine::core::timer::{Clock, SteadyClockType, Timer, TimerClock};

/// Error delivered to waiters of a condition variable broken by [`ConditionVariable::broken`].
#[derive(Debug, Clone, Copy, Default)]
pub struct BrokenConditionVariable;

impl BrokenConditionVariable {
    /// Reports the exception reason.
    pub fn what(&self) -> &'static str {
        "Condition variable is broken"
    }
}

impl fmt::Display for BrokenConditionVariable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.what())
    }
}

impl std::error::Error for BrokenConditionVariable {}

/// Error delivered when a timed wait ([`ConditionVariable::wait_until`] and friends) times
/// out.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConditionVariableTimedOut;

impl ConditionVariableTimedOut {
    /// Reports the exception reason.
    pub fn what(&self) -> &'static str {
        "Condition variable timed out"
    }
}

impl fmt::Display for ConditionVariableTimedOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.what())
    }
}

impl std::error::Error for ConditionVariableTimedOut {}

// The header of every waiter in the wait list: promise-based waiters and awaiters both
// start with it, and the vtable dispatches to the concrete kind.
#[repr(C)]
struct Waiter {
    hook: ListHook,
    vt: &'static WaiterVTable,
}

struct WaiterVTable {
    /// Wakes the waiter successfully.
    signal: unsafe fn(*mut Waiter),
    /// Fails the waiter with an error.
    set_exception: unsafe fn(*mut Waiter, ExceptionPtr),
}

crate::intrusive_adapter!(WaiterAdapter = Waiter { hook });

impl Waiter {
    #[inline]
    fn new(vt: &'static WaiterVTable) -> Self {
        Self { hook: ListHook::new(), vt }
    }

    /// Times the waiter out: unlinks it and fails it with [`ConditionVariableTimedOut`].
    ///
    /// # Safety
    /// `w` must be a live waiter.
    unsafe fn timeout(w: *mut Waiter) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            (*w).hook.unlink();
            ((*w).vt.set_exception)(w, make_exception_ptr(ConditionVariableTimedOut));
        }
    }

    /// # Safety
    /// `w` must be a live waiter.
    #[inline]
    unsafe fn signal(w: *mut Waiter) {
        // SAFETY: guaranteed by the caller.
        unsafe { ((*w).vt.signal)(w) };
    }

    /// # Safety
    /// `w` must be a live waiter.
    #[inline]
    unsafe fn set_exception(w: *mut Waiter, ep: ExceptionPtr) {
        // SAFETY: guaranteed by the caller.
        unsafe { ((*w).vt.set_exception)(w, ep) };
    }
}

/// A promise-based waiter, with a timer for timed waits. It is owned by a [`HostBox`], so the
/// promise node is embedded: one allocation per wait.
#[repr(C)]
struct PromiseWaiter<T> {
    // First field: the waiter header, so a pointer to it is a pointer to the waiter.
    waiter: Waiter,
    pr: HostedPromise<()>,
    timer: T,
}

// SAFETY: hosted_promise returns the `pr` field, derived from `this`.
unsafe impl<T> PromiseHost for PromiseWaiter<T> {
    type Value = ();
    #[inline]
    unsafe fn hosted_promise(this: *mut Self) -> *mut HostedPromise<()> {
        // SAFETY: guaranteed by the caller.
        unsafe { ptr::addr_of_mut!((*this).pr) }
    }
}

impl<T> PromiseWaiter<T> {
    const VT: WaiterVTable =
        WaiterVTable { signal: Self::signal, set_exception: Self::set_exception };

    /// # Safety
    /// `w` must be the header of a live `PromiseWaiter<T>`, derived from the pointer
    /// `HostBox::into_raw` gave (the waiter list and the timer callback hold that one).
    unsafe fn signal(w: *mut Waiter) {
        // SAFETY: the header is the first field of the repr(C) PromiseWaiter<T>, and `w`
        // has the provenance of the whole boxed waiter.
        unsafe {
            let me = w.cast::<Self>();
            HostedPromise::set_value(ptr::addr_of_mut!((*me).pr), ());
            // note: we self-delete in either case we are woken
            // up. See usage below: only the resulting future
            // state is required once we've left the wait queue
            drop(HostBox::from_raw(me));
        }
    }

    /// # Safety
    /// As [`PromiseWaiter::signal`].
    unsafe fn set_exception(w: *mut Waiter, ep: ExceptionPtr) {
        // SAFETY: as in signal.
        unsafe {
            let me = w.cast::<Self>();
            HostedPromise::set_exception(ptr::addr_of_mut!((*me).pr), ep);
            // see comment above
            drop(HostBox::from_raw(me));
        }
    }
}

/// Condition variable.
///
/// This is a standard computer science condition variable without locking,
/// since on a shard access to variables is atomic anyway, adapted
/// for futures.  You can wait for the variable to be notified.
///
/// To support exceptional conditions, a [`ConditionVariable::broken`] method
/// is provided, which causes all current waiters to stop waiting,
/// with a failed future returned.  This allows causing all
/// fibers that are blocked on a condition variable to continue.
/// This is similar to POSIX's `pthread_cancel()`, with [`ConditionVariable::wait`]
/// acting as a cancellation point.
///
/// A signal with no waiter is remembered and consumed by the next wait, which then returns
/// at once.
///
/// The waiters point at the condition variable, so the waiting operations take it
/// pinned (`std::pin::pin!`, `Box::pin`, or a pinned field of a pinned owner).
#[derive(Default)]
pub struct ConditionVariable {
    waiters: UnsafeCell<List<WaiterAdapter>>,
    ex: UnsafeCell<ExceptionPtr>, //"broken" exception
    signalled: Cell<bool>,        // set to true if signalled while no waiters
    _pin: PhantomPinned,
}

impl ConditionVariable {
    /// Constructs a condition variable with no pending signal, so the first wait before a
    /// signal is not woken up immediately.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// The wait list. Each use is a short reference that ends before any waiter runs.
    #[inline(always)]
    fn list(&self) -> *mut List<WaiterAdapter> {
        self.waiters.get()
    }

    /// A clone of the "broken" exception (null if not broken).
    #[inline(always)]
    fn ex(&self) -> ExceptionPtr {
        // SAFETY: the exception is only replaced by broken_with, never while borrowed.
        unsafe { (*self.ex.get()).clone() }
    }

    /// Whether the condition variable was broken.
    #[inline(always)]
    fn is_broken(&self) -> bool {
        // SAFETY: as in ex().
        unsafe { !(*self.ex.get()).is_null() }
    }

    /// Links `w` at the back of the wait list, or fails it at once if the variable is broken.
    ///
    /// # Safety
    /// `w` must be a live waiter that stays in place while linked; the condition variable
    /// must be pinned (it does not move while it has waiters).
    unsafe fn add_waiter(&self, w: *mut Waiter) {
        assert!(!self.signalled.get()); // should not have snuck between
        if self.is_broken() {
            // SAFETY: guaranteed by the caller.
            unsafe { Waiter::set_exception(w, self.ex()) };
            return;
        }
        // SAFETY: guaranteed by the caller.
        unsafe { (*self.list()).push_back(w) };
    }

    fn wakeup_first(&self) -> bool {
        // SAFETY: the reference to the list ends with the statement.
        let Some(w) = (unsafe { (*self.list()).pop_front() }) else {
            return false;
        };
        // SAFETY: linked waiters are live.
        unsafe {
            if self.is_broken() {
                Waiter::set_exception(w.as_ptr(), self.ex());
            } else {
                Waiter::signal(w.as_ptr());
            }
        }
        true
    }

    #[inline]
    fn check_and_consume_signal(&self) -> bool {
        self.signalled.replace(false)
    }

    /// Waits until the condition variable is signaled; may wake up without the condition
    /// being met.
    ///
    /// Returns a future that becomes ready when [`ConditionVariable::signal`] is called. If
    /// the condition variable was broken, it fails with the broken error
    /// ([`BrokenConditionVariable`] by default).
    pub fn wait(self: Pin<&Self>) -> Future<()> {
        if self.check_and_consume_signal() {
            return make_ready_future(());
        }
        let w = HostBox::into_raw(HostBox::new(PromiseWaiter {
            waiter: Waiter::new(&PromiseWaiter::<()>::VT),
            pr: HostedPromise::new(),
            timer: (),
        }));
        // SAFETY: the waiter is boxed; it frees itself when woken up. The condition
        // variable is pinned.
        unsafe {
            let f = HostedPromise::get_future(ptr::addr_of_mut!((*w).pr));
            self.add_waiter(w.cast::<Waiter>());
            f
        }
    }

    /// Waits until the condition variable is signaled or `timeout` is reached.
    ///
    /// Returns a future that becomes ready when [`ConditionVariable::signal`] is called. If
    /// the condition variable was broken, it fails with the broken error; if `timeout` is
    /// reached, with [`ConditionVariableTimedOut`].
    pub fn wait_until<C: TimerClock>(self: Pin<&Self>, timeout: C::TimePoint) -> Future<()> {
        if self.check_and_consume_signal() {
            return make_ready_future(());
        }
        let w = HostBox::into_raw(HostBox::new(PromiseWaiter {
            waiter: Waiter::new(&PromiseWaiter::<Timer<C>>::VT),
            pr: HostedPromise::new(),
            timer: Timer::<C>::new(),
        }));
        // SAFETY: the waiter is boxed; it frees itself when woken up or timed out, which
        // also drops (cancels) its timer. The boxed timer never moves, so it may be pinned;
        // when the timer callback frees the waiter, Timer::fire stops touching the timer.
        // The condition variable is pinned.
        unsafe {
            let f = HostedPromise::get_future(ptr::addr_of_mut!((*w).pr));
            let h = w.cast::<Waiter>();
            let mut timer = Pin::new_unchecked(&mut *ptr::addr_of_mut!((*w).timer));
            timer.as_mut().set_callback(move || Waiter::timeout(h));
            timer.arm(timeout, None);
            self.add_waiter(h);
            f
        }
    }

    /// Waits until the condition variable is signaled or the duration `timeout` has passed,
    /// on the steady clock. Fails as [`ConditionVariable::wait_until`] does.
    pub fn wait_for(self: Pin<&Self>, timeout: Duration) -> Future<()> {
        self.wait_until::<SteadyClockType>(SteadyClockType::add(SteadyClockType::now(), timeout))
    }

    /// Waits until the condition variable is notified and `pred` returns true, otherwise
    /// waits again. `pred` is checked first, so no wait happens if it already holds.
    ///
    /// Returns a future that becomes ready once `pred` holds. If the condition variable was
    /// broken, or `pred` fails, the future fails with that error.
    ///
    /// # Safety
    /// The condition variable must outlive the returned future: the loop waits on it again
    /// after each wake-up.
    pub unsafe fn wait_pred<P: StopCondition<M> + 'static, M: 'static>(
        self: Pin<&Self>,
        pred: P,
    ) -> Future<()> {
        let me: *const Self = self.get_ref();
        // SAFETY: guaranteed by the caller; the condition variable stays pinned.
        do_until(pred, move || unsafe { Pin::new_unchecked(&*me).wait() })
    }

    /// Waits until the condition variable is notified and `pred` returns true, or `timeout`
    /// is reached; otherwise waits again.
    ///
    /// Fails like [`ConditionVariable::wait_pred`], and with [`ConditionVariableTimedOut`] if
    /// `timeout` is reached first.
    ///
    /// # Safety
    /// As [`ConditionVariable::wait_pred`].
    pub unsafe fn wait_until_pred<C: TimerClock, P: StopCondition<M> + 'static, M: 'static>(
        self: Pin<&Self>,
        timeout: C::TimePoint,
        pred: P,
    ) -> Future<()> {
        let me: *const Self = self.get_ref();
        // SAFETY: as in wait_pred.
        do_until(pred, move || unsafe { Pin::new_unchecked(&*me).wait_until::<C>(timeout) })
    }

    /// [`ConditionVariable::wait_until_pred`] with a duration from now on the steady clock.
    ///
    /// # Safety
    /// As [`ConditionVariable::wait_pred`].
    pub unsafe fn wait_for_pred<P: StopCondition<M> + 'static, M: 'static>(
        self: Pin<&Self>,
        timeout: Duration,
        pred: P,
    ) -> Future<()> {
        // SAFETY: forwarded from the caller.
        unsafe {
            self.wait_until_pred::<SteadyClockType, P, M>(
                SteadyClockType::add(SteadyClockType::now(), timeout),
                pred,
            )
        }
    }

    /// Awaitable form of [`ConditionVariable::wait`], for coroutines; allocation-free.
    ///
    /// The awaiter resolves when [`ConditionVariable::signal`] is called, or to the broken
    /// error if the condition variable was broken.
    #[must_use = "the future returned by when() must be awaited"]
    pub fn when(self: Pin<&Self>) -> CvAwaiter<'_, NoTimeout, NoPredicate> {
        CvAwaiter::new(self, NoTimeout, NoPredicate)
    }

    /// Awaitable form of [`ConditionVariable::wait_until`], for coroutines; allocation-free.
    ///
    /// Resolves to the broken error if the condition variable was broken, and to
    /// [`ConditionVariableTimedOut`] if `timeout` is reached first.
    #[must_use = "the future returned by when() must be awaited"]
    pub fn when_until<C: TimerClock>(
        self: Pin<&Self>,
        timeout: C::TimePoint,
    ) -> CvAwaiter<'_, WithTimeout<C>, NoPredicate> {
        CvAwaiter::new(self, WithTimeout { timer: Timer::new(), timeout }, NoPredicate)
    }

    /// Awaitable form of [`ConditionVariable::wait_for`], for coroutines; allocation-free.
    ///
    /// Resolves to the broken error if the condition variable was broken, and to
    /// [`ConditionVariableTimedOut`] if the duration `timeout` passes first.
    #[must_use = "the future returned by when() must be awaited"]
    pub fn when_for(
        self: Pin<&Self>,
        timeout: Duration,
    ) -> CvAwaiter<'_, WithTimeout<SteadyClockType>, NoPredicate> {
        self.when_until::<SteadyClockType>(SteadyClockType::add(SteadyClockType::now(), timeout))
    }

    /// Awaitable form of [`ConditionVariable::wait_pred`], for coroutines; allocation-free.
    ///
    /// Resolves once `pred` returns true, checking it first and after each signal; resolves
    /// to an error if the variable was broken or `pred` fails.
    #[must_use = "the future returned by when() must be awaited"]
    pub fn when_pred<P: StopCondition<M>, M>(
        self: Pin<&Self>,
        pred: P,
    ) -> CvAwaiter<'_, NoTimeout, WithPredicate<P, M>> {
        CvAwaiter::new(self, NoTimeout, WithPredicate(pred, PhantomData))
    }

    /// Awaitable form of [`ConditionVariable::wait_until_pred`], for coroutines;
    /// allocation-free.
    ///
    /// Resolves once `pred` returns true; to an error if the variable was broken or `pred`
    /// fails, and to [`ConditionVariableTimedOut`] if `timeout` is reached first.
    #[must_use = "the future returned by when() must be awaited"]
    pub fn when_until_pred<C: TimerClock, P: StopCondition<M>, M>(
        self: Pin<&Self>,
        timeout: C::TimePoint,
        pred: P,
    ) -> CvAwaiter<'_, WithTimeout<C>, WithPredicate<P, M>> {
        CvAwaiter::new(
            self,
            WithTimeout { timer: Timer::new(), timeout },
            WithPredicate(pred, PhantomData),
        )
    }

    /// Awaitable form of [`ConditionVariable::wait_for_pred`], for coroutines;
    /// allocation-free.
    ///
    /// Resolves once `pred` returns true; to an error if the variable was broken or `pred`
    /// fails, and to [`ConditionVariableTimedOut`] if the duration `timeout` passes first.
    #[must_use = "the future returned by when() must be awaited"]
    pub fn when_for_pred<P: StopCondition<M>, M>(
        self: Pin<&Self>,
        timeout: Duration,
        pred: P,
    ) -> CvAwaiter<'_, WithTimeout<SteadyClockType>, WithPredicate<P, M>> {
        self.when_until_pred::<SteadyClockType, P, M>(
            SteadyClockType::add(SteadyClockType::now(), timeout),
            pred,
        )
    }

    /// Whether or not the condition variable currently has pending waiter(s)
    /// The returned answer is valid until next continuation/fiber switch.
    #[inline]
    pub fn has_waiters(&self) -> bool {
        // SAFETY: the reference to the list ends with the call.
        unsafe { !(*self.list()).is_empty() }
    }

    /// Notifies the variable and wakes up the first waiter if there is one; otherwise the
    /// signal is remembered for the next wait.
    pub fn signal(&self) {
        if !self.wakeup_first() {
            self.signalled.set(true);
        }
    }

    /// Notifies the variable and wakes up all current waiters.
    pub fn broadcast(&self) {
        // The waiters are moved to a local list, which stays on this frame until it is
        // drained.
        let mut tmp = List::<WaiterAdapter>::new();
        // SAFETY: `tmp` does not move while it holds waiters; the reference to the wait
        // list ends with the statement.
        unsafe { tmp.splice_back(&mut *self.list()) };
        while let Some(w) = tmp.pop_front() {
            // SAFETY: linked waiters are live; a waiter that re-adds itself goes to the
            // condition variable's list, not to `tmp`.
            unsafe {
                if self.is_broken() {
                    Waiter::set_exception(w.as_ptr(), self.ex());
                } else {
                    Waiter::signal(w.as_ptr());
                }
            }
        }
    }

    /// Signals to waiters that an error occurred. [`ConditionVariable::wait`] will see a
    /// failed future holding [`BrokenConditionVariable`]. The future is made available
    /// immediately, for current and later waits.
    pub fn broken(&self) {
        self.broken_with(make_exception_ptr(BrokenConditionVariable));
    }

    /// Signals to waiters that an error occurred. [`ConditionVariable::wait`] will see a
    /// failed future holding `ep`. The future is made available immediately, for current and
    /// later waits.
    pub fn broken_with(&self, ep: ExceptionPtr) {
        // SAFETY: the exception is never borrowed across this assignment (ex() clones).
        unsafe { *self.ex.get() = ep };
        self.broadcast();
    }
}

impl Drop for ConditionVariable {
    fn drop(&mut self) {
        self.broken();
    }
}

/// The timeout part of an awaiter: [`WithTimeout`], or [`NoTimeout`].
///
/// It lives in a pinned awaiter, so it is taken pinned (the timer links itself).
pub trait AwaiterTimeout {
    /// Arms the timer to call `on_timeout(waiter)` at the deadline.
    fn arm(self: Pin<&mut Self>, waiter: NonNull<()>, on_timeout: unsafe fn(NonNull<()>));
    /// Cancels the timer.
    fn cancel(self: Pin<&mut Self>);
}

/// No timeout.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoTimeout;

impl AwaiterTimeout for NoTimeout {
    #[inline]
    fn arm(self: Pin<&mut Self>, _: NonNull<()>, _: unsafe fn(NonNull<()>)) {}
    #[inline]
    fn cancel(self: Pin<&mut Self>) {}
}

/// A deadline of clock `C`: the awaiter embeds a timer.
pub struct WithTimeout<C: TimerClock> {
    timer: Timer<C>,
    timeout: C::TimePoint,
}

impl<C: TimerClock> WithTimeout<C> {
    /// The pinned timer (structural pinning: the timer is never moved out).
    #[inline]
    fn timer(self: Pin<&mut Self>) -> Pin<&mut Timer<C>> {
        // SAFETY: `timer` is pinned whenever `self` is, and never moved out.
        unsafe { self.map_unchecked_mut(|s| &mut s.timer) }
    }
}

impl<C: TimerClock> AwaiterTimeout for WithTimeout<C> {
    fn arm(mut self: Pin<&mut Self>, waiter: NonNull<()>, on_timeout: unsafe fn(NonNull<()>)) {
        let timeout = self.timeout;
        let mut timer = self.as_mut().timer();
        // SAFETY: the awaiter is pinned and cancels the timer before it goes away.
        timer.as_mut().set_callback(move || unsafe { on_timeout(waiter) });
        timer.arm(timeout, None);
    }
    #[inline]
    fn cancel(self: Pin<&mut Self>) {
        self.timer().cancel();
    }
}

/// The predicate part of an awaiter: [`WithPredicate`], or [`NoPredicate`].
pub trait AwaiterPredicate {
    /// Whether there is a predicate.
    const HAS_PREDICATE: bool;
    /// Evaluates the predicate, which may fail.
    fn check(&mut self) -> Result<bool, ExceptionPtr>;
}

/// No predicate.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoPredicate;

impl AwaiterPredicate for NoPredicate {
    const HAS_PREDICATE: bool = false;
    #[inline]
    fn check(&mut self) -> Result<bool, ExceptionPtr> {
        Ok(true)
    }
}

/// A predicate re-checked after each signal; `M` is the predicate's return type (see
/// [`StopCondition`]).
pub struct WithPredicate<P: StopCondition<M>, M>(P, PhantomData<fn() -> M>);

impl<P: StopCondition<M>, M> AwaiterPredicate for WithPredicate<P, M> {
    const HAS_PREDICATE: bool = true;
    #[inline]
    fn check(&mut self) -> Result<bool, ExceptionPtr> {
        self.0.check()
    }
}

/// Where an awaiter stands.
enum CvAwaitState {
    /// Not polled yet.
    Start,
    /// Linked into the wait list; the waker of the awaiting task.
    Waiting(Option<Waker>),
    /// Signalled (`Ok`) or failed: the outcome, not yet returned.
    Done(Result<(), ExceptionPtr>),
    /// The outcome was returned.
    Finished,
}

/// An awaitable waiter of a condition variable, with an optional deadline and predicate,
/// returned by the `when*` methods.
///
/// It borrows the condition variable shared and pinned, so several fibers can wait at
/// once. It is pinned itself while linked, and it unlinks itself when dropped. It is only
/// reached through raw pointers while linked (the wait list and the timer hold pointers
/// to it), so its fields are accessed through `*mut Self`.
#[repr(C)]
pub struct CvAwaiter<'a, TO: AwaiterTimeout, PR: AwaiterPredicate> {
    // First field: the waiter header, so a pointer to it is a pointer to the awaiter.
    waiter: Waiter,
    cv: Pin<&'a ConditionVariable>,
    state: CvAwaitState,
    to: TO,
    pred: PR,
    _pin: PhantomPinned,
}

impl<'a, TO: AwaiterTimeout, PR: AwaiterPredicate> CvAwaiter<'a, TO, PR> {
    const VT: WaiterVTable =
        WaiterVTable { signal: Self::signal_vt, set_exception: Self::set_exception_vt };

    fn new(cv: Pin<&'a ConditionVariable>, to: TO, pred: PR) -> Self {
        Self {
            waiter: Waiter::new(&Self::VT),
            cv,
            state: CvAwaitState::Start,
            to,
            pred,
            _pin: PhantomPinned,
        }
    }

    /// The pinned timeout part (structural pinning: `to` is never moved out).
    ///
    /// # Safety
    /// `this` must point to the pinned, live awaiter; the reference must end before the
    /// timer runs its callback.
    #[inline]
    unsafe fn to_pinned<'b>(this: *mut Self) -> Pin<&'b mut TO> {
        // SAFETY: guaranteed by the caller.
        unsafe { Pin::new_unchecked(&mut *ptr::addr_of_mut!((*this).to)) }
    }

    /// Records the outcome and wakes the awaiting task.
    ///
    /// # Safety
    /// `this` must point to the pinned, live awaiter.
    unsafe fn complete(this: *mut Self, r: Result<(), ExceptionPtr>) {
        // SAFETY: guaranteed by the caller; the reference covers the state field only.
        unsafe {
            let st = &mut *ptr::addr_of_mut!((*this).state);
            if let CvAwaitState::Waiting(waker) = std::mem::replace(st, CvAwaitState::Done(r))
                && let Some(w) = waker
            {
                w.wake();
            }
        }
    }

    /// Completes successfully, cancelling the timer first.
    ///
    /// # Safety
    /// As [`CvAwaiter::complete`].
    unsafe fn base_signal(this: *mut Self) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            Self::to_pinned(this).cancel();
            Self::complete(this, Ok(()));
        }
    }

    /// Completes with the error `ep`, cancelling the timer first.
    ///
    /// # Safety
    /// As [`CvAwaiter::complete`].
    unsafe fn base_set_exception(this: *mut Self, ep: ExceptionPtr) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            Self::to_pinned(this).cancel();
            Self::complete(this, Err(ep));
        }
    }

    /// # Safety
    /// `w` must be the header of a pinned, live CvAwaiter, derived from the pointer to
    /// the whole awaiter (`start`).
    unsafe fn signal_vt(w: *mut Waiter) {
        let me = w.cast::<Self>();
        // SAFETY: the header is the first field of the repr(C) awaiter.
        unsafe {
            if !PR::HAS_PREDICATE {
                Self::base_signal(me);
                return;
            }
            match (*ptr::addr_of_mut!((*me).pred)).check() {
                Ok(true) => Self::base_signal(me),
                Ok(false) => {
                    // must re-enter waiter queue
                    // this maintains "wait" version
                    // semantics of moving to back of queue
                    // if predicate fails
                    // The awaiter is pinned and the condition variable outlives it ('a).
                    (*me).cv.add_waiter(w);
                }
                // The predicate failed: the awaiter completes with its error.
                Err(ep) => Self::base_set_exception(me, ep),
            }
        }
    }

    /// # Safety
    /// As [`CvAwaiter::signal_vt`].
    unsafe fn set_exception_vt(w: *mut Waiter, ep: ExceptionPtr) {
        // SAFETY: as in signal_vt.
        unsafe { Self::base_set_exception(w.cast::<Self>(), ep) };
    }

    /// # Safety
    /// `w` must be the header of a pinned, live CvAwaiter (the timer only fires while it
    /// is alive: it is cancelled on drop).
    unsafe fn on_timeout(w: NonNull<()>) {
        // SAFETY: guaranteed by the caller.
        unsafe { Waiter::timeout(w.as_ptr().cast::<Waiter>()) };
    }

    /// Runs on the first poll and decides what to wait for. Returns the outcome when there
    /// is nothing to wait for.
    ///
    /// # Safety
    /// `this` must point to the pinned, live awaiter, not started yet.
    unsafe fn start(this: *mut Self, waker: &Waker) -> Option<Result<(), ExceptionPtr>> {
        // SAFETY: guaranteed by the caller; the waiter pointer is derived from `this`.
        unsafe {
            let cv = (*this).cv;
            if PR::HAS_PREDICATE {
                match (*ptr::addr_of_mut!((*this).pred)).check() {
                    Ok(true) => return Some(Ok(())),
                    Ok(false) => {}
                    // the predicate's error is returned to the awaiting coroutine
                    Err(ep) => return Some(Err(ep)),
                }
                cv.check_and_consume_signal(); // clear out any signal state
            }
            if cv.check_and_consume_signal() {
                return Some(Ok(()));
            }
            *ptr::addr_of_mut!((*this).state) = CvAwaitState::Waiting(Some(waker.clone()));
            let w = ptr::addr_of_mut!((*this).waiter);
            Self::to_pinned(this).arm(NonNull::new_unchecked(w.cast::<()>()), Self::on_timeout);
            // The awaiter is pinned, so the waiter stays in place while linked; it unlinks
            // itself when dropped. add_waiter may complete it at once (broken variable).
            cv.add_waiter(w);
            None
        }
    }
}

impl<TO: AwaiterTimeout, PR: AwaiterPredicate> StdFuture for CvAwaiter<'_, TO, PR> {
    type Output = Result<(), ExceptionPtr>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: the awaiter is never moved out of the pin; it is reached through the
        // raw pointer from here on.
        let me: *mut Self = unsafe { self.get_unchecked_mut() };
        // SAFETY: `me` is the pinned, live awaiter; each reference to the state ends
        // before anything that may signal it runs.
        unsafe {
            let st = ptr::addr_of_mut!((*me).state);
            if let CvAwaitState::Start = *st
                && let Some(r) = Self::start(me, cx.waker())
            {
                *st = CvAwaitState::Finished;
                return Poll::Ready(r);
            }
            match std::mem::replace(&mut *st, CvAwaitState::Finished) {
                CvAwaitState::Done(r) => Poll::Ready(r),
                CvAwaitState::Waiting(old) => {
                    let waker = match old {
                        Some(w) if w.will_wake(cx.waker()) => w,
                        _ => cx.waker().clone(),
                    };
                    *st = CvAwaitState::Waiting(Some(waker));
                    Poll::Pending
                }
                CvAwaitState::Start | CvAwaitState::Finished => Poll::Ready(Ok(())),
            }
        }
    }
}

impl<TO: AwaiterTimeout, PR: AwaiterPredicate> Drop for CvAwaiter<'_, TO, PR> {
    fn drop(&mut self) {
        let me: *mut Self = self;
        // SAFETY: a Drop of a !Unpin value runs at its pinned address.
        unsafe { Self::to_pinned(me) }.cancel();
        // the hook unlinks itself when dropped
    }
}
