//! Lazily evaluated values for logging.
//!
//! Log arguments are formatted only if the message's level is enabled. Wrapping an
//! expensive computation in [`value_of`], or an optional value in [`lazy_deref`], defers
//! the work (and the null check) to the moment the argument is actually formatted.

use std::fmt;
use std::ops::Deref;

/// A value computed on demand by a closure that takes no arguments.
///
/// Created with [`value_of`]. Formatting it with `{}` calls the closure and displays the
/// result, so a disabled log statement never evaluates it.
pub struct LazyEval<Func> {
    _func: Func,
}

impl<R, Func: Fn() -> R> LazyEval<Func> {
    fn new(f: Func) -> Self {
        LazyEval { _func: f }
    }

    /// Evaluates the closure and returns its value. Each call evaluates it again.
    pub fn call(&self) -> R {
        (self._func)()
    }
}

/// Wraps `func` in a [`LazyEval`], to be evaluated only when the result is formatted or
/// [`LazyEval::call`] is invoked.
///
/// For example, a debug-level log call whose argument is `value_of(|| heavy())` does not
/// call `heavy` when the debug level is disabled.
#[inline]
pub fn value_of<R, Func: Fn() -> R>(func: Func) -> LazyEval<Func> {
    LazyEval::new(func)
}

/// A borrowed, possibly empty pointer that is dereferenced only when formatted.
///
/// Useful to log the target of an optional pointer without paying for the emptiness check
/// on the call site: the check happens inside `Display`, which prints `null` when empty.
pub struct LazyDerefWrapper<'a, T> {
    /// The pointer to dereference when formatting.
    pub p: &'a Option<T>,
}

impl<'a, T> LazyDerefWrapper<'a, T> {
    /// Wraps a reference to `p`; nothing is copied.
    pub const fn new(p: &'a Option<T>) -> Self {
        LazyDerefWrapper { p }
    }
}

/// Wraps `p` in a [`LazyDerefWrapper`].
///
/// The dereference happens when the wrapper is formatted. Only a reference to `p` is
/// kept. `T` may be any `Deref` pointer (`Box`, `Rc`, `&U`, ...).
pub fn lazy_deref<T>(p: &Option<T>) -> LazyDerefWrapper<'_, T> {
    LazyDerefWrapper::new(p)
}

/// Displays the value returned by the closure, evaluating it at format time.
impl<R: fmt::Display, Func: Fn() -> R> fmt::Display for LazyEval<Func> {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(os, "{}", self.call())
    }
}

/// Displays the pointed-to value, or `null` when the pointer is empty.
impl<T: Deref> fmt::Display for LazyDerefWrapper<'_, T>
where
    T::Target: fmt::Display,
{
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(p) = self.p {
            return write!(os, "{}", &**p);
        }
        os.write_str("null")
    }
}
