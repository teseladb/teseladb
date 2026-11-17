//! Counting semaphores for fibers of one shard, and RAII units.
//!
//! [`BasicSemaphore`] holds a counter of units. Waiters take units, waiting in FIFO order when
//! not enough are available, optionally with a deadline of clock `C` or an [`AbortSource`];
//! [`BasicSemaphore::signal`] returns units and wakes waiters. [`BasicSemaphore::broken`]
//! fails every current and future waiter. [`SemaphoreUnits`] (from [`get_units`] and friends)
//! returns its units on drop, and [`with_semaphore`] runs a function while holding units.
//!
//! The errors are produced by a [`SemaphoreExceptionFactory`]: the default one gives
//! [`SemaphoreTimedOut`], [`BrokenSemaphore`] and [`SemaphoreAborted`], and
//! [`NamedSemaphoreExceptionFactory`] gives variants that carry the semaphore's name. Each
//! error is its own type; the timed-out and aborted errors also expose a base error
//! (`TimedOutError`, `AbortRequestedException`) through `DerivedException`, so generic
//! handlers that look for the base find them.

use std::fmt;
use std::pin::Pin;
use std::time::Duration;

use crate::engine::core::abort_on_expiry::AbortOnExpiry;
use crate::engine::core::abort_source::{AbortRequestedException, AbortSource};
use crate::engine::core::abortable_fifo::internal::{AbortableFifo, Aborter};
use crate::engine::core::future::{
    Exception, ExceptionPtr, Future, Futurize, FuturizeT, InvalidArgument, Promise,
    futurize_invoke, make_exception_future, make_exception_ptr, make_ready_future,
};
use crate::engine::core::sstring::Sstring;
use crate::engine::core::timed_out_error::TimedOutError;
use crate::engine::core::timer::{SteadyClockType, TimerClock};

macro_rules! simple_exception {
    ($(#[$m:meta])* $name:ident, $what:literal) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, Default)]
        pub struct $name;
        impl $name {
            /// Reports the exception reason.
            pub fn what(&self) -> &'static str {
                $what
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.what())
            }
        }
        impl std::error::Error for $name {}
    };
}

simple_exception!(
    /// Error delivered to waiters of a semaphore broken by [`BasicSemaphore::broken`].
    BrokenSemaphore,
    "Semaphore broken"
);

simple_exception!(
    /// Error delivered when a timed semaphore wait ([`BasicSemaphore::wait_until`],
    /// [`BasicSemaphore::wait_for`]) times out.
    SemaphoreTimedOut,
    "Semaphore timedout"
);

simple_exception!(
    /// Error delivered when an abortable semaphore wait ([`BasicSemaphore::wait_abortable`])
    /// is aborted without an error of its own.
    SemaphoreAborted,
    "Semaphore aborted"
);

/// Produces the errors of a semaphore: it has to provide `timeout()` and may override
/// `broken()` and `aborted()`, whose defaults are [`BrokenSemaphore`] and
/// [`SemaphoreAborted`].
pub trait SemaphoreExceptionFactory {
    /// The error of a timed-out wait.
    fn timeout(&self) -> ExceptionPtr;
    /// The error of a wait on a broken semaphore.
    fn broken(&self) -> ExceptionPtr {
        make_exception_ptr(BrokenSemaphore)
    }
    /// The error of an aborted wait.
    fn aborted(&self) -> ExceptionPtr {
        ExceptionPtr::new_derived(SemaphoreAborted)
    }
}

/// Error factory of the standard semaphore: builds [`SemaphoreTimedOut`],
/// [`BrokenSemaphore`] and [`SemaphoreAborted`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SemaphoreDefaultExceptionFactory;

impl SemaphoreExceptionFactory for SemaphoreDefaultExceptionFactory {
    fn timeout(&self) -> ExceptionPtr {
        ExceptionPtr::new_derived(SemaphoreTimedOut)
    }
}

macro_rules! named_exception {
    ($(#[$m:meta])* $name:ident, $prefix:literal, $fallback:literal) => {
        $(#[$m])*
        #[derive(Debug, Clone, Default)]
        pub struct $name {
            msg: Sstring,
        }
        impl $name {
            /// Builds the exception for the semaphore named `msg`.
            pub fn new(msg: &str) -> Self {
                Self { msg: Sstring::from(format!(concat!($prefix, ": {}"), msg).as_str()) }
            }
            /// Reports the exception reason.
            pub fn what(&self) -> &str {
                // return a static message if generating the dynamic message failed.
                if self.msg.is_empty() { $fallback } else { self.msg.as_str() }
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.what())
            }
        }
        impl std::error::Error for $name {}
    };
}

named_exception!(
    /// [`SemaphoreTimedOut`] for a named semaphore; its message includes the name.
    NamedSemaphoreTimedOut,
    "Semaphore timed out",
    "Named semaphore timed out"
);
named_exception!(
    /// [`BrokenSemaphore`] for a named semaphore; its message includes the name.
    BrokenNamedSemaphore,
    "Semaphore broken",
    "Broken named semaphore"
);
named_exception!(
    /// [`SemaphoreAborted`] for a named semaphore; its message includes the name.
    NamedSemaphoreAborted,
    "Semaphore aborted",
    "Named semaphore aborted"
);

// The base errors of the semaphore errors, so that a handler looking for a base finds them:
// SemaphoreTimedOut -> TimedOutError, SemaphoreAborted -> AbortRequestedException, and each
// named error -> its plain counterpart. The base values carry no state; their `what()` is
// the base's own message.
macro_rules! semaphore_exception_base {
    ($name:ident : $base:ident = $value:expr $(, $grand:ident = $gvalue:expr)?) => {
        impl crate::engine::core::future::DerivedException for $name {
            type Base = $base;
            #[inline]
            fn base(&self) -> &$base {
                &$value
            }
            #[inline]
            fn find_base(&self, ty: std::any::TypeId) -> Option<&dyn std::any::Any> {
                if ty == std::any::TypeId::of::<$base>() {
                    return Some(self.base());
                }
                $(if ty == std::any::TypeId::of::<$grand>() {
                    return Some(&$gvalue);
                })?
                None
            }
        }
    };
}

semaphore_exception_base!(SemaphoreTimedOut: TimedOutError = TimedOutError);
semaphore_exception_base!(SemaphoreAborted: AbortRequestedException = AbortRequestedException);
semaphore_exception_base!(NamedSemaphoreTimedOut: SemaphoreTimedOut = SemaphoreTimedOut, TimedOutError = TimedOutError);
semaphore_exception_base!(BrokenNamedSemaphore: BrokenSemaphore = BrokenSemaphore);
semaphore_exception_base!(
    NamedSemaphoreAborted: SemaphoreAborted = SemaphoreAborted,
    AbortRequestedException = AbortRequestedException
);

/// A factory of semaphore errors that carry additional context: the semaphore's name, for
/// example `NamedSemaphore::with_factory(0, NamedSemaphoreExceptionFactory { name })`.
#[derive(Debug, Clone, Default)]
pub struct NamedSemaphoreExceptionFactory {
    /// The semaphore name.
    pub name: Sstring,
}

impl SemaphoreExceptionFactory for NamedSemaphoreExceptionFactory {
    fn timeout(&self) -> ExceptionPtr {
        ExceptionPtr::new_derived(NamedSemaphoreTimedOut::new(self.name.as_str()))
    }
    fn broken(&self) -> ExceptionPtr {
        ExceptionPtr::new_derived(BrokenNamedSemaphore::new(self.name.as_str()))
    }
    fn aborted(&self) -> ExceptionPtr {
        ExceptionPtr::new_derived(NamedSemaphoreAborted::new(self.name.as_str()))
    }
}

struct Entry<C: TimerClock> {
    pr: Promise<()>,
    nr: usize,
    timer: Option<AbortOnExpiry<C>>,
}

/// The aborter of the wait list: fails an expired or aborted waiter with the right error.
/// The back pointer is set by the semaphore each time it queues a waiter (the semaphore is
/// only at its final address once it is in use).
struct ExpiryHandler<F: SemaphoreExceptionFactory + 'static, C: TimerClock> {
    sem: *mut BasicSemaphore<F, C>,
}

impl<F: SemaphoreExceptionFactory + 'static, C: TimerClock> Aborter<Entry<C>>
    for ExpiryHandler<F, C>
{
    fn on_abort(&mut self, e: &mut Entry<C>, ex: Option<&ExceptionPtr>) {
        // SAFETY: the semaphore does not move while it has waiters (a contract of the timed
        // and abortable waits) and set this pointer when queueing.
        let sem = unsafe { &*self.sem };
        if e.timer.is_some() {
            e.pr.set_exception(sem.get_timeout_exception());
        } else if let Some(ex) = ex {
            e.pr.set_exception(ex.clone());
        } else if !sem.ex.is_null() {
            e.pr.set_exception(sem.ex.clone());
        } else {
            e.pr.set_exception(sem.get_aborted_exception());
        }
    }

    // After a waiter left the wait list because it timed out or was aborted, the waiters
    // that were behind it may now be served.
    unsafe fn after_abort(this: *mut Self) {
        // SAFETY: `this` is the live handler (guaranteed by the caller); its back pointer
        // was set when the waiter was queued and the semaphore does not move while it has
        // waiters. The handler is only read here, before the semaphore is borrowed.
        unsafe {
            let sem = (*this).sem;
            (*sem).wake_waiters();
        }
    }
}

/// Set from the wait path when built with `--cfg engine_semaphore_debug`, to catch a
/// semaphore that is moved or reassigned after it was used; empty otherwise.
#[derive(Debug, Default)]
struct UsedFlag {
    #[cfg(engine_semaphore_debug)]
    used: bool,
}

impl UsedFlag {
    #[inline]
    fn use_(&mut self) {
        #[cfg(engine_semaphore_debug)]
        {
            self.used = true;
        }
    }
}

/// Counted resource guard.
///
/// This is a standard computer science semaphore, adapted
/// for futures.  You can deposit units into a counter,
/// or take them away.  Taking units from the counter may wait
/// if not enough units are available.
///
/// To support exceptional conditions, a [`BasicSemaphore::broken`] method
/// is provided, which causes all current waiters to stop waiting,
/// with a failed future returned.  This allows causing all
/// fibers that are blocked on a semaphore to continue.  This is
/// similar to POSIX's `pthread_cancel()`, with [`BasicSemaphore::wait`] acting
/// as a cancellation point.
///
/// `F` customizes the errors on timeout, broken and aborted (see
/// [`SemaphoreExceptionFactory`]); `C` is the clock of timed waits.
pub struct BasicSemaphore<F: SemaphoreExceptionFactory + 'static, C: TimerClock = SteadyClockType> {
    factory: F,
    count: isize,
    ex: ExceptionPtr,
    wait_list: AbortableFifo<Entry<C>, ExpiryHandler<F, C>>,
    used: UsedFlag,
}

impl<F: SemaphoreExceptionFactory + 'static, C: TimerClock> BasicSemaphore<F, C> {
    fn get_timeout_exception(&self) -> ExceptionPtr {
        self.factory.timeout()
    }

    fn get_aborted_exception(&self) -> ExceptionPtr {
        self.factory.aborted()
    }

    #[inline]
    fn has_available_units(&self, nr: usize) -> bool {
        self.count >= 0 && (self.count as usize >= nr)
    }

    #[inline]
    fn may_proceed(&self, nr: usize) -> bool {
        self.has_available_units(nr) && self.wait_list.empty()
    }

    /// Returns the maximum number of units the semaphore counter can hold
    #[inline]
    pub const fn max_counter() -> usize {
        isize::MAX as usize
    }

    /// Constructs a semaphore object with a specific number of units
    /// in its internal counter. E.g., starting it at 1 is suitable for use as
    /// an unlocked mutex.
    ///
    /// `count` is the number of initial units present in the counter.
    #[inline]
    pub fn new(count: usize) -> Self
    where
        F: Default,
    {
        Self::with_factory(count, F::default())
    }

    /// Constructs a semaphore with `count` initial units and the error factory `factory`.
    #[inline]
    pub fn with_factory(count: usize, factory: F) -> Self {
        Self {
            factory,
            count: count as isize,
            ex: ExceptionPtr::null(),
            wait_list: AbortableFifo::new(ExpiryHandler { sem: std::ptr::null_mut() }),
            used: UsedFlag::default(),
        }
    }

    /// Points the expiry handler at this semaphore.
    #[inline]
    fn bind(&mut self) {
        let me: *mut Self = self;
        self.wait_list.on_abort_mut().sem = me;
    }

    /// Serves the waiters at the front of the wait list, in order, while the available
    /// units are enough for them.
    fn wake_waiters(&mut self) {
        while !self.wait_list.empty() && self.has_available_units(self.wait_list.front_ref().nr) {
            let x = self.wait_list.front();
            self.count -= x.nr as isize;
            x.pr.set_value(());
            self.wait_list.pop_front();
        }
    }

    /// Waits until at least a specific number of units are available in the
    /// counter, and reduces the counter by that amount of units.
    ///
    /// Waits are serviced in FIFO order, though if several are awakened
    /// at once, they may be reordered by the scheduler.
    ///
    /// `nr` is the amount of units to wait for. Returns a future that becomes ready when
    /// sufficient units are available to satisfy the request. If the semaphore was
    /// [broken](BasicSemaphore::broken), the future fails with the broken error.
    #[inline]
    pub fn wait(&mut self, nr: usize) -> Future<()> {
        // A wait without a timeout is never made abortable, so nothing points back at the
        // semaphore or at its wait list.
        self.wait_impl(C::max_time_point(), nr)
    }

    /// Waits until at least a specific number of units are available in the
    /// counter, and reduces the counter by that amount of units.  If the request
    /// cannot be satisfied in time, the request is aborted.
    ///
    /// Waits are serviced in FIFO order, though if several are awakened
    /// at once, they may be reordered by the scheduler.
    ///
    /// `timeout` is the expiration time and `nr` the amount of units to wait for. Returns a
    /// future that becomes ready when sufficient units are available to satisfy the request.
    /// On timeout, the future fails with the factory's timeout error ([`SemaphoreTimedOut`]
    /// by default). If the semaphore was broken, it fails with the broken error.
    ///
    /// # Safety
    /// The semaphore must not move nor be dropped while the wait is pending: its timer
    /// points back at the wait list.
    pub unsafe fn wait_until(&mut self, timeout: C::TimePoint, nr: usize) -> Future<()> {
        self.wait_impl(timeout, nr)
    }

    /// The body of `wait_until`; with the clock's maximum time point as `timeout`, no timer
    /// is armed and nothing points back at the semaphore.
    fn wait_impl(&mut self, timeout: C::TimePoint, nr: usize) -> Future<()> {
        self.used.use_();
        if self.may_proceed(nr) {
            self.count -= nr as isize;
            return make_ready_future(());
        }
        if !self.ex.is_null() {
            return make_exception_future(self.ex.clone());
        }
        if C::now() >= timeout {
            return make_exception_future(self.get_timeout_exception());
        }
        self.bind();
        let e = self.wait_list.emplace_back(Entry { pr: Promise::new(), nr, timer: None });
        let f = e.pr.get_future();
        if timeout != C::max_time_point() {
            let t = e.timer.insert(AbortOnExpiry::new(timeout));
            // SAFETY: the abort_on_expiry lives in the entry, which does not move while
            // it is in the wait list and is dropped in place, so it may be pinned there
            // (pinning it arms its timer); the abort source is pinned with it. A timed
            // wait only comes from wait_until, whose caller keeps the semaphore in place.
            unsafe {
                let as_: *mut AbortSource =
                    Pin::new_unchecked(t).abort_source().get_unchecked_mut();
                self.wait_list.make_back_abortable(Pin::new_unchecked(&mut *as_));
            }
        }
        f
    }

    /// Waits until at least a specific number of units are available in the
    /// counter, and reduces the counter by that amount of units.  The request
    /// can be aborted using an [`AbortSource`].
    ///
    /// Waits are serviced in FIFO order, though if several are awakened
    /// at once, they may be reordered by the scheduler.
    ///
    /// `as_` is the abort source and `nr` the amount of units to wait for. Returns a future
    /// that becomes ready when sufficient units are available to satisfy the request. On
    /// abort, the future fails with the abort's error, or the factory's aborted error
    /// ([`SemaphoreAborted`] by default) when the abort carries none. If the semaphore was
    /// broken, it fails with the broken error.
    ///
    /// # Safety
    /// As [`BasicSemaphore::wait_until`]: the abort subscription points back at the wait
    /// list and the semaphore.
    pub unsafe fn wait_abortable(&mut self, as_: Pin<&mut AbortSource>, nr: usize) -> Future<()> {
        self.used.use_();
        if self.may_proceed(nr) {
            self.count -= nr as isize;
            return make_ready_future(());
        }
        if !self.ex.is_null() {
            return make_exception_future(self.ex.clone());
        }
        if as_.abort_requested() {
            return make_exception_future(self.get_aborted_exception());
        }
        self.bind();
        let e = self.wait_list.emplace_back(Entry { pr: Promise::new(), nr, timer: None });
        // taking future here since make_back_abortable may expire the entry
        let f = e.pr.get_future();
        // SAFETY: forwarded from the caller of wait_abortable.
        unsafe { self.wait_list.make_back_abortable(as_) };
        f
    }

    /// Waits until at least a specific number of units are available in the
    /// counter, and reduces the counter by that amount of units.  If the request
    /// cannot be satisfied in time, the request is aborted.
    ///
    /// Waits are serviced in FIFO order, though if several are awakened
    /// at once, they may be reordered by the scheduler.
    ///
    /// `timeout` is how long to wait and `nr` the amount of units to wait for. The result is
    /// as for [`BasicSemaphore::wait_until`].
    ///
    /// # Safety
    /// As [`BasicSemaphore::wait_until`].
    #[inline]
    pub unsafe fn wait_for(&mut self, timeout: Duration, nr: usize) -> Future<()> {
        // SAFETY: forwarded from the caller.
        unsafe { self.wait_until(C::add(C::now(), timeout), nr) }
    }

    /// Deposits a specified number of units into the counter.
    ///
    /// The counter is incremented by the specified number of units.
    /// If the new counter value is sufficient to satisfy the request
    /// of one or more waiters, their futures (in FIFO order) become
    /// ready, and the value of the counter is reduced according to
    /// the amount requested.
    ///
    /// `nr` is the number of units to deposit. Does nothing on a broken semaphore.
    pub fn signal(&mut self, nr: usize) {
        if !self.ex.is_null() {
            return;
        }
        self.count += nr as isize;
        self.wake_waiters();
    }

    /// Consumes `nr` units without blocking.
    ///
    /// Consumes the units now, regardless of how many units are available in the counter,
    /// and reduces the counter by that amount. This operation may cause the counter to go
    /// negative. Does nothing on a broken semaphore.
    #[inline]
    pub fn consume(&mut self, nr: usize) {
        self.used.use_();
        if !self.ex.is_null() {
            return;
        }
        self.count -= nr as isize;
    }

    /// Attempts to reduce the counter value by a specified number of units.
    ///
    /// If sufficient units are available in the counter, and if no
    /// other fiber is waiting, then the counter is reduced.  Otherwise,
    /// nothing happens.  This is useful for "opportunistic" waits where
    /// useful work can happen if the counter happens to be ready, but
    /// when it is not worthwhile to wait.
    ///
    /// `nr` is the number of units to reduce the counter by. Returns `true` if the counter
    /// had sufficient units, and was decremented.
    #[inline]
    pub fn try_wait(&mut self, nr: usize) -> bool {
        self.used.use_();
        if self.may_proceed(nr) {
            self.count -= nr as isize;
            true
        } else {
            false
        }
    }

    /// Returns the number of units available in the counter.
    ///
    /// Does not take into account any waiters.
    #[inline]
    pub fn current(&self) -> usize {
        self.count.max(0) as usize
    }

    /// Returns the number of available units.
    ///
    /// Takes into account units consumed using [`BasicSemaphore::consume`] and therefore
    /// may return a negative value.
    #[inline]
    pub fn available_units(&self) -> isize {
        self.count
    }

    /// Returns the current number of waiters
    #[inline]
    pub fn waiters(&self) -> usize {
        self.wait_list.size()
    }

    /// Signals to waiters that an error occurred. [`BasicSemaphore::wait`] will see a failed
    /// future holding the factory's broken error ([`BrokenSemaphore`] by default). The future
    /// is made available immediately, for current and later waits.
    pub fn broken(&mut self) {
        let ep = self.factory.broken();
        self.broken_with(ep);
    }

    /// Signals to waiters that an error occurred. [`BasicSemaphore::wait`] will see a failed
    /// future holding `ex`. The future is made available immediately.
    pub fn broken_with_exception<E: Exception>(&mut self, ex: E) {
        self.broken_with(make_exception_ptr(ex));
    }

    /// Signals to waiters that an error occurred. [`BasicSemaphore::wait`] will see a failed
    /// future holding `xp`. The future is made available immediately.
    pub fn broken_with(&mut self, xp: ExceptionPtr) {
        self.ex = xp.clone();
        self.count = 0;
        while !self.wait_list.empty() {
            let x = self.wait_list.front();
            x.pr.set_exception(xp.clone());
            self.wait_list.pop_front();
        }
    }

    /// Reserves memory for `n` waiters so that waits do not allocate.
    #[inline]
    pub fn ensure_space_for_waiters(&mut self, n: usize) {
        self.wait_list.reserve(n);
    }

    /// Move-assigns a semaphore object from a moved-from semaphore object,
    /// inheriting the number of units from the moved-from semaphore.
    /// The number of units of the assigned semaphore is overwritten by the
    /// the moved-from number of units.
    /// Both the moved-to and moved-from semaphores must be unused.
    ///
    /// # Panics
    /// Panics if either semaphore has waiters.
    pub fn assign_from(&mut self, other: &mut Self)
    where
        F: Clone,
    {
        // semaphore cannot be moved with non-empty waiting list
        assert!(self.wait_list.empty());
        assert!(other.wait_list.empty());
        if !std::ptr::eq(self, other) {
            self.factory = other.factory.clone();
            self.count = other.count;
            self.ex = std::mem::take(&mut other.ex);
            self.used = std::mem::take(&mut other.used);
        }
    }
}

/// Units taken from a semaphore, returned to it when dropped.
pub struct SemaphoreUnits<
    F: SemaphoreExceptionFactory + 'static = SemaphoreDefaultExceptionFactory,
    C: TimerClock = SteadyClockType,
> {
    sem: *mut BasicSemaphore<F, C>,
    n: usize,
}

impl<F: SemaphoreExceptionFactory + 'static, C: TimerClock> Default for SemaphoreUnits<F, C> {
    fn default() -> Self {
        Self { sem: std::ptr::null_mut(), n: 0 }
    }
}

impl<F: SemaphoreExceptionFactory + 'static, C: TimerClock> SemaphoreUnits<F, C> {
    /// Wraps `n` units already taken from `sem`.
    ///
    /// # Safety
    /// The semaphore must outlive the units and stay at its address while they exist,
    /// and no reference to it may be live when the units return to it.
    #[inline]
    pub unsafe fn new(sem: *mut BasicSemaphore<F, C>, n: usize) -> Self {
        Self { sem, n }
    }

    #[inline]
    fn sem(&mut self) -> &mut BasicSemaphore<F, C> {
        // SAFETY: the caller of new() guarantees the semaphore outlives the units.
        unsafe { &mut *self.sem }
    }

    /// Return ownership of some units to the semaphore. The semaphore will be signaled by the number of units returned.
    ///
    /// `units` is the number of units to return. Returns the number of remaining units.
    ///
    /// # Errors
    /// Fails with an invalid-argument error if `units` is more than those held.
    pub fn return_units(&mut self, units: usize) -> Result<usize, ExceptionPtr> {
        if units > self.n {
            return Err(make_exception_ptr(InvalidArgument(
                "Cannot take more units than those protected by the semaphore".into(),
            )));
        }
        self.n -= units;
        self.sem().signal(units);
        Ok(self.n)
    }

    /// Return ownership of all units. The semaphore will be signaled by the number of units returned.
    #[inline]
    pub fn return_all(&mut self) {
        if self.n != 0 {
            let n = self.n;
            self.sem().signal(n);
            self.n = 0;
        }
    }

    /// Releases ownership of the units. The semaphore will not be signalled.
    ///
    /// Returns the number of units held.
    #[inline]
    pub fn release(&mut self) -> usize {
        std::mem::take(&mut self.n)
    }

    /// Splits off a new [`SemaphoreUnits`] holding `units` units; this object keeps holding
    /// the remaining units.
    ///
    /// # Errors
    /// Fails with an invalid-argument error if `units` is more than those held.
    pub fn split(&mut self, units: usize) -> Result<Self, ExceptionPtr> {
        if units > self.n {
            return Err(make_exception_ptr(InvalidArgument(
                "Cannot take more units than those protected by the semaphore".into(),
            )));
        }
        self.n -= units;
        Ok(Self { sem: self.sem, n: units })
    }

    /// The inverse of [`SemaphoreUnits::split`]: the units held by `other` are merged into
    /// this object.
    ///
    /// # Panics
    /// Panics if the two are not associated with the same semaphore.
    pub fn adopt(&mut self, mut other: Self) {
        assert!(other.sem == self.sem);
        self.n += other.release();
    }

    /// Returns the number of units held
    #[inline]
    pub fn count(&self) -> usize {
        self.n
    }

    /// Returns true if and only if any units are held.
    #[inline]
    pub fn is_some(&self) -> bool {
        self.n != 0
    }
}

impl<F: SemaphoreExceptionFactory + 'static, C: TimerClock> Drop for SemaphoreUnits<F, C> {
    #[inline]
    fn drop(&mut self) {
        self.return_all();
    }
}

/// Takes units from a semaphore temporarily.
///
/// Takes `units` units from `sem` and returns them when the [`SemaphoreUnits`] are dropped.
/// This provides a safe way to temporarily take units from a semaphore and ensure that they
/// are eventually returned in every case (errors, early returns, and so on).
///
/// Unlike [`with_semaphore`], the scope of unit holding is not limited to a single
/// asynchronous function.
///
/// Returns a future of the units; it fails as [`BasicSemaphore::wait`] does.
///
/// # Safety
/// The semaphore must outlive the returned units (and, for a timed or abortable wait, the
/// returned future), stay at its address meanwhile, and not be borrowed when the units
/// return to it (see [`SemaphoreUnits::new`]).
pub unsafe fn get_units<F: SemaphoreExceptionFactory + 'static, C: TimerClock>(
    sem: &mut BasicSemaphore<F, C>,
    units: usize,
) -> Future<SemaphoreUnits<F, C>> {
    let p: *mut BasicSemaphore<F, C> = sem;
    sem.wait(units).then(move |()| Ok(SemaphoreUnits { sem: p, n: units }))
}

/// Takes units from a semaphore temporarily, with a deadline on the wait.
///
/// Like [`get_units`], but when `timeout` is reached before the units are granted, the
/// future fails with the factory's timeout error ([`SemaphoreTimedOut`] by default).
///
/// # Safety
/// The semaphore must outlive the returned units (and, for a timed or abortable wait, the
/// returned future), stay at its address meanwhile, and not be borrowed when the units
/// return to it (see [`SemaphoreUnits::new`]).
pub unsafe fn get_units_until<F: SemaphoreExceptionFactory + 'static, C: TimerClock>(
    sem: &mut BasicSemaphore<F, C>,
    units: usize,
    timeout: C::TimePoint,
) -> Future<SemaphoreUnits<F, C>> {
    let p: *mut BasicSemaphore<F, C> = sem;
    // SAFETY: forwarded from the caller.
    unsafe { sem.wait_until(timeout, units) }
        .then(move |()| Ok(SemaphoreUnits { sem: p, n: units }))
}

/// Takes units from a semaphore temporarily, with a time bound on the wait.
///
/// Like [`get_units_until`], but `timeout` is a duration from now.
///
/// # Safety
/// The semaphore must outlive the returned units (and, for a timed or abortable wait, the
/// returned future), stay at its address meanwhile, and not be borrowed when the units
/// return to it (see [`SemaphoreUnits::new`]).
pub unsafe fn get_units_for<F: SemaphoreExceptionFactory + 'static, C: TimerClock>(
    sem: &mut BasicSemaphore<F, C>,
    units: usize,
    timeout: Duration,
) -> Future<SemaphoreUnits<F, C>> {
    let p: *mut BasicSemaphore<F, C> = sem;
    // SAFETY: forwarded from the caller.
    unsafe { sem.wait_for(timeout, units) }.then(move |()| Ok(SemaphoreUnits { sem: p, n: units }))
}

/// Takes units from a semaphore temporarily, with an [`AbortSource`].
///
/// Like [`get_units`], but the wait can be aborted with `as_`; the future then fails as
/// [`BasicSemaphore::wait_abortable`] describes.
///
/// # Safety
/// The semaphore must outlive the returned units (and, for a timed or abortable wait, the
/// returned future), stay at its address meanwhile, and not be borrowed when the units
/// return to it (see [`SemaphoreUnits::new`]).
pub unsafe fn get_units_abortable<F: SemaphoreExceptionFactory + 'static, C: TimerClock>(
    sem: &mut BasicSemaphore<F, C>,
    units: usize,
    as_: Pin<&mut AbortSource>,
) -> Future<SemaphoreUnits<F, C>> {
    let p: *mut BasicSemaphore<F, C> = sem;
    // SAFETY: forwarded from the caller.
    unsafe { sem.wait_abortable(as_, units) }
        .then(move |()| Ok(SemaphoreUnits { sem: p, n: units }))
}

/// Tries to take units from a semaphore temporarily, without waiting.
///
/// Takes `units` units from `sem` if they are available now (see
/// [`BasicSemaphore::try_wait`]) and returns them when the [`SemaphoreUnits`] are dropped.
/// Returns `None` if the units are not available.
///
/// # Safety
/// The semaphore must outlive the returned units (and, for a timed or abortable wait, the
/// returned future), stay at its address meanwhile, and not be borrowed when the units
/// return to it (see [`SemaphoreUnits::new`]).
pub unsafe fn try_get_units<F: SemaphoreExceptionFactory + 'static, C: TimerClock>(
    sem: &mut BasicSemaphore<F, C>,
    units: usize,
) -> Option<SemaphoreUnits<F, C>> {
    if !sem.try_wait(units) {
        return None;
    }
    // SAFETY: forwarded from the caller.
    Some(unsafe { SemaphoreUnits::new(sem, units) })
}

/// Consumes units from a semaphore temporarily.
///
/// Consumes `units` units from `sem` with the non-blocking [`BasicSemaphore::consume`]
/// (the counter may go negative) and returns them when the [`SemaphoreUnits`] are dropped.
///
/// # Safety
/// The semaphore must outlive the returned units (and, for a timed or abortable wait, the
/// returned future), stay at its address meanwhile, and not be borrowed when the units
/// return to it (see [`SemaphoreUnits::new`]).
pub unsafe fn consume_units<F: SemaphoreExceptionFactory + 'static, C: TimerClock>(
    sem: &mut BasicSemaphore<F, C>,
    units: usize,
) -> SemaphoreUnits<F, C> {
    sem.consume(units);
    // SAFETY: forwarded from the caller.
    unsafe { SemaphoreUnits::new(sem, units) }
}

/// Runs a function protected by a semaphore.
///
/// Acquires `units` units of `sem` (as with [`BasicSemaphore::wait`]), runs `func`, and
/// releases the units when its future resolves. Returns a future holding the function's
/// result or error, or a failed future with the broken error if the semaphore was broken.
///
/// # Safety
/// The semaphore must outlive the returned future and stay at its address meanwhile
/// (see [`get_units`]).
pub unsafe fn with_semaphore<F, C, Func, R>(
    sem: &mut BasicSemaphore<F, C>,
    units: usize,
    func: Func,
) -> FuturizeT<R>
where
    F: SemaphoreExceptionFactory + 'static,
    C: TimerClock,
    Func: FnOnce() -> R + 'static,
    R: Futurize,
{
    // SAFETY: forwarded from the caller.
    unsafe { get_units(sem, units) }
        .then(move |units| futurize_invoke(func).finally(move || drop(units)))
}

/// Runs a function protected by a semaphore, with a time bound on the wait.
///
/// Like [`with_semaphore`], but if the units cannot be acquired within `timeout`, the
/// result fails with the factory's timeout error ([`SemaphoreTimedOut`] by default) and
/// `func` is not run.
///
/// # Safety
/// The semaphore must outlive the returned future and stay at its address meanwhile
/// (see [`get_units`]).
pub unsafe fn with_semaphore_for<F, C, Func, R>(
    sem: &mut BasicSemaphore<F, C>,
    units: usize,
    timeout: Duration,
    func: Func,
) -> FuturizeT<R>
where
    F: SemaphoreExceptionFactory + 'static,
    C: TimerClock,
    Func: FnOnce() -> R + 'static,
    R: Futurize,
{
    // SAFETY: forwarded from the caller.
    unsafe { get_units_for(sem, units, timeout) }
        .then(move |units| futurize_invoke(func).finally(move || drop(units)))
}

/// The default semaphore: steady-clock deadlines and the standard semaphore errors.
pub type Semaphore = BasicSemaphore<SemaphoreDefaultExceptionFactory>;
/// A semaphore whose errors carry its name.
pub type NamedSemaphore = BasicSemaphore<NamedSemaphoreExceptionFactory>;
