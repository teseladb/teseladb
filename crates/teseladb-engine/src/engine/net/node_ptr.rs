//! Pointer helpers for the network stack's long-lived, self-referenced objects.
//!
//! [`NodePtr`] is an owning heap pointer that other code may point into: the stack keeps
//! connections, listeners and devices in maps or fields while closures, timers and C
//! callbacks hold a raw pointer to them. `Box<T>` cannot play that role: it asserts
//! uniqueness (`noalias`) when it is moved or passed by value, which invalidates the raw
//! pointers handed out to its contents. `NodePtr` is a plain `NonNull<T>` with ownership:
//! moving it moves only the address, and the raw pointers from [`NodePtr::as_ptr`] stay
//! valid until the `NodePtr` is dropped.
//!
//! [`CrossShard`] carries a non-`Send` value into a closure that runs on another shard, and
//! `StaticConst` is a lazily built value shared read-only by all shards. Both move the
//! soundness argument to an `unsafe` constructor whose caller states it.

use std::marker::PhantomData;
use std::ptr::NonNull;

/// An owning, aliasable pointer to a heap `T` (one allocation, like `Box`).
pub struct NodePtr<T: ?Sized> {
    ptr: NonNull<T>,
    _owns: PhantomData<T>,
}

impl<T> NodePtr<T> {
    /// Moves `v` to the heap.
    #[inline]
    pub fn new(v: T) -> Self {
        NodePtr { ptr: NonNull::from(Box::leak(Box::new(v))), _owns: PhantomData }
    }
}

impl<T: ?Sized> NodePtr<T> {
    /// Takes ownership of a heap value made with `Box::into_raw` (or of a `Box<MaybeUninit<T>>`
    /// initialized in place), so that it can be built where it will live.
    ///
    /// # Safety
    /// `p` comes from `Box::into_raw` of a `Box<T>` or `Box<MaybeUninit<T>>`, points to an
    /// initialized `T`, and is owned by nobody else.
    #[inline]
    pub unsafe fn from_raw(p: *mut T) -> Self {
        // SAFETY: `Box::into_raw` never returns null.
        NodePtr { ptr: unsafe { NonNull::new_unchecked(p) }, _owns: PhantomData }
    }

    /// Gives up ownership: the value is freed by whoever takes the pointer back with
    /// [`NodePtr::from_raw`]. Used to change the pointee type (`NodePtr<Q>` to
    /// `NodePtr<dyn Qp>`) without going through a `Box`.
    #[inline]
    pub fn into_raw(self) -> *mut T {
        let p = self.ptr.as_ptr();
        std::mem::forget(self);
        p
    }

    /// The stable address of the value; valid until `self` is dropped.
    #[inline]
    pub fn as_ptr(&self) -> *mut T {
        self.ptr.as_ptr()
    }

    /// A shared reference to the value.
    ///
    /// # Safety
    /// No `&mut T` made from [`NodePtr::as_ptr`] is live while the result is used.
    #[inline]
    pub unsafe fn get(&self) -> &T {
        // SAFETY: the value is live while `self` is; aliasing is the caller's contract.
        unsafe { &*self.ptr.as_ptr() }
    }

    /// An exclusive reference to the value.
    ///
    /// # Safety
    /// No other reference to the value (from `get`, `get_mut` or [`NodePtr::as_ptr`]) is live
    /// while the result is used.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_mut(&self) -> &mut T {
        // SAFETY: the value is live while `self` is; exclusivity is the caller's contract.
        unsafe { &mut *self.ptr.as_ptr() }
    }
}

impl<T: ?Sized> Drop for NodePtr<T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: `ptr` came from `Box::leak` in `new` and is freed only here.
        drop(unsafe { Box::from_raw(self.ptr.as_ptr()) });
    }
}

/// A value that a closure of `Smp::submit_to` or `Smp::invoke_on_all` carries to another
/// shard: a raw pointer, a reference or a moved object that the closure needs there.
///
/// Those APIs require `Send`; raw pointers, packets and deleters are not, because using them
/// on another shard is only sound under a contract the type cannot see (the pointee is
/// shared by design, or touched by one shard at a time, or only destroyed on its home
/// shard). [`CrossShard::new`] is where each caller states that contract.
///
/// Take the value out with [`CrossShard::into_inner`] or [`CrossShard::get`] inside the
/// closure, never by field: edition 2021 closures capture only the fields they name, so a
/// field access would capture the inner value, which is not `Send`.
pub struct CrossShard<T>(T);

// SAFETY: every value is wrapped through `CrossShard::new`, whose caller guarantees that
// using and dropping it on the target shard is sound; `Send` only lets the closure travel.
unsafe impl<T> Send for CrossShard<T> {}

impl<T: Clone> Clone for CrossShard<T> {
    #[inline]
    fn clone(&self) -> Self {
        CrossShard(self.0.clone())
    }
}

impl<T: Copy> Copy for CrossShard<T> {}

impl<T> CrossShard<T> {
    /// Wraps `v` for another shard.
    ///
    /// # Safety
    /// Using `v` on the shard the closure runs on, and dropping it wherever the closure is
    /// dropped, is sound: what `v` points to outlives that use and is not accessed by another
    /// shard at the same time.
    #[inline]
    pub unsafe fn new(v: T) -> Self {
        CrossShard(v)
    }

    /// The value.
    #[inline]
    pub fn into_inner(self) -> T {
        self.0
    }

    /// A reference to the value.
    #[inline]
    pub fn get(&self) -> &T {
        &self.0
    }
}

/// A process-wide constant built on first use: the first shard that gets there builds it,
/// then all shards share it and only read it.
pub(crate) struct StaticConst<T>(std::sync::OnceLock<T>);

impl<T> StaticConst<T> {
    /// An empty static.
    ///
    /// # Safety
    /// `T` must have no interior mutability, and reading a `T` from any thread must be sound:
    /// its raw pointers, if any, own heap blocks that are only read once the value is built.
    pub(crate) const unsafe fn new() -> Self {
        StaticConst(std::sync::OnceLock::new())
    }

    /// The value, built by `f` on first use.
    #[inline]
    pub(crate) fn get_or_init(&self, f: impl FnOnce() -> T) -> &T {
        self.0.get_or_init(f)
    }

    /// The value, if built.
    #[inline]
    pub(crate) fn get(&self) -> Option<&T> {
        self.0.get()
    }

    /// Stores `v` unless a value is there already (which is kept; `v` is returned).
    #[inline]
    pub(crate) fn set(&self, v: T) -> Result<(), T> {
        self.0.set(v)
    }
}

// SAFETY: guaranteed by the contract of `new`: the value is written once under the `OnceLock`
// and only read afterwards, which is sound from any thread for such a `T`.
unsafe impl<T> Sync for StaticConst<T> {}
