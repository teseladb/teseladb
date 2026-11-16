//! Timers that run a callback at a time point in the future, on the shard that armed them.
//!
//! Timers are provided for
//! [`LowresClock`](crate::engine::core::lowres_clock::LowresClock) (about 10 ms resolution,
//! efficient), for the steady clock [`SteadyClockType`] (accurate but less efficient) and for
//! [`ManualClock`](crate::engine::core::manual_clock::ManualClock) (for tests).
//!
//! Timers are optimized for cancellation; that is, adding a timer and cancelling
//! it is very efficient. This means that attaching a timer per object for
//! a timeout that rarely happens is reasonable; one does not have to maintain
//! a single timer and a sorted list for this use case.
//!
//! Timer callbacks should be short and execute quickly. If involved processing
//! is required, a timer can launch a continuation.
//!
//! The timer operations that link a timer into the reactor go through the [`TimerClock`]
//! trait, which the reactor implements for each clock with its own timer set.
//!
//! A timer holds its intrusive list hook inline (nothing is allocated) and the hook cannot be
//! fixed up after a move, so [`Timer`] is a pinned type: it moves freely while it has never
//! been armed, and every operation that can link it (`arm`, `rearm`, ...) takes it pinned
//! (`Pin<&mut Timer>`), so an armed timer stays in place until it is dropped, which cancels
//! it.

use std::ffi::c_int;
use std::pin::Pin;
use std::ptr::{self, NonNull};

use crate::engine::util::noncopyable_function::NoncopyableFunction;
use std::time::Duration;

use crate::engine::core::api::intrusive::ListHook;
use crate::engine::core::scheduling::{
    SchedulingGroup, current_scheduling_group, internal::current_scheduling_group_ptr,
};
pub use crate::engine::core::timer_set::{TimerSet, TimerSetEntry};

/// A clock usable by timers: its time point type, `now()` and the representation of time
/// points as a count of ticks since the epoch.
pub trait Clock: 'static {
    /// The time point type of the clock.
    type TimePoint: Copy + Ord + std::fmt::Debug;
    /// Returns the current time.
    fn now() -> Self::TimePoint;
    /// Converts a time point to ticks since the epoch.
    fn to_rep(tp: Self::TimePoint) -> i64;
    /// Converts ticks since the epoch to a time point.
    fn from_rep(rep: i64) -> Self::TimePoint;
    /// Returns `tp` advanced by `d`.
    fn add(tp: Self::TimePoint, d: Duration) -> Self::TimePoint;
    /// Returns the latest representable time point, used as "never".
    fn max_time_point() -> Self::TimePoint;
}

/// The reactor services for timers of a clock, implemented by the reactor for each clock.
pub trait TimerClock: Clock + Sized {
    /// Inserts an armed timer into the reactor's set for this clock and reprograms the
    /// reactor's wake-up if it is now the earliest.
    fn add_timer(tmr: NonNull<TimerImpl<Self>>);
    /// Inserts a timer into the set without reprogramming the wake-up; returns whether it is
    /// now the earliest timer. Used when re-adding periodic timers during expiry.
    fn queue_timer(tmr: NonNull<TimerImpl<Self>>) -> bool;
    /// Removes a queued timer from the reactor's set (or from the expired list).
    fn del_timer(tmr: NonNull<TimerImpl<Self>>);
}

#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

unsafe extern "C" {
    fn clock_gettime(clk: c_int, tp: *mut Timespec) -> c_int;
}

const CLOCK_MONOTONIC: c_int = 1;

/// The steady clock of the operating system (`CLOCK_MONOTONIC`), the default timer clock.
#[derive(Clone, Copy, Debug)]
pub struct SteadyClockType;

/// A point of [`SteadyClockType`]: nanoseconds since an unspecified start (usually boot).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SteadyTimePoint(pub i64);

impl SteadyTimePoint {
    /// The latest representable time point.
    pub const MAX: Self = SteadyTimePoint(i64::MAX);
}

impl Clock for SteadyClockType {
    type TimePoint = SteadyTimePoint;
    #[inline]
    fn now() -> SteadyTimePoint {
        let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `ts` is a valid, writable timespec with the C layout; the call writes only
        // into it.
        unsafe { clock_gettime(CLOCK_MONOTONIC, &mut ts) };
        SteadyTimePoint(ts.tv_sec * 1_000_000_000 + ts.tv_nsec)
    }
    #[inline]
    fn to_rep(tp: SteadyTimePoint) -> i64 {
        tp.0
    }
    #[inline]
    fn from_rep(rep: i64) -> SteadyTimePoint {
        SteadyTimePoint(rep)
    }
    #[inline]
    fn add(tp: SteadyTimePoint, d: Duration) -> SteadyTimePoint {
        SteadyTimePoint(tp.0.saturating_add(i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)))
    }
    #[inline]
    fn max_time_point() -> SteadyTimePoint {
        SteadyTimePoint::MAX
    }
}

/// The callback of a timer: small closures (for example one capturing a single pointer) are
/// stored inline, without an allocation.
pub type TimerCallback = NoncopyableFunction<fn()>;

/// The state of a timer that the reactor's timer sets link. It is embedded in the [`Timer`]
/// and does not move while the timer is armed.
pub struct TimerImpl<C: Clock> {
    /// Hook into the reactor's timer set or expired list.
    link: ListHook,
    /// Scheduling group the callback runs under.
    sg: SchedulingGroup,
    /// The callback.
    callback: TimerCallback,
    /// Deadline of the armed timer.
    expiry: C::TimePoint,
    /// Re-arm period of a periodic timer.
    period: Option<Duration>,
    /// Armed and not yet expired or cancelled.
    armed: bool,
    /// Linked into the reactor's set or expired list.
    queued: bool,
    /// Moved to the expired list and not yet fired.
    expired: bool,
    /// While the callback runs (see `fire`): the flag that the destructor clears, so that
    /// `fire` knows the callback destroyed its own timer and must not touch it again.
    firing: *mut bool,
}

/// The intrusive-list adapter that links [`TimerImpl`]s through their `link` hook.
pub struct TimerLink<C: Clock>(std::marker::PhantomData<C>);

// SAFETY: OFFSET is the offset of the `link` hook of TimerImpl<C>.
unsafe impl<C: Clock> crate::engine::core::api::intrusive::Adapter for TimerLink<C> {
    type Value = TimerImpl<C>;
    const OFFSET: usize = std::mem::offset_of!(TimerImpl<C>, link);
}

/// The reactor's set of armed timers of clock `C`.
pub type TimerSetT<C> = TimerSet<TimerLink<C>>;

/// A list of timers of clock `C`, used by the reactor for its expired timers.
pub type TimerListT<C> = crate::engine::core::timer_set::TimerList<TimerLink<C>>;

impl<C: TimerClock> TimerImpl<C> {
    fn arm_state(&mut self, until: C::TimePoint, period: Option<Duration>) {
        assert!(!self.armed);
        self.period = period;
        self.armed = true;
        self.expired = false;
        self.expiry = until;
        self.queued = true;
    }

    /// Re-arms a periodic timer for its next period and queues it again.
    ///
    /// # Safety
    /// `this` must point to a live timer that stays in place while queued, with no
    /// reference to it live.
    unsafe fn readd_periodic(this: *mut Self) {
        // SAFETY: guaranteed by the caller; the short reference ends before the reactor
        // links the timer.
        unsafe {
            let p = (*this).period.unwrap_or_default();
            (*this).arm_state(C::add(C::now(), p), Some(p));
            C::queue_timer(NonNull::new_unchecked(this));
        }
    }

    /// Cancels the timer; returns whether it was armed.
    ///
    /// # Safety
    /// As [`TimerImpl::readd_periodic`] (the reactor unlinks the timer).
    unsafe fn cancel_raw(this: *mut Self) -> bool {
        // SAFETY: guaranteed by the caller.
        unsafe {
            if !(*this).armed {
                return false;
            }
            (*this).armed = false;
            if (*this).queued {
                C::del_timer(NonNull::new_unchecked(this));
                (*this).queued = false;
            }
            true
        }
    }

    /// Whether the timer is linked into a reactor set or expired list (reactor use).
    #[inline]
    pub fn queued(&self) -> bool {
        self.queued
    }
}

impl<C: TimerClock> TimerSetEntry for TimerImpl<C> {
    type TimePoint = C::TimePoint;
    #[inline]
    fn get_timeout(&self) -> C::TimePoint {
        self.expiry
    }
    #[inline]
    fn timestamp(tp: C::TimePoint) -> i64 {
        C::to_rep(tp)
    }
    #[inline]
    fn from_timestamp(ts: i64) -> C::TimePoint {
        C::from_rep(ts)
    }
    #[inline]
    fn now() -> C::TimePoint {
        C::now()
    }
    #[inline]
    fn expired(&self) -> bool {
        self.expired
    }
    #[inline]
    unsafe fn set_expired(this: *mut Self, v: bool) {
        // SAFETY: guaranteed by the caller.
        unsafe { (*this).expired = v };
    }
    /// Fires one expired timer: the step the reactor runs for each timer of its expired list.
    ///
    /// The callback may destroy the timer, or re-arm or cancel it and its neighbours, so
    /// the timer is only reached through `this`, and the callback is moved out of it for
    /// the call (calling a closure in place while it frees its own storage is undefined
    /// in Rust): it is moved back afterwards unless the timer was destroyed meanwhile or
    /// was given another callback.
    unsafe fn fire(this: *mut Self) {
        // SAFETY: the timer is live (it was just unlinked from the expired list); every
        // access goes through `this`, and none happens after the callback destroyed it.
        unsafe {
            (*this).queued = false;
            if (*this).armed {
                (*this).armed = false;
                if (*this).period.is_some() {
                    Self::readd_periodic(this);
                }
                // The callback runs under the timer's scheduling group. The pointer refers to
                // the thread-local current group.
                *current_scheduling_group_ptr() = (*this).sg;
                let mut cb = std::mem::replace(
                    &mut *ptr::addr_of_mut!((*this).callback),
                    TimerCallback::empty(),
                );
                let mut alive = true;
                let alive_p: *mut bool = &mut alive;
                (*this).firing = alive_p;
                cb.call();
                if *alive_p {
                    (*this).firing = ptr::null_mut();
                    let slot = &mut *ptr::addr_of_mut!((*this).callback);
                    if !slot.is_some() {
                        *slot = cb;
                    }
                }
            }
        }
    }
    #[inline]
    unsafe fn cancel(this: *mut Self) -> bool {
        // SAFETY: guaranteed by the caller.
        unsafe { TimerImpl::cancel_raw(this) }
    }
    #[inline]
    unsafe fn cancel_unlinked(this: *mut Self) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            (*this).armed = false;
            (*this).queued = false;
        }
    }
}

/// Runs a callback at a certain time point in the future.
///
/// Timer callbacks should execute quickly. If more involved computation
/// is required, the timer should launch it as a fiber (or signal an
/// existing fiber to continue execution). Fibers launched from a timer
/// callback are executed under the scheduling group that was current
/// when the timer was created (see `current_scheduling_group()`), or the
/// scheduling group that was given explicitly by the caller when the callback
/// was specified.
///
/// Expiration of a `Timer<SteadyClockType>` is independent of the task quota, so it has
/// relatively high accuracy, but as a result this is a relatively expensive timer. It is
/// recommended to use `Timer<LowresClock>` instead, which has very coarse resolution (about
/// 10 ms) but is quite efficient. It is suitable for most user timeouts.
///
/// `C` is the clock of the deadlines: [`SteadyClockType`] (default), `LowresClock` (more
/// efficient but with less resolution) or `ManualClock` (fine-grained control for tests).
pub struct Timer<C: TimerClock = SteadyClockType> {
    t: TimerImpl<C>,
}

impl<C: TimerClock> Default for Timer<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: TimerClock> Timer<C> {
    /// Constructs a timer with no callback set and no expiration time.
    pub fn new() -> Self {
        Self::make(current_scheduling_group(), TimerCallback::empty())
    }

    #[inline]
    fn make(sg: SchedulingGroup, callback: TimerCallback) -> Self {
        Self {
            t: TimerImpl {
                link: ListHook::new(),
                sg,
                callback,
                expiry: C::from_rep(0),
                period: None,
                armed: false,
                queued: false,
                expired: false,
                firing: ptr::null_mut(),
            },
        }
    }

    /// The timer state, for operations that do not move it, as a raw pointer: the
    /// reactor reaches a queued timer through its own pointers too.
    #[inline]
    fn state(self: Pin<&mut Self>) -> *mut TimerImpl<C> {
        // SAFETY: nothing is moved out of the pinned timer; the state is only mutated in
        // place.
        unsafe { ptr::addr_of_mut!(self.get_unchecked_mut().t) }
    }

    /// Constructs a timer with a callback. The timer is not armed.
    ///
    /// `sg` is the scheduling group to run the callback under, and `callback` the function to
    /// execute each time the armed timer expires.
    pub fn with_group<F: FnMut() + 'static>(sg: SchedulingGroup, callback: F) -> Self {
        Self::make(sg, TimerCallback::new(callback))
    }

    /// Constructs a timer with a callback, run under the current scheduling group. The timer is
    /// not armed.
    ///
    /// `callback` is the function to execute each time the armed timer expires.
    pub fn with_callback<F: FnMut() + 'static>(callback: F) -> Self {
        Self::with_group(current_scheduling_group(), callback)
    }

    /// Sets the callback function to be called when the timer expires, and the scheduling
    /// group `sg` under which it will run.
    pub fn set_callback_with_group<F: FnMut() + 'static>(
        self: Pin<&mut Self>,
        sg: SchedulingGroup,
        callback: F,
    ) {
        let t = self.state();
        // SAFETY: the pinned timer is live; the old callback is dropped here, unless it is
        // running (then fire() keeps it aside and drops it afterwards).
        unsafe {
            (*t).sg = sg;
            (*t).callback = TimerCallback::new(callback);
        }
    }

    /// Sets the callback function to be called when the timer expires, run under the current
    /// scheduling group.
    pub fn set_callback<F: FnMut() + 'static>(self: Pin<&mut Self>, callback: F) {
        self.set_callback_with_group(current_scheduling_group(), callback);
    }

    /// Sets the timer expiration time.
    ///
    /// `until` is the time when the timer expires. `period` is an optional automatic rearm
    /// duration; if given the timer will automatically rearm itself when it expires, using the
    /// period to calculate the next expiration time.
    ///
    /// # Panics
    /// Panics if the timer is already armed (and not disarmed by expiration or
    /// [`Timer::cancel`]). See [`Timer::rearm`].
    pub fn arm(self: Pin<&mut Self>, until: C::TimePoint, period: Option<Duration>) {
        let t = self.state();
        // SAFETY: the pinned timer is live and stays in place while queued; the short
        // reference ends before the reactor links it.
        unsafe {
            (*t).arm_state(until, period);
            C::add_timer(NonNull::new_unchecked(t));
        }
    }

    /// Sets the timer expiration time. If the timer was already armed, it is
    /// canceled first.
    ///
    /// `until` and `period` are as for [`Timer::arm`].
    pub fn rearm(mut self: Pin<&mut Self>, until: C::TimePoint, period: Option<Duration>) {
        if self.t.armed {
            self.as_mut().cancel();
        }
        self.arm(until, period);
    }

    /// Sets the timer expiration time.
    ///
    /// `delta` is the time when the timer expires, relative to now.
    ///
    /// # Panics
    /// Panics if the timer is already armed. See [`Timer::rearm`].
    pub fn arm_in(self: Pin<&mut Self>, delta: Duration) {
        self.arm(C::add(C::now(), delta), None);
    }

    /// Sets the timer expiration time, with automatic rearming.
    ///
    /// `delta` is the time when the timer expires, relative to now. The timer will also rearm
    /// automatically using the same delta time.
    ///
    /// # Panics
    /// Panics if the timer is already armed.
    pub fn arm_periodic(self: Pin<&mut Self>, delta: Duration) {
        self.arm(C::add(C::now(), delta), Some(delta));
    }

    /// Sets the timer expiration time, with automatic rearming.
    /// If the timer was already armed, it is canceled first.
    ///
    /// `delta` is as for [`Timer::arm_periodic`].
    pub fn rearm_periodic(mut self: Pin<&mut Self>, delta: Duration) {
        if self.t.armed {
            self.as_mut().cancel();
        }
        self.arm_periodic(delta);
    }

    /// Returns `true` if the timer is armed and has not expired yet.
    #[inline]
    pub fn armed(&self) -> bool {
        self.t.armed
    }

    /// Cancels an armed timer.
    ///
    /// If the timer was armed, it is disarmed. If the timer was not
    /// armed, does nothing.
    ///
    /// Returns `true` if the timer was armed before the call.
    #[inline]
    pub fn cancel(self: Pin<&mut Self>) -> bool {
        // SAFETY: the pinned timer is live and in place.
        unsafe { TimerImpl::cancel_raw(self.state()) }
    }

    /// Gets the expiration time of an armed timer.
    ///
    /// Returns the time at which the timer is scheduled to expire; meaningless if the timer is
    /// not armed.
    #[inline]
    pub fn get_timeout(&self) -> C::TimePoint {
        self.t.expiry
    }
}

impl<C: TimerClock> Drop for Timer<C> {
    /// Destroys the timer. The timer is cancelled if armed.
    fn drop(&mut self) {
        let t: *mut TimerImpl<C> = &mut self.t;
        // SAFETY: a queued timer was armed through a pin, so it is still at that address.
        // A non-null `firing` is the flag of the fire() running this timer's callback,
        // which is alive on the stack of fire().
        unsafe {
            if !(*t).firing.is_null() {
                *(*t).firing = false;
            }
            if (*t).queued {
                C::del_timer(NonNull::new_unchecked(t));
            }
        }
    }
}
