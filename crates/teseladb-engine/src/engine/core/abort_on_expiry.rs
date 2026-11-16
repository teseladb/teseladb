//! Ties a deadline to an [`AbortSource`], so that abortable fibers also support timeouts.
//!
//! [`AbortOnExpiry`] owns a timer and an abort source on the current shard. When the timer
//! reaches its deadline it requests an abort with a [`TimedOutError`], and every subscriber of
//! the abort source is notified as if the abort had been requested by hand.

use std::marker::PhantomPinned;
use std::pin::Pin;

use crate::engine::core::abort_source::AbortSource;
use crate::engine::core::future::ExceptionPtr;
use crate::engine::core::lowres_clock::LowresClock;
use crate::engine::core::timed_out_error::TimedOutError;
use crate::engine::core::timer::{Timer, TimerClock};

/// A timer and an abort source that is aborted with [`TimedOutError`] when the timer expires.
///
/// The timer callback points back at the abort source, so the object must not move once armed:
/// it is `!Unpin`. [`AbortOnExpiry::new`] only records the deadline, and the first
/// [`AbortOnExpiry::abort_source`] call, made on the pinned object, arms the timer. Nothing is
/// allocated. Dropping the object cancels the timer.
pub struct AbortOnExpiry<C: TimerClock = LowresClock> {
    _tr: Timer<C>,
    _as: AbortSource,
    /// The deadline, until the timer is armed.
    _timeout: Option<C::TimePoint>,
    _pin: PhantomPinned,
}

impl<C: TimerClock> AbortOnExpiry<C> {
    /// Creates the timer and its abort source for the deadline `timeout`.
    ///
    /// When the deadline passes the abort source is aborted automatically. The timer is armed by
    /// the first call to [`AbortOnExpiry::abort_source`], once the object is pinned.
    pub fn new(timeout: C::TimePoint) -> Self {
        Self {
            _tr: Timer::new(),
            _as: AbortSource::new(),
            _timeout: Some(timeout),
            _pin: PhantomPinned,
        }
    }

    /// Returns the abort source tied to the deadline, arming the timer on the first call.
    pub fn abort_source(self: Pin<&mut Self>) -> Pin<&mut AbortSource> {
        // SAFETY: no field is moved out of the pinned object; the timer and the abort source
        // are pinned with it (structural pinning).
        let this = unsafe { self.get_unchecked_mut() };
        if let Some(timeout) = this._timeout.take() {
            let as_ptr: *mut AbortSource = &mut this._as;
            // SAFETY: as above.
            let mut tr = unsafe { Pin::new_unchecked(&mut this._tr) };
            tr.as_mut().set_callback(move || {
                // SAFETY: the timer and the abort source are fields of the same pinned object;
                // the timer is dropped (cancelled) with it, so the callback never outlives
                // `_as`, which stays in place.
                unsafe { Pin::new_unchecked(&mut *as_ptr) }
                    .request_abort_ex(ExceptionPtr::new(TimedOutError::new()));
            });
            tr.arm(timeout, None);
        }
        // SAFETY: structural pinning of `_as`, as above.
        unsafe { Pin::new_unchecked(&mut this._as) }
    }
}
