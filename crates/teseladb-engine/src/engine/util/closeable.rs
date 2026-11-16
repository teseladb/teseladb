//! Automatic close and stop of asynchronous resources.
//!
//! Objects that own asynchronous resources expose `close()` or `stop()` returning a
//! future. [`DeferredClose`] / [`DeferredStop`] call it when they are dropped and wait for
//! it (so they must be used inside an engine thread, where blocking on a future with
//! `.get()` is allowed). [`with_closeable`] / [`with_stoppable`] are the continuation-based
//! variants: they keep the object alive for the duration of a function and close or stop
//! it afterwards, whether the function succeeded or failed.

use crate::engine::core::do_with::do_with;
use crate::engine::core::future::{Future, Futurize, futurize_invoke};

/// An object that can be closed asynchronously.
pub trait Closeable {
    /// Starts closing the object; the future resolves when it is closed.
    fn close(&mut self) -> Future<()>;
}

/// Closes the referenced object when dropped, waiting for the close to finish.
///
/// Must be used in an engine thread, as the drop waits on the `close()` future with
/// `.get()`. The close's error, if any, is ignored.
#[must_use]
pub struct DeferredClose<'a, Object: Closeable> {
    _obj: &'a mut Object,
    _closed: bool,
}

impl<'a, Object: Closeable> DeferredClose<'a, Object> {
    fn do_close(&mut self) {
        if !self._closed {
            self._closed = true;
            let _ = self._obj.close().get();
        }
    }

    /// Creates a guard that will close `obj` when dropped.
    pub fn new(obj: &'a mut Object) -> Self {
        DeferredClose { _obj: obj, _closed: false }
    }

    /// Replaces this guard with `x`: closes the current object first (unless already
    /// closed or cancelled), then takes over `x`'s object and state. `x` is left cancelled.
    pub fn assign(&mut self, mut x: DeferredClose<'a, Object>) {
        self.do_close();
        self._closed = std::mem::replace(&mut x._closed, true);
        // Moving x's reference into self: swap the references, x is cancelled.
        std::mem::swap(&mut self._obj, &mut x._obj);
    }

    /// Closes the object now, waiting for it.
    ///
    /// # Panics
    ///
    /// Aborts the process (via [`engine_assert!`](crate::engine_assert)) if the object was
    /// already closed or the guard was cancelled.
    pub fn close_now(&mut self) {
        crate::engine_assert!(!self._closed);
        self.do_close();
    }

    /// Prevents `close()` from being called when the guard is dropped.
    /// [`close_now`](Self::close_now) may not be called after this.
    pub fn cancel(&mut self) {
        self._closed = true;
    }
}

/// Closes the object, unless already closed or cancelled.
impl<'a, Object: Closeable> Drop for DeferredClose<'a, Object> {
    fn drop(&mut self) {
        self.do_close();
    }
}

/// Moves `obj` into a stable location, calls `func` with it and then closes it.
///
/// The close runs whether `func` (or the future it returns) succeeds or fails. The
/// returned future resolves with `func`'s result once the close has finished.
pub fn with_closeable<C, Func, R>(obj: C, func: Func) -> Future<<R as Futurize>::Output>
where
    C: Closeable + 'static,
    Func: FnOnce(&mut C) -> R + 'static,
    R: Futurize,
{
    do_with(obj, move |obj: &mut C| {
        // Both the call and the final close() go through `p`, so the second use does not
        // follow a re-borrow of `obj` that would invalidate the pointer.
        let p = obj as *mut C;
        // SAFETY: do_with keeps the object alive and in place until the returned future
        // resolves; the reference lasts for the call only.
        futurize_invoke(move || func(unsafe { &mut *p })).finally(move || {
            // SAFETY: do_with keeps obj alive until the returned future resolves.
            unsafe { (*p).close() }
        })
    })
}

/// An object that can be stopped asynchronously.
pub trait Stoppable {
    /// Starts stopping the object; the future resolves when it is stopped.
    fn stop(&mut self) -> Future<()>;
}

/// Stops the referenced object when dropped, waiting for the stop to finish.
///
/// Must be used in an engine thread, as the drop waits on the `stop()` future with
/// `.get()`. The stop's error, if any, is ignored.
#[must_use]
pub struct DeferredStop<'a, Object: Stoppable> {
    _obj: &'a mut Object,
    _stopped: bool,
}

impl<'a, Object: Stoppable> DeferredStop<'a, Object> {
    fn do_stop(&mut self) {
        if !self._stopped {
            self._stopped = true;
            let _ = self._obj.stop().get();
        }
    }

    /// Creates a guard that will stop `obj` when dropped.
    pub fn new(obj: &'a mut Object) -> Self {
        DeferredStop { _obj: obj, _stopped: false }
    }

    /// Replaces this guard with `x`: stops the current object first (unless already
    /// stopped or cancelled), then takes over `x`'s object and state. `x` is left cancelled.
    pub fn assign(&mut self, mut x: DeferredStop<'a, Object>) {
        self.do_stop();
        self._stopped = std::mem::replace(&mut x._stopped, true);
        std::mem::swap(&mut self._obj, &mut x._obj);
    }

    /// Stops the object now, waiting for it.
    ///
    /// # Panics
    ///
    /// Aborts the process (via [`engine_assert!`](crate::engine_assert)) if the object was
    /// already stopped or the guard was cancelled.
    pub fn stop_now(&mut self) {
        crate::engine_assert!(!self._stopped);
        self.do_stop();
    }

    /// Prevents `stop()` from being called when the guard is dropped.
    /// [`stop_now`](Self::stop_now) may not be called after this.
    pub fn cancel(&mut self) {
        self._stopped = true;
    }
}

/// Stops the object, unless already stopped or cancelled.
impl<'a, Object: Stoppable> Drop for DeferredStop<'a, Object> {
    fn drop(&mut self) {
        self.do_stop();
    }
}

/// Moves `obj` into a stable location, calls `func` with it and then stops it.
///
/// The stop runs whether `func` (or the future it returns) succeeds or fails. The
/// returned future resolves with `func`'s result once the stop has finished.
pub fn with_stoppable<S, Func, R>(obj: S, func: Func) -> Future<<R as Futurize>::Output>
where
    S: Stoppable + 'static,
    Func: FnOnce(&mut S) -> R + 'static,
    R: Futurize,
{
    do_with(obj, move |obj: &mut S| {
        // Both the call and the final stop() go through `p`, so the second use does not
        // follow a re-borrow of `obj` that would invalidate the pointer.
        let p = obj as *mut S;
        // SAFETY: do_with keeps the object alive and in place until the returned future
        // resolves; the reference lasts for the call only.
        futurize_invoke(move || func(unsafe { &mut *p })).finally(move || {
            // SAFETY: do_with keeps obj alive until the returned future resolves.
            unsafe { (*p).stop() }
        })
    })
}
