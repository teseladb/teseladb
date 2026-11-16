//! Explicit by-reference arguments.
//!
//! [`ref_`] and [`cref`] wrap a reference so that a call site states explicitly that an
//! object is passed by reference, which matters when execution is deferred to a
//! continuation: the borrow checker then proves that the object outlives the wrapper.

/// A mutable reference wrapped so that passing it by reference is explicit.
///
/// It can only be built with [`ref_`], never implicitly from a reference. The wrapper
/// borrows the object for `'a`, so the object outlives every use of the wrapper. It is not
/// `Copy` (two copies would be two `&mut` to the same object); see
/// [`ConstReferenceWrapper`] for the shared, copyable variant.
pub struct ReferenceWrapper<'a, T: ?Sized> {
    _pointer: &'a mut T,
}

impl<'a, T: ?Sized> ReferenceWrapper<'a, T> {
    #[inline]
    fn new(object: &'a mut T) -> Self {
        ReferenceWrapper { _pointer: object }
    }

    /// Returns a shared reference to the wrapped object.
    #[inline]
    pub fn get(&self) -> &T {
        self._pointer
    }

    /// Returns a mutable reference to the wrapped object.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        self._pointer
    }

    /// Consumes the wrapper and returns the reference for its whole borrow `'a`.
    #[inline]
    pub fn into_inner(self) -> &'a mut T {
        self._pointer
    }
}

/// Wraps a mutable reference in a [`ReferenceWrapper`].
#[inline]
pub fn ref_<T: ?Sized>(object: &mut T) -> ReferenceWrapper<'_, T> {
    ReferenceWrapper::new(object)
}

/// Wraps a shared reference in a [`ConstReferenceWrapper`].
#[inline]
pub fn cref<T: ?Sized>(object: &T) -> ConstReferenceWrapper<'_, T> {
    ConstReferenceWrapper { _pointer: object }
}

/// A shared reference wrapped so that passing it by reference is explicit. `Copy`.
pub struct ConstReferenceWrapper<'a, T: ?Sized> {
    _pointer: &'a T,
}

impl<T: ?Sized> Clone for ConstReferenceWrapper<'_, T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: ?Sized> Copy for ConstReferenceWrapper<'_, T> {}

impl<'a, T: ?Sized> ConstReferenceWrapper<'a, T> {
    /// Returns the wrapped reference.
    #[inline]
    pub fn get(&self) -> &'a T {
        self._pointer
    }
}
