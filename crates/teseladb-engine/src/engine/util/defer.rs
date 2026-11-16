//! Scope guards.
//!
//! [`defer`] returns a [`DeferredAction`] that runs a closure when it is dropped, unless
//! it was cancelled first. It is the usual way to release a resource on every exit path of
//! a block, including early returns and `?`.

/// A callable that can be deferred: any `FnOnce()` returning nothing.
pub trait DeferrableAction: FnOnce() {}
impl<F: FnOnce()> DeferrableAction for F {}

/// Runs its closure exactly once when dropped, unless [`cancel`](Self::cancel) was called.
#[must_use]
pub struct DeferredAction<Func: FnOnce()> {
    _func: Option<Func>,
    _cancelled: bool,
}

impl<Func: FnOnce()> DeferredAction<Func> {
    /// Creates a guard that will run `func` on drop.
    pub fn new(func: Func) -> Self {
        DeferredAction { _func: Some(func), _cancelled: false }
    }

    /// Replaces this guard with `o`: first runs this guard's action (unless cancelled),
    /// then takes over `o`'s action and cancelled state. `o` is left cancelled.
    pub fn assign(&mut self, mut o: DeferredAction<Func>) {
        if !self._cancelled
            && let Some(f) = self._func.take()
        {
            f();
        }
        self._func = o._func.take();
        self._cancelled = o._cancelled;
        o._cancelled = true;
    }

    /// Prevents the action from running on drop.
    pub fn cancel(&mut self) {
        self._cancelled = true;
    }
}

impl<Func: FnOnce()> Drop for DeferredAction<Func> {
    fn drop(&mut self) {
        if !self._cancelled
            && let Some(f) = self._func.take()
        {
            f();
        }
    }
}

/// Returns a guard that runs `func` when it goes out of scope.
#[inline]
pub fn defer<Func: FnOnce()>(func: Func) -> DeferredAction<Func> {
    DeferredAction::new(func)
}
