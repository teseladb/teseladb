//! A pointer wrapper that checks for null on every checked access.
//!
//! [`CheckedPtr`] wraps a raw pointer, an `Option` of a smart pointer or a
//! [`WeakPtr`](crate::engine::core::weak_ptr::WeakPtr). Its checked accessors run a
//! [`NullDerefAction`] when the pointer is not engaged; the default action,
//! [`DefaultNullDerefAction`], returns a [`CheckedPtrIsNullException`] as the error. Equality
//! and hashing compare the pointed-to address and never check.

use crate::engine::core::future::ExceptionPtr;

/// The error [`DefaultNullDerefAction`] returns when a [`CheckedPtr`] that is not engaged is
/// dereferenced.
#[derive(Debug, Clone, Copy, Default)]
pub struct CheckedPtrIsNullException;

impl std::fmt::Display for CheckedPtrIsNullException {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("dereference of a null checked pointer")
    }
}

impl std::error::Error for CheckedPtrIsNullException {}

/// The action a `CheckedPtr` runs when dereferenced while not engaged.
pub trait NullDerefAction: Default {
    /// Runs the action. An `Err` aborts the access and is returned by the checked accessor;
    /// `Ok` lets the access proceed with the null pointer.
    fn call(&self) -> Result<(), ExceptionPtr>;
}

/// The default action on dereferencing a [`CheckedPtr`] that is not engaged: fails with a
/// [`CheckedPtrIsNullException`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultNullDerefAction;

impl NullDerefAction for DefaultNullDerefAction {
    /// Always returns a [`CheckedPtrIsNullException`].
    #[inline]
    fn call(&self) -> Result<(), ExceptionPtr> {
        Err(ExceptionPtr::new(CheckedPtrIsNullException))
    }
}

/// The pointer types [`CheckedPtr`] wraps: a raw pointer, a weak pointer, or an `Option` of a
/// smart pointer (`None` is the null state).
pub trait CheckedPtrTarget {
    /// The type pointed to.
    type Element: ?Sized;
    /// Whether the pointer is engaged (not null, or the weak pointer's object is alive).
    fn is_engaged(&self) -> bool;
    /// The address of the pointed-to object, or null when not engaged.
    fn do_get(&self) -> *const Self::Element;
}

impl<T: ?Sized> CheckedPtrTarget for *mut T {
    type Element = T;
    #[inline]
    fn is_engaged(&self) -> bool {
        !self.is_null()
    }
    #[inline]
    fn do_get(&self) -> *const T {
        *self
    }
}

impl<T: ?Sized> CheckedPtrTarget for *const T {
    type Element = T;
    #[inline]
    fn is_engaged(&self) -> bool {
        !self.is_null()
    }
    #[inline]
    fn do_get(&self) -> *const T {
        *self
    }
}

/// A weak pointer is engaged while its object is alive.
impl<T> CheckedPtrTarget for crate::engine::core::weak_ptr::WeakPtr<T> {
    type Element = T;
    #[inline]
    fn is_engaged(&self) -> bool {
        self.is_some()
    }
    #[inline]
    fn do_get(&self) -> *const T {
        self.get()
    }
}

impl<P: std::ops::Deref> CheckedPtrTarget for Option<P>
where
    P::Target: Sized,
{
    type Element = P::Target;
    #[inline]
    fn is_engaged(&self) -> bool {
        self.is_some()
    }
    #[inline]
    fn do_get(&self) -> *const P::Target {
        match self {
            Some(p) => &**p,
            None => std::ptr::null(),
        }
    }
}
/// A wrapper around any pointer type (a smart pointer or a raw pointer) that runs the action
/// `A` when it is dereferenced while not engaged.
///
/// It may still be assigned, compared and moved without restriction; only the checked
/// accessors ([`get`](Self::get), [`ptr`](Self::ptr), [`ptr_mut`](Self::ptr_mut)) run the
/// check. With the default action they fail with a [`CheckedPtrIsNullException`].
#[derive(Debug, Clone, Default)]
pub struct CheckedPtr<P: CheckedPtrTarget, A: NullDerefAction = DefaultNullDerefAction> {
    _ptr: P,
    _action: std::marker::PhantomData<A>,
}

impl<P: CheckedPtrTarget, A: NullDerefAction> CheckedPtr<P, A> {
    /// Runs the action `A` if the underlying pointer is not engaged.
    #[inline]
    fn check(&self) -> Result<(), ExceptionPtr> {
        if !self._ptr.is_engaged() {
            return A::default().call();
        }
        Ok(())
    }

    /// Wraps `ptr`.
    #[inline]
    pub fn new(ptr: P) -> Self {
        Self { _ptr: ptr, _action: std::marker::PhantomData }
    }

    // Checked methods: they run the action `A` first if the underlying pointer is not engaged.

    /// Returns the address of the pointed-to object.
    ///
    /// # Errors
    ///
    /// Returns the action's error when the pointer is not engaged.
    #[inline]
    pub fn get(&self) -> Result<*const P::Element, ExceptionPtr> {
        self.check()?;
        Ok(self._ptr.do_get())
    }

    /// Returns a reference to the wrapped pointer object.
    ///
    /// # Errors
    ///
    /// Returns the action's error when the pointer is not engaged.
    #[inline]
    pub fn ptr(&self) -> Result<&P, ExceptionPtr> {
        self.check()?;
        Ok(&self._ptr)
    }

    /// Returns a mutable reference to the wrapped pointer object.
    ///
    /// # Errors
    ///
    /// Returns the action's error when the pointer is not engaged.
    #[inline]
    pub fn ptr_mut(&mut self) -> Result<&mut P, ExceptionPtr> {
        self.check()?;
        Ok(&mut self._ptr)
    }

    /// The wrapped pointer object of a pinned `CheckedPtr`, without the check, for assigning a
    /// pointer object that must not move once set (a linked weak pointer, filled in place by
    /// `weak_from_this`). The pointer object is structurally pinned: a `CheckedPtr` never moves
    /// it out.
    #[inline]
    pub fn ptr_pin(self: std::pin::Pin<&mut Self>) -> std::pin::Pin<&mut P> {
        // SAFETY: structural pinning of `_ptr`: no method moves it out of a pinned
        // `CheckedPtr` (the `&mut` accessors need an unpinned one when `P: !Unpin`), there is
        // no `Drop` impl, and the struct is not packed.
        unsafe { self.map_unchecked_mut(|s| &mut s._ptr) }
    }

    // Unchecked methods: they may be called when the underlying pointer is not engaged.

    /// Whether the underlying pointer is engaged.
    #[inline]
    pub fn is_engaged(&self) -> bool {
        self._ptr.is_engaged()
    }

    /// Hash of the pointed-to address (with the standard library's default hasher).
    pub fn hash_value(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::hash::DefaultHasher::new();
        (self._ptr.do_get() as *const () as usize).hash(&mut h);
        h.finish()
    }
}

impl<P: CheckedPtrTarget, A: NullDerefAction> PartialEq for CheckedPtr<P, A> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self._ptr.do_get() as *const (), other._ptr.do_get() as *const ())
    }
}

/// Hashes the pointed-to address through [`CheckedPtr::hash_value`].
impl<P: CheckedPtrTarget, A: NullDerefAction> std::hash::Hash for CheckedPtr<P, A> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash_value());
    }
}
