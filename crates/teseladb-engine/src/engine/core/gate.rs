//! Gates: stop new requests to a service and wait for the requests in flight to finish.
//!
//! A [`Gate`] counts in-progress requests on one shard. Requests enter it with
//! [`Gate::enter`] / [`Gate::leave`], or hold it for a scope with a RAII [`Holder`] from
//! [`Gate::hold`]; [`with_gate`] and [`try_with_gate`] do the same around an asynchronous
//! function. [`Gate::close`] makes further entries fail with [`GateClosedException`] (or
//! [`NamedGateClosedException`] for a named gate) and returns a future that resolves once
//! the count drops to zero.
//!
//! Holders point at their gate, so the gate must be pinned to hand them out and must not be
//! dropped while any is alive (that aborts the process). Building with
//! `--cfg engine_gate_holder_debug` also counts holders separately, to assert that none is
//! outstanding when a gate is moved by [`Gate::assign_from`] or dropped.

use std::cell::{Cell, UnsafeCell};
use std::fmt;
use std::marker::PhantomPinned;
use std::pin::Pin;

use crate::engine::core::future::{
    ExceptionPtr, Future, Futurize, FuturizeT, Promise, futurize_invoke, make_exception_ptr,
};
use crate::engine::core::sstring::Sstring;

/// Error returned when entering a [`Gate`] that has been closed by [`Gate::close`].
#[derive(Debug, Clone, Copy, Default)]
pub struct GateClosedException;

impl GateClosedException {
    /// Returns the error message.
    pub fn what(&self) -> &'static str {
        "gate closed"
    }
}

impl fmt::Display for GateClosedException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.what())
    }
}

impl std::error::Error for GateClosedException {}

/// Error returned when entering a named [`Gate`] (see [`Gate::named`]) that has been closed;
/// its message includes the gate's name.
#[derive(Debug, Clone, Default)]
pub struct NamedGateClosedException {
    what: Sstring,
}

impl NamedGateClosedException {
    const DEFAULT_WHAT: &'static str = "named gate closed";

    /// Builds the error for the gate called `name`.
    pub fn new(name: &Sstring) -> Self {
        let mut what = Sstring::default();
        if !name.is_empty() {
            what = Sstring::from(format!("{} gate closed", name.as_str()).as_str());
        }
        Self { what }
    }

    /// Returns the error message.
    pub fn what(&self) -> &str {
        if self.what.is_empty() { Self::DEFAULT_WHAT } else { self.what.as_str() }
    }
}

impl fmt::Display for NamedGateClosedException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.what())
    }
}

impl std::error::Error for NamedGateClosedException {}

/// Facility to stop new requests, and to tell when existing requests are done.
///
/// When stopping a service that serves asynchronous requests, we are faced with
/// two problems: preventing new requests from coming in, and knowing when existing
/// requests have completed. A gate provides a solution.
///
/// A named gate is a gate with a name (see [`Gate::named`]); its closed error is
/// a [`NamedGateClosedException`].
///
/// Holders point at their gate, so a gate gives holders only when pinned
/// ([`Gate::hold`] takes `Pin<&Self>`), and its state is interior (several holders and
/// the code that owns the gate use it at once). Destroying a gate with requests still
/// registered aborts the process, because the holders would dangle.
#[derive(Default)]
pub struct Gate {
    count: Cell<usize>,
    stopped: UnsafeCell<Option<Promise<()>>>,
    /// The name of a named gate (`None` for a plain gate).
    name: Option<Sstring>,
    /// The number of live holders (debug builds with `engine_gate_holder_debug`).
    #[cfg(engine_gate_holder_debug)]
    holders: Cell<usize>,
    _pin: PhantomPinned,
}

impl Gate {
    /// Creates an open gate with no requests.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an open gate called `name`; entering it once closed fails with
    /// [`NamedGateClosedException`].
    #[inline]
    pub fn named(name: Sstring) -> Self {
        Self {
            count: Cell::new(0),
            stopped: UnsafeCell::new(None),
            name: Some(name),
            #[cfg(engine_gate_holder_debug)]
            holders: Cell::new(0),
            _pin: PhantomPinned,
        }
    }

    #[cfg(engine_gate_holder_debug)]
    fn assert_not_held_when_moved(&self) {
        assert!(self.holders.get() == 0, "gate moved with outstanding holders");
    }
    #[cfg(engine_gate_holder_debug)]
    fn assert_not_held_when_destroyed(&self) {
        assert!(self.holders.get() == 0, "gate destroyed with outstanding holders");
    }
    #[cfg(not(engine_gate_holder_debug))]
    #[inline]
    fn assert_not_held_when_moved(&self) {}
    #[cfg(not(engine_gate_holder_debug))]
    #[inline]
    fn assert_not_held_when_destroyed(&self) {}

    /// Whether the gate is closed (the close promise exists).
    #[inline(always)]
    fn stopped(&self) -> bool {
        // SAFETY: the option is only borrowed for statements that do not re-enter.
        unsafe { (*self.stopped.get()).is_some() }
    }

    /// Moves the state of `x` into this gate, leaving `x` open and empty.
    ///
    /// `x` must have no holders; with `engine_gate_holder_debug` that is asserted.
    ///
    /// # Panics
    /// Panics if this gate still has requests registered.
    pub fn assign_from(&mut self, x: &mut Gate) {
        if !std::ptr::eq(self, x) {
            assert!(self.count.get() == 0, "gate reassigned with outstanding requests");
            x.assert_not_held_when_moved();
            self.count.set(x.count.replace(0));
            *self.stopped.get_mut() = x.stopped.get_mut().take();
        }
    }

    /// Tries to register an in-progress request.
    ///
    /// If the gate is not closed, the request is registered and the function returns `true`,
    /// Otherwise the function just returns `false` and has no other effect.
    #[inline]
    pub fn try_enter(&self) -> bool {
        let opened = !self.stopped();
        if opened {
            self.count.set(self.count.get() + 1);
        }
        opened
    }

    /// Registers an in-progress request.
    ///
    /// If the gate is not closed, the request is registered.
    ///
    /// # Errors
    /// Returns the gate's closed error ([`Gate::make_closed_exception`]) if it is closed.
    #[inline]
    pub fn enter(&self) -> Result<(), ExceptionPtr> {
        if !self.try_enter() {
            return self.throw_closed_exception();
        }
        Ok(())
    }

    /// Unregisters an in-progress request.
    ///
    /// If the gate is closed, and there are no more in-progress requests,
    /// the future returned by [`Gate::close`] is resolved.
    #[inline]
    pub fn leave(&self) {
        let c = self.count.get() - 1;
        self.count.set(c);
        if c == 0 {
            // SAFETY: set_value only schedules the waiting task; it does not re-enter the
            // gate, so the reference ends before anything else uses the option.
            if let Some(s) = unsafe { (*self.stopped.get()).as_mut() } {
                s.set_value(());
            }
        }
    }

    /// Potentially stop an in-progress request.
    ///
    /// By using [`Gate::enter`] and [`Gate::leave`], the program can ensure that
    /// no further requests are serviced. However, long-running requests may
    /// continue to run. This method allows such a long operation to
    /// voluntarily stop itself after the gate is closed, by calling it
    /// in appropriate places and bailing out on error.
    ///
    /// # Errors
    /// Returns the gate's closed error if the gate is closed.
    #[inline]
    pub fn check(&self) -> Result<(), ExceptionPtr> {
        if self.stopped() {
            return self.throw_closed_exception();
        }
        Ok(())
    }

    /// Closes the gate.
    ///
    /// Future calls to [`Gate::enter`] will fail, and when all current requests call
    /// [`Gate::leave`], the returned future will be made ready.
    ///
    /// # Panics
    /// Panics if the gate is already closed.
    pub fn close(&self) -> Future<()> {
        assert!(!self.stopped(), "teseladb::gate::close() cannot be called more than once");
        let count = self.count.get();
        // SAFETY: nothing below re-enters the gate while the reference is live.
        let s = unsafe { (*self.stopped.get()).insert(Promise::new()) };
        if count == 0 {
            s.set_value(());
        }
        s.get_future()
    }

    /// Returns a current number of registered in-progress requests.
    #[inline]
    pub fn get_count(&self) -> usize {
        self.count.get()
    }

    /// Returns whether the gate is closed.
    #[inline]
    pub fn is_closed(&self) -> bool {
        self.stopped()
    }

    /// Returns a RAII [`Holder`] that enters the gate now and leaves it when dropped.
    ///
    /// # Errors
    /// Returns the gate's closed error if the gate is closed.
    #[inline]
    pub fn hold(self: Pin<&Self>) -> Result<Holder, ExceptionPtr> {
        Holder::new(self)
    }

    /// Tries to get a RAII [`Holder`] that enters the gate now and leaves it when dropped.
    /// Returns `None` if and only if the gate is closed.
    #[inline]
    pub fn try_hold(self: Pin<&Self>) -> Option<Holder> {
        if self.is_closed() { None } else { Holder::new(self).ok() }
    }

    /// Returns the error reported when the closed gate is entered:
    /// [`NamedGateClosedException`] for a named gate, [`GateClosedException`] otherwise.
    pub fn make_closed_exception(&self) -> ExceptionPtr {
        match &self.name {
            None => make_exception_ptr(GateClosedException),
            Some(name) => make_exception_ptr(NamedGateClosedException::new(name)),
        }
    }

    /// Returns `Err` with the closed error.
    #[cold]
    fn throw_closed_exception<T>(&self) -> Result<T, ExceptionPtr> {
        Err(self.make_closed_exception())
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        // A gate dropped with outstanding requests is always fatal, because holders would be
        // left pointing at freed memory.
        if self.count.get() != 0 {
            eprintln!("gate destroyed with outstanding requests");
            std::process::abort();
        }
        self.assert_not_held_when_destroyed();
    }
}

/// A gate with a name: a [`Gate`] built with [`Gate::named`].
pub type NamedGate = Gate;

/// Facility to hold a gate opened using RAII.
///
/// A holder is usually obtained using [`Gate::hold`].
///
/// The gate is entered when the holder is constructed, and left when the holder is dropped.
///
/// Cloning the holder re-enters the gate to keep an extra reference on it.
/// Moving the holder is supported and has no effect on the gate itself.
///
/// The gate is pinned (it cannot move), and it cannot be destroyed while a holder
/// exists (that aborts), so the pointer stays valid.
pub struct Holder {
    g: *const Gate,
}

impl Default for Holder {
    /// Constructs a holder referencing no gate.
    fn default() -> Self {
        Self::with_ptr(std::ptr::null())
    }
}

impl Holder {
    #[inline]
    fn with_ptr(g: *const Gate) -> Self {
        Self { g }
    }

    /// Counts this holder in its gate's debug holder count.
    #[cfg(engine_gate_holder_debug)]
    fn debug_hold_gate(&mut self) {
        if !self.g.is_null() {
            // SAFETY: the gate outlives its holders.
            unsafe { (*self.g).holders.set((*self.g).holders.get() + 1) };
        }
    }
    /// Removes this holder from the debug holder count of `g`, the gate it held.
    #[cfg(engine_gate_holder_debug)]
    fn debug_release_gate(&mut self, g: *const Gate) {
        // SAFETY: the gate outlives its holders.
        unsafe { (*g).holders.set((*g).holders.get() - 1) };
    }
    #[cfg(not(engine_gate_holder_debug))]
    #[inline]
    fn debug_hold_gate(&mut self) {}
    #[cfg(not(engine_gate_holder_debug))]
    #[inline]
    fn debug_release_gate(&mut self, _g: *const Gate) {}

    // release the holder from the gate without leaving it.
    // used for release and move.
    #[inline]
    fn release_gate(&mut self) -> *const Gate {
        let g = std::mem::replace(&mut self.g, std::ptr::null());
        if !g.is_null() {
            self.debug_release_gate(g);
        }
        g
    }

    /// Constructs a holder by entering the gate `g`.
    ///
    /// # Errors
    /// Returns the gate's closed error if the gate is already closed.
    pub fn new(g: Pin<&Gate>) -> Result<Self, ExceptionPtr> {
        g.enter()?;
        let mut h = Self::with_ptr(g.get_ref());
        h.debug_hold_gate();
        Ok(h)
    }

    /// Leaves the held gate, if any; the holder then references no gate.
    #[inline]
    pub fn release(&mut self) {
        let g = self.release_gate();
        if !g.is_null() {
            // SAFETY: the gate outlives its holders.
            unsafe { (*g).leave() };
        }
    }
}

impl Clone for Holder {
    /// Constructs a holder by copying another holder.
    /// Copying a holder never fails: the copied holder has already entered the gate,
    /// so even if the gate was closed since, the copy is also allowed to enter.
    /// Note that the fiber waiting for `close()`, which until now was waiting for the one holder to leave,
    /// will now wait for both copies to leave.
    fn clone(&self) -> Self {
        let mut h = Self::with_ptr(self.g);
        if !h.g.is_null() {
            // SAFETY: the gate outlives its holders.
            unsafe { (*h.g).count.set((*h.g).count.get() + 1) };
            h.debug_hold_gate();
        }
        h
    }

    /// Copy-assigns another holder, leaving the current gate first if the two differ.
    fn clone_from(&mut self, x: &Self) {
        if x.g != self.g {
            self.release();
            self.g = x.g;
            if !self.g.is_null() {
                // SAFETY: the gate outlives its holders.
                unsafe { (*self.g).count.set((*self.g).count.get() + 1) };
                self.debug_hold_gate();
            }
        }
    }
}

impl Drop for Holder {
    /// Leaves the referenced gate, if any.
    #[inline]
    fn drop(&mut self) {
        self.release();
    }
}

/// Helpers of [`with_gate`] and [`try_with_gate`].
pub mod internal {
    use super::*;

    /// Runs `func` and keeps the holder `gh` alive until its future resolves.
    #[inline]
    pub fn invoke_func_with_gate<Func, R>(gh: Holder, func: Func) -> FuturizeT<R>
    where
        Func: FnOnce() -> R + 'static,
        R: Futurize,
    {
        futurize_invoke(func).finally(move || drop(gh))
    }
}
/// Executes the function `func` making sure the gate `g` is properly entered
/// and later on, properly left.
///
/// Returns whatever `func` returns, as a future; the gate is left when that future resolves.
///
/// # Errors
/// Returns the gate's closed error, without running `func`, if the gate is closed.
pub fn with_gate<Func, R>(g: Pin<&Gate>, func: Func) -> Result<FuturizeT<R>, ExceptionPtr>
where
    Func: FnOnce() -> R + 'static,
    R: Futurize,
{
    Ok(internal::invoke_func_with_gate(g.hold()?, func))
}

/// Executes the function `func` if the gate `g` can be entered
/// and later on, properly left.
///
/// If the gate is already closed, a failed future holding the gate's closed error is
/// returned; otherwise whatever `func` returns, as a future.
pub fn try_with_gate<Func, R>(g: Pin<&Gate>, func: Func) -> FuturizeT<R>
where
    Func: FnOnce() -> R + 'static,
    R: Futurize,
{
    if g.is_closed() {
        return R::make_exception_future(g.make_closed_exception());
    }
    match g.hold() {
        Ok(h) => internal::invoke_func_with_gate(h, func),
        Err(e) => R::make_exception_future(e),
    }
}

/// A gate can be closed asynchronously, so it can be used with `deferred_close` and
/// `with_closeable`.
impl crate::engine::util::closeable::Closeable for Gate {
    #[inline]
    fn close(&mut self) -> Future<()> {
        Gate::close(self)
    }
}

/// A gate is closed through a shared reference (its state is interior), so a shared
/// reference to one is closeable too: `deferred_close` can hold it while the gate is still
/// entered through other references.
impl crate::engine::util::closeable::Closeable for &Gate {
    #[inline]
    fn close(&mut self) -> Future<()> {
        Gate::close(self)
    }
}
