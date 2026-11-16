//! Non-atomic reference-counted pointers for per-shard data.
//!
//! Two facilities are provided, both modelled on the standard shared-pointer design but **not
//! thread-safe**: two pointers sharing an object must stay on the same shard (OS thread), which
//! lets the count be a plain integer instead of an atomic.
//!
//! * [`LwSharedPtr`] is the lightweight variant: one machine word per pointer and one word of
//!   overhead in the shared object (the count, placed right before it). It does not support
//!   polymorphism.
//! * [`SharedPtr`] costs two words per pointer and two words in the shared object (the count
//!   and a type-erased destructor). In return it supports polymorphism: `T` may be a
//!   `dyn Trait`, and the cast functions convert between views of the same object.
//!
//! Both support getting a new pointer from the object itself ([`EnableLwSharedFromThis`],
//! [`SharedFromThis`]). The object is reachable through `&T` from any copy; mutation goes
//! through [`LwSharedPtr::get_mut_unchecked`] / [`SharedPtr::get_mut_unchecked`], whose
//! contract is that no other reference to the object is in use. With
//! `--cfg engine_debug_shared_ptr` the count also checks that it is only touched on its shard.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::mem::offset_of;
use std::ops::Deref;
use std::ptr::{self, NonNull};

/// The reference count of a shared pointer: a plain (non-atomic) `i64`.
#[cfg(not(engine_debug_shared_ptr))]
#[derive(Debug, Default)]
#[repr(transparent)]
pub struct SharedPtrCounterType(std::cell::Cell<i64>);

#[cfg(not(engine_debug_shared_ptr))]
impl SharedPtrCounterType {
    /// A counter starting at `x`.
    #[inline]
    pub const fn new(x: i64) -> Self {
        Self(std::cell::Cell::new(x))
    }
    /// The current count.
    #[inline]
    pub fn get(&self) -> i64 {
        self.0.get()
    }
    /// Increments the count.
    #[inline]
    pub fn inc(&self) {
        self.0.set(self.0.get() + 1);
    }
    /// Decrements the count and returns the new value.
    #[inline]
    pub fn dec(&self) -> i64 {
        let v = self.0.get() - 1;
        self.0.set(v);
        v
    }
}

/// The reference count in `engine_debug_shared_ptr` builds: checks that it is used on the
/// thread that created it.
#[cfg(engine_debug_shared_ptr)]
pub type SharedPtrCounterType =
    crate::engine::core::shared_ptr_debug_helper::DebugSharedPtrCounterType;

/// The header of an [`LwSharedPtr`] allocation: the reference count, starting at zero.
#[repr(C)]
#[derive(Debug)]
pub struct LwSharedPtrCounterBase {
    pub(crate) count: SharedPtrCounterType,
}

impl Default for LwSharedPtrCounterBase {
    #[inline]
    fn default() -> Self {
        Self { count: SharedPtrCounterType::new(0) }
    }
}

// An `LwSharedPtr<T>` may point to any type `T`, and a `T` may also want to produce a pointer
// to itself (`EnableLwSharedFromThis`). Both are served by one layout: the object always sits
// right after its counter in an `LwSharedPtrNoEsft<T>`, and `shared_from_this()` steps back from
// the object to the counter by the field offset. That keeps a one-word pointer and a one-word
// overhead in both cases.

/// Types that may produce an `LwSharedPtr` to themselves from a pointer to the object.
///
/// The counter sits before the object, so the object is taken as a raw pointer with the
/// provenance of the whole allocation (from [`LwSharedPtr::get`], or one derived from
/// it), never as a reference, whose provenance would stop at the object.
pub trait EnableLwSharedFromThis: Sized {
    /// A new owning pointer to the object at `this` (the count is incremented).
    ///
    /// # Safety
    ///
    /// `this` must point to an object created by [`make_lw_shared`], and be derived from
    /// a pointer of one of its `LwSharedPtr`s.
    #[inline]
    unsafe fn shared_from_this(this: *const Self) -> LwSharedPtr<Self> {
        // SAFETY: guaranteed by the caller.
        unsafe { LwSharedPtr::from_counter(LwSharedPtrNoEsft::from_value(this)) }
    }
    /// The number of owning pointers to the object at `this`.
    ///
    /// # Safety
    ///
    /// As for `shared_from_this`.
    #[inline]
    unsafe fn use_count(this: *const Self) -> i64 {
        // SAFETY: guaranteed by the caller.
        unsafe { (*LwSharedPtrNoEsft::from_value(this)).counter.count.get() }
    }
}

impl<T: EnableLwSharedFromThis> LwSharedPtr<T> {
    /// [`EnableLwSharedFromThis::shared_from_this`] for an object reached through a raw
    /// pointer.
    ///
    /// # Safety
    ///
    /// As [`EnableLwSharedFromThis::shared_from_this`].
    #[inline]
    pub unsafe fn shared_from_this(this: *const T) -> Self {
        // SAFETY: forwarded from the caller.
        unsafe { T::shared_from_this(this) }
    }
}

/// The allocation behind an [`LwSharedPtr`]: the counter followed by the object.
#[repr(C)]
pub struct LwSharedPtrNoEsft<T> {
    counter: LwSharedPtrCounterBase,
    value: T,
}

impl<T> LwSharedPtrNoEsft<T> {
    /// The allocation that contains the object at `value`, found by subtracting the field
    /// offset (the pointer keeps `value`'s provenance).
    ///
    /// # Safety
    ///
    /// `value` must be the `value` field of a live `LwSharedPtrNoEsft<T>`.
    #[inline]
    unsafe fn from_value(value: *const T) -> *mut Self {
        // SAFETY: guaranteed by the caller.
        unsafe { (value as *const u8).sub(offset_of!(Self, value)) as *mut Self }
    }
}

/// A custom way to destroy the objects of [`LwSharedPtr`]s, used through
/// [`LwSharedPtr::dispose_with`].
pub trait LwSharedPtrDeleter<T> {
    /// Destroys the object at `p` and releases its storage.
    fn dispose(p: *mut T);
}

/// Conversions between an [`LwSharedPtr`] counter and its object.
pub mod internal {
    use super::*;

    /// Conversions between the counter of an `LwSharedPtrNoEsft<T>` and its object.
    pub struct LwSharedPtrAccessors<T>(PhantomData<T>);

    impl<T> LwSharedPtrAccessors<T> {
        /// The object that follows `counter`.
        ///
        /// # Safety
        ///
        /// `counter` must be the counter of a live `LwSharedPtrNoEsft<T>`.
        #[inline]
        pub unsafe fn to_value(counter: *mut LwSharedPtrCounterBase) -> *mut T {
            // SAFETY: the counter is the first field of a repr(C) LwSharedPtrNoEsft<T>.
            unsafe { &raw mut (*(counter as *mut LwSharedPtrNoEsft<T>)).value }
        }
        /// Drops the object and frees the allocation of `counter`.
        ///
        /// # Safety
        ///
        /// `counter` must be the counter of a live boxed `LwSharedPtrNoEsft<T>` with no
        /// owners left.
        #[inline]
        pub unsafe fn dispose(counter: *mut LwSharedPtrCounterBase) {
            // SAFETY: allocated with Box::new in LwSharedPtr::make.
            unsafe { drop(Box::from_raw(counter as *mut LwSharedPtrNoEsft<T>)) };
        }
        /// Drops the object at `value_ptr` and frees its allocation.
        ///
        /// # Safety
        ///
        /// `value_ptr` must point into a live boxed `LwSharedPtrNoEsft<T>` with no owners.
        #[inline]
        pub unsafe fn dispose_value(value_ptr: *mut T) {
            // SAFETY: guaranteed by the caller.
            unsafe { drop(Box::from_raw(LwSharedPtrNoEsft::from_value(value_ptr))) };
        }
    }
}

/// A one-word, non-atomic, reference-counted pointer; see the [module documentation](self).
///
/// It may be null. Cloning increments the count, dropping decrements it, and the last owner
/// drops the object and frees the allocation.
pub struct LwSharedPtr<T> {
    p: Option<NonNull<LwSharedPtrCounterBase>>,
    _marker: PhantomData<T>,
}

impl<T> LwSharedPtr<T> {
    /// A new owner of the allocation of counter `p` (the count is incremented), or a null
    /// pointer for a null `p`.
    ///
    /// # Safety
    ///
    /// `p` must be null or the counter of a live `LwSharedPtrNoEsft<T>`.
    #[inline]
    unsafe fn from_counter(p: *mut LwSharedPtrNoEsft<T>) -> Self {
        let p = NonNull::new(p as *mut LwSharedPtrCounterBase);
        if let Some(c) = p {
            // SAFETY: guaranteed by the caller.
            unsafe { c.as_ref().count.inc() };
        }
        Self { p, _marker: PhantomData }
    }

    /// Allocates `value` with its counter and returns the first owner.
    #[inline]
    fn make(value: T) -> Self {
        let p = Box::into_raw(Box::new(LwSharedPtrNoEsft {
            counter: LwSharedPtrCounterBase::default(),
            value,
        }));
        // SAFETY: p is a freshly allocated object.
        unsafe { Self::from_counter(p) }
    }

    /// Drops the object at `p` and frees its storage.
    ///
    /// # Safety
    ///
    /// `p` must come from [`LwSharedPtr::release`] and be disposed once.
    #[inline]
    pub unsafe fn dispose(p: *mut T) {
        // SAFETY: guaranteed by the caller.
        unsafe { internal::LwSharedPtrAccessors::<T>::dispose_value(p) };
    }

    /// Disposes of `p` through a custom [`LwSharedPtrDeleter`].
    #[inline]
    pub fn dispose_with<D: LwSharedPtrDeleter<T>>(p: *mut T) {
        D::dispose(p);
    }

    /// A null pointer.
    #[inline]
    pub const fn null() -> Self {
        Self { p: None, _marker: PhantomData }
    }

    /// The object's address, or null for a null pointer.
    #[inline]
    pub fn get(&self) -> *mut T {
        match self.p {
            // SAFETY: an engaged pointer refers to a live object.
            Some(c) => unsafe { internal::LwSharedPtrAccessors::<T>::to_value(c.as_ptr()) },
            None => ptr::null_mut(),
        }
    }

    /// Mutable access to the object.
    ///
    /// # Safety
    ///
    /// The pointer must be engaged and no other reference to the object may be in use
    /// while the returned one is.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_mut_unchecked(&self) -> &mut T {
        // SAFETY: guaranteed by the caller.
        unsafe { &mut *self.get() }
    }

    /// Releases this pointer's ownership without destroying the object, leaving `self` null.
    ///
    /// If this was the last owner, returns an [`LwSharedPtrUnique`] that is now the sole owner
    /// of the object; otherwise returns `None` (other owners remain). A raw pointer taken out
    /// of the unique owner with [`LwSharedPtrUnique::into_raw`] must still be destroyed with
    /// [`LwSharedPtr::dispose`].
    #[inline]
    pub fn release(&mut self) -> Option<LwSharedPtrUnique<T>> {
        let p = self.p.take()?;
        // SAFETY: the pointer was engaged, so the object is alive.
        unsafe {
            if p.as_ref().count.dec() != 0 {
                None
            } else {
                Some(LwSharedPtrUnique {
                    p: NonNull::new_unchecked(internal::LwSharedPtrAccessors::<T>::to_value(
                        p.as_ptr(),
                    )),
                })
            }
        }
    }

    /// The number of owners, 0 for a null pointer.
    #[inline]
    pub fn use_count(&self) -> i64 {
        // SAFETY: an engaged pointer refers to a live object.
        self.p.map_or(0, |c| unsafe { c.as_ref().count.get() })
    }

    /// Whether the pointer is not null.
    #[inline]
    pub fn is_some(&self) -> bool {
        self.p.is_some()
    }

    /// Whether this is the only owner of the object (false for a null pointer).
    #[inline]
    pub fn owned(&self) -> bool {
        // SAFETY: `is_some_and` calls the closure only for an engaged pointer, whose counter
        // is live while `self` owns it.
        self.p.is_some_and(|c| unsafe { c.as_ref().count.get() } == 1)
    }

    #[inline]
    fn raw(&self) -> *mut LwSharedPtrCounterBase {
        self.p.map_or(ptr::null_mut(), NonNull::as_ptr)
    }
}

/// The sole owner of an object released from an [`LwSharedPtr`]; dropping it destroys the
/// object with [`LwSharedPtr::dispose`].
pub struct LwSharedPtrUnique<T> {
    p: NonNull<T>,
}

impl<T> LwSharedPtrUnique<T> {
    /// Gives up ownership and returns the raw pointer, to be disposed with
    /// [`LwSharedPtr::dispose`].
    #[inline]
    pub fn into_raw(self) -> *mut T {
        let p = self.p.as_ptr();
        std::mem::forget(self);
        p
    }
}

impl<T> Deref for LwSharedPtrUnique<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: the unique owner keeps the object alive.
        unsafe { self.p.as_ref() }
    }
}

impl<T> std::ops::DerefMut for LwSharedPtrUnique<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: this is the only owner.
        unsafe { self.p.as_mut() }
    }
}

impl<T> Drop for LwSharedPtrUnique<T> {
    /// Destroys the object with [`LwSharedPtr::dispose`].
    #[inline]
    fn drop(&mut self) {
        // SAFETY: the object came from release() and has no other owner.
        unsafe { LwSharedPtr::<T>::dispose(self.p.as_ptr()) };
    }
}

impl<T> Default for LwSharedPtr<T> {
    #[inline]
    fn default() -> Self {
        Self::null()
    }
}

impl<T> Clone for LwSharedPtr<T> {
    #[inline]
    fn clone(&self) -> Self {
        if let Some(c) = self.p {
            // SAFETY: an engaged pointer refers to a live object.
            unsafe { c.as_ref().count.inc() };
        }
        Self { p: self.p, _marker: PhantomData }
    }
}

impl<T> Drop for LwSharedPtr<T> {
    #[inline(always)]
    fn drop(&mut self) {
        if let Some(c) = self.p {
            // SAFETY: an engaged pointer holds one reference on a live object.
            unsafe {
                if c.as_ref().count.dec() == 0 {
                    internal::LwSharedPtrAccessors::<T>::dispose(c.as_ptr());
                }
            }
        }
    }
}

/// Panics on dereferencing a null shared pointer. Kept out of line: the check is one
/// predictable branch, and the pointer's own `get()` and the `unsafe` accessors stay unchecked.
#[cold]
#[inline(never)]
#[track_caller]
fn null_deref() -> ! {
    panic!("dereferencing a null shared pointer");
}

impl<T> Deref for LwSharedPtr<T> {
    type Target = T;
    /// The object.
    ///
    /// # Panics
    ///
    /// Panics if the pointer is null.
    #[inline]
    fn deref(&self) -> &T {
        match self.p {
            // SAFETY: an engaged pointer refers to a live object, kept alive by `self`.
            Some(c) => unsafe { &*internal::LwSharedPtrAccessors::<T>::to_value(c.as_ptr()) },
            None => null_deref(),
        }
    }
}

impl<T> PartialEq for LwSharedPtr<T> {
    #[inline]
    fn eq(&self, x: &Self) -> bool {
        self.p == x.p
    }
}

impl<T> Eq for LwSharedPtr<T> {}

impl<T> PartialOrd for LwSharedPtr<T> {
    #[inline]
    fn partial_cmp(&self, x: &Self) -> Option<Ordering> {
        Some(self.cmp(x))
    }
}

impl<T> Ord for LwSharedPtr<T> {
    #[inline]
    fn cmp(&self, x: &Self) -> Ordering {
        self.raw().cmp(&x.raw())
    }
}

/// Allocates `value` with its counter and returns the first [`LwSharedPtr`] to it.
#[inline]
pub fn make_lw_shared<T>(a: T) -> LwSharedPtr<T> {
    LwSharedPtr::make(a)
}

impl<T: fmt::Display> fmt::Display for LwSharedPtr<T> {
    /// Prints the object, or `null` for a null pointer.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.p.is_none() {
            return f.write_str("null");
        }
        write!(f, "{}", **self)
    }
}

impl<T: fmt::Debug> fmt::Debug for LwSharedPtr<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.p.is_none() {
            return f.write_str("null");
        }
        write!(f, "{:?}", **self)
    }
}

/// The header of a [`SharedPtr`] allocation: the count plus the function that destroys the
/// fully-typed object, so a pointer to a trait object can free it.
#[repr(C)]
pub struct SharedPtrCountBase {
    // Drops the fully-typed object and frees its allocation.
    destroy: unsafe fn(*mut SharedPtrCountBase),
    pub(crate) count: SharedPtrCounterType,
}

impl SharedPtrCountBase {
    /// A counter at zero whose object is freed by `destroy`.
    #[inline]
    pub fn new(destroy: unsafe fn(*mut SharedPtrCountBase)) -> Self {
        Self { destroy, count: SharedPtrCounterType::new(0) }
    }
}

/// The allocation [`make_shared`] creates: the counter header followed by the object.
#[repr(C)]
pub struct SharedPtrCountFor<T> {
    base: SharedPtrCountBase,
    data: T,
}

impl<T> SharedPtrCountFor<T> {
    unsafe fn destroy(b: *mut SharedPtrCountBase) {
        // SAFETY: b is the base of a boxed SharedPtrCountFor<T>.
        unsafe { drop(Box::from_raw(b as *mut Self)) };
    }
}

/// A counter embedded in an object so that it can produce [`SharedPtr`]s to itself: the type
/// embeds an `EnableSharedFromThis`, exposes it through [`SharedFromThis::esft`], and
/// [`make_shared_esft`] uses it as the object's counter.
#[repr(C)]
pub struct EnableSharedFromThis {
    base: SharedPtrCountBase,
}

impl EnableSharedFromThis {
    /// A detached counter (set up by [`make_shared_esft`]).
    #[inline]
    pub fn new() -> Self {
        Self { base: SharedPtrCountBase::new(Self::not_shared) }
    }

    unsafe fn not_shared(_: *mut SharedPtrCountBase) {}

    /// The number of owning pointers to the object.
    #[inline]
    pub fn use_count(&self) -> i64 {
        self.base.count.get()
    }
}

impl Default for EnableSharedFromThis {
    fn default() -> Self {
        Self::new()
    }
}

/// Types that embed an [`EnableSharedFromThis`] and can produce [`SharedPtr`]s to themselves.
///
/// The place of the embedded counter is the associated
/// offset, which [`esft`](Self::esft) and [`make_shared_esft`] use for pointer
/// arithmetic. Implement it with [`shared_from_this_impl!`](crate::shared_from_this_impl),
/// which takes the offset from `offset_of!` and checks the field's type.
///
/// # Safety
///
/// `ESFT_OFFSET` must be `offset_of!(Self, field)` of a field of `Self` whose type is
/// [`EnableSharedFromThis`]; any other value lets safe code read and write outside the
/// object.
pub unsafe trait SharedFromThis: Sized {
    /// Offset of the embedded `EnableSharedFromThis` (`offset_of!(Self, field)`).
    const ESFT_OFFSET: usize;

    /// The embedded [`EnableSharedFromThis`].
    #[inline]
    fn esft(&self) -> &EnableSharedFromThis {
        // SAFETY: by the trait's contract ESFT_OFFSET is the offset of an
        // EnableSharedFromThis field of Self, so the pointer stays inside `*self`, is
        // aligned and points to an initialised value borrowed for as long as `self`.
        unsafe {
            &*((self as *const Self as *const u8).add(Self::ESFT_OFFSET)
                as *const EnableSharedFromThis)
        }
    }

    /// A new owning pointer to the object at `this` (the count is incremented).
    ///
    /// # Safety
    ///
    /// `this` must point to an object created by [`make_shared_esft`], and be derived
    /// from a pointer of one of its `SharedPtr`s (the last one may free the object through
    /// it, which a pointer derived from a reference cannot do).
    #[inline]
    unsafe fn shared_from_this(this: *const Self) -> SharedPtr<Self> {
        // SAFETY: guaranteed by the caller; the counter is derived from the object pointer.
        unsafe {
            let p = this.cast_mut();
            let b = esft_base_ptr(p);
            SharedPtr::from_parts(b, p)
        }
    }
}

/// Implements [`SharedFromThis`] for a type and its [`EnableSharedFromThis`] field:
/// `shared_from_this_impl!(Connection { _esft });` (generic types:
/// `shared_from_this_impl!(<T: Bound> Tcb<T> { _esft });`). The offset comes from
/// `offset_of!` and the field's type is checked at compile time, so the `unsafe impl`
/// it writes always meets the trait's contract.
#[macro_export]
macro_rules! shared_from_this_impl {
    (<$($gen:ident $(: $bound:path)?),+> $ty:ty { $field:ident }) => {
        // SAFETY: offset_of! gives the offset of `$field`, whose type the const below
        // checks is EnableSharedFromThis.
        unsafe impl<$($gen $(: $bound)?),+> $crate::engine::core::shared_ptr::SharedFromThis for $ty {
            const ESFT_OFFSET: usize = {
                let _check: fn(&$ty) -> &$crate::engine::core::shared_ptr::EnableSharedFromThis =
                    |v| &v.$field;
                ::std::mem::offset_of!($ty, $field)
            };
        }
    };
    ($ty:ty { $field:ident }) => {
        // SAFETY: offset_of! gives the offset of `$field`, whose type the const below
        // checks is EnableSharedFromThis.
        unsafe impl $crate::engine::core::shared_ptr::SharedFromThis for $ty {
            const ESFT_OFFSET: usize = {
                let _check: fn(&$ty) -> &$crate::engine::core::shared_ptr::EnableSharedFromThis =
                    |v| &v.$field;
                ::std::mem::offset_of!($ty, $field)
            };
        }
    };
}

/// The embedded counter of `p`, derived from `p`.
///
/// # Safety
///
/// `p` must point to a live `T`.
#[inline]
unsafe fn esft_base_ptr<T: SharedFromThis>(p: *mut T) -> *mut SharedPtrCountBase {
    // SAFETY: guaranteed by the caller; ESFT_OFFSET is the offset of an
    // EnableSharedFromThis field of T.
    unsafe {
        let e = p.cast::<u8>().add(T::ESFT_OFFSET).cast::<EnableSharedFromThis>();
        ptr::addr_of_mut!((*e).base)
    }
}

impl<T: SharedFromThis> SharedPtr<T> {
    /// [`SharedFromThis::shared_from_this`] for an object reached through a raw pointer.
    ///
    /// # Safety
    ///
    /// As [`SharedFromThis::shared_from_this`].
    #[inline]
    pub unsafe fn shared_from_this(this: *const T) -> Self {
        // SAFETY: forwarded from the caller.
        unsafe { T::shared_from_this(this) }
    }
}

/// A two-word, non-atomic, reference-counted pointer supporting polymorphism (`T` may be a
/// `dyn Trait`); see the [module documentation](self).
///
/// It may be null. The last owner destroys the object through the counter's typed
/// destructor, whatever view `T` it holds.
pub struct SharedPtr<T: ?Sized> {
    b: *mut SharedPtrCountBase,
    p: Option<NonNull<T>>,
}

impl<T: ?Sized> SharedPtr<T> {
    /// A new owner of the object `p` counted by `b` (the count is incremented), or a null
    /// pointer for a null `b`.
    ///
    /// # Safety
    ///
    /// `b` must be null or the counter of a live object that `p` points into.
    #[inline]
    pub unsafe fn from_parts(b: *mut SharedPtrCountBase, p: *mut T) -> Self {
        if !b.is_null() {
            // SAFETY: guaranteed by the caller.
            unsafe { (*b).count.inc() };
        }
        Self { b, p: NonNull::new(p) }
    }

    /// Whether the pointer is not null.
    #[inline]
    pub fn is_some(&self) -> bool {
        self.p.is_some()
    }

    /// The object's address, or `None` for a null pointer.
    #[inline]
    pub fn get(&self) -> Option<NonNull<T>> {
        self.p
    }

    /// Mutable access to the object.
    ///
    /// # Safety
    ///
    /// The pointer must be engaged and no other reference to the object may be in use
    /// while the returned one is.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_mut_unchecked(&self) -> &mut T {
        // SAFETY: guaranteed by the caller.
        unsafe { &mut *self.p.unwrap_unchecked().as_ptr() }
    }

    /// The number of owners, 0 for a null pointer.
    #[inline]
    pub fn use_count(&self) -> i64 {
        if self.b.is_null() {
            0
        } else {
            // SAFETY: a non-null counter is alive while we hold a reference.
            unsafe { (*self.b).count.get() }
        }
    }

    #[inline]
    fn addr(&self) -> *const u8 {
        self.p.map_or(ptr::null(), |p| p.as_ptr() as *const u8)
    }
}

impl<T> SharedPtr<T> {
    /// A null pointer.
    #[inline]
    pub const fn null() -> Self {
        Self { b: ptr::null_mut(), p: None }
    }
}

impl<T> Default for SharedPtr<T> {
    #[inline]
    fn default() -> Self {
        Self::null()
    }
}

impl<T: ?Sized> Clone for SharedPtr<T> {
    #[inline]
    fn clone(&self) -> Self {
        if !self.b.is_null() {
            // SAFETY: a non-null counter is alive while we hold a reference.
            unsafe { (*self.b).count.inc() };
        }
        Self { b: self.b, p: self.p }
    }
}

impl<T: ?Sized> Drop for SharedPtr<T> {
    #[inline]
    fn drop(&mut self) {
        if !self.b.is_null() {
            // SAFETY: we hold one reference; `destroy` is the fully-typed deleter.
            unsafe {
                if (*self.b).count.dec() == 0 {
                    ((*self.b).destroy)(self.b);
                }
            }
        }
    }
}

impl<T: ?Sized> Deref for SharedPtr<T> {
    type Target = T;
    /// The object.
    #[inline]
    ///
    /// # Panics
    ///
    /// Panics if the pointer is null.
    fn deref(&self) -> &T {
        match self.p {
            // SAFETY: an engaged pointer refers to a live object, kept alive by `self`.
            Some(p) => unsafe { p.as_ref() },
            None => null_deref(),
        }
    }
}

/// Allocates `value` after a counter header and returns the first [`SharedPtr`] to it.
#[inline]
pub fn make_shared<T>(a: T) -> SharedPtr<T> {
    let b = Box::into_raw(Box::new(SharedPtrCountFor {
        base: SharedPtrCountBase::new(SharedPtrCountFor::<T>::destroy),
        data: a,
    }));
    // SAFETY: b is a freshly allocated object; data lives inside it.
    unsafe { SharedPtr::from_parts(b as *mut SharedPtrCountBase, &raw mut (*b).data) }
}

/// A new `shared_ptr` sharing ownership of an object that [`make_shared`] placed: the counter
/// sits right before the object in the same block.
///
/// # Safety
///
/// `p` points to the `data` of a live block made by [`make_shared::<T>`] (with the provenance
/// of the whole block, as the pointers the `SharedPtr` hands out have), on its shard.
#[inline]
pub(crate) unsafe fn shared_from_make_shared<T>(p: *const T) -> SharedPtr<T> {
    let offset = std::mem::offset_of!(SharedPtrCountFor<T>, data);
    // SAFETY: by the caller `p` is the `data` field of a repr(C) `SharedPtrCountFor<T>`, whose
    // `base` is its first field, `offset` bytes before `data`.
    unsafe {
        let b = p.cast_mut().byte_sub(offset).cast::<SharedPtrCountBase>();
        SharedPtr::from_parts(b, p.cast_mut())
    }
}

/// Allocates `value` and returns the first [`SharedPtr`] to it, counting with the
/// [`EnableSharedFromThis`] the type embeds.
#[inline]
pub fn make_shared_esft<T: SharedFromThis>(a: T) -> SharedPtr<T> {
    unsafe fn destroy<T: SharedFromThis>(b: *mut SharedPtrCountBase) {
        // SAFETY: b is the embedded counter (first field of the repr(C)
        // EnableSharedFromThis) of a boxed T, found back through its offset.
        unsafe { drop(Box::from_raw((b as *mut u8).sub(T::ESFT_OFFSET) as *mut T)) };
    }
    let p = Box::into_raw(Box::new(a));
    // SAFETY: p is a freshly allocated T that embeds its counter; the counter pointer is
    // derived from p, so it may be written and the object freed through it.
    unsafe {
        let b = esft_base_ptr(p);
        (*b).destroy = destroy::<T>;
        SharedPtr::from_parts(b, p)
    }
}

/// A new owner of `p`'s object as another type; `cast` converts the object pointer (an
/// upcast to a trait object, or a downcast known to be right). A null `p` gives null.
///
/// # Safety
///
/// `cast` must return a pointer into the same object.
#[inline]
pub unsafe fn static_pointer_cast<T: ?Sized, U: ?Sized>(
    p: &SharedPtr<U>,
    cast: impl FnOnce(*mut U) -> *mut T,
) -> SharedPtr<T> {
    // A null `p` gives a null result (a `?Sized` target has no null pointer to cast to).
    let q = p.p.and_then(|x| NonNull::new(cast(x.as_ptr())));
    if !p.b.is_null() {
        // SAFETY: guaranteed by the caller: the counter is the live object's.
        unsafe { (*p.b).count.inc() };
    }
    SharedPtr { b: p.b, p: q }
}

/// Converts `p` into a pointer to another view of the same object (an upcast to a trait
/// object), taking over its reference without touching the count.
///
/// # Safety
///
/// `cast` must return a pointer into the same object.
#[inline]
pub unsafe fn shared_ptr_converting_move<T: ?Sized, U: ?Sized>(
    p: SharedPtr<U>,
    cast: impl FnOnce(*mut U) -> *mut T,
) -> SharedPtr<T> {
    let p = std::mem::ManuallyDrop::new(p);
    SharedPtr { b: p.b, p: p.p.and_then(|x| NonNull::new(cast(x.as_ptr()))) }
}

/// A new owner of `p`'s object as another type when the checked downcast `cast` succeeds, or
/// a null pointer when it fails.
#[inline]
pub fn dynamic_pointer_cast<T, U: ?Sized>(
    p: &SharedPtr<U>,
    cast: impl FnOnce(&U) -> Option<&T>,
) -> SharedPtr<T> {
    let q: *mut T = match p.p {
        // SAFETY: the object is alive while p is.
        Some(x) => cast(unsafe { x.as_ref() }).map_or(ptr::null_mut(), |r| r as *const T as *mut T),
        None => ptr::null_mut(),
    };
    let b = if q.is_null() { ptr::null_mut() } else { p.b };
    // SAFETY: q points into the object counted by b (or both are null).
    unsafe { SharedPtr::from_parts(b, q) }
}

/// A copy of `p`; exists for symmetry with the other casts (pointees have no constness to
/// remove).
#[inline]
pub fn const_pointer_cast<T: ?Sized>(p: &SharedPtr<T>) -> SharedPtr<T> {
    p.clone()
}

impl<T: ?Sized, U: ?Sized> PartialEq<SharedPtr<U>> for SharedPtr<T> {
    #[inline]
    fn eq(&self, y: &SharedPtr<U>) -> bool {
        self.addr() == y.addr()
    }
}

impl<T: ?Sized> Eq for SharedPtr<T> {}

impl<T: ?Sized, U: ?Sized> PartialOrd<SharedPtr<U>> for SharedPtr<T> {
    #[inline]
    fn partial_cmp(&self, y: &SharedPtr<U>) -> Option<Ordering> {
        self.addr().partial_cmp(&y.addr())
    }
}

impl<T: ?Sized> Ord for SharedPtr<T> {
    #[inline]
    fn cmp(&self, y: &Self) -> Ordering {
        self.addr().cmp(&y.addr())
    }
}

impl<T: ?Sized + fmt::Display> fmt::Display for SharedPtr<T> {
    /// Prints the object, or `null` for a null pointer.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.p.is_none() {
            return f.write_str("null");
        }
        write!(f, "{}", &**self)
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for SharedPtr<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.p.is_none() {
            return f.write_str("null");
        }
        write!(f, "{:?}", &**self)
    }
}

/// Compares two [`SharedPtr`]s by the values they point to.
pub type SharedPtrEqualByValue<T> = crate::engine::util::indirect::IndirectEqualTo<SharedPtr<T>>;
/// Hashes a [`SharedPtr`] by the value it points to.
pub type SharedPtrValueHash<T> = crate::engine::util::indirect::IndirectHash<SharedPtr<T>>;

impl<T> Hash for LwSharedPtr<T> {
    /// Hashes the object address.
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.get().hash(state);
    }
}

impl<T: ?Sized> Hash for SharedPtr<T> {
    /// Hashes the object address.
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.addr().hash(state);
    }
}

/// The object address of `p`, for printing with `{:p}`.
#[inline]
pub fn fmt_ptr_lw<T>(p: &LwSharedPtr<T>) -> *const () {
    p.get() as *const ()
}

/// The object address of `p` (null for a null pointer), for printing with `{:p}`.
#[inline]
pub fn fmt_ptr<T: ?Sized>(p: &SharedPtr<T>) -> *const () {
    p.addr() as *const ()
}

impl<T: ?Sized> crate::engine::util::is_smart_ptr::IsSmartPtr for SharedPtr<T> {
    const VALUE: bool = true;
}

impl<T> crate::engine::util::is_smart_ptr::IsSmartPtr for LwSharedPtr<T> {
    const VALUE: bool = true;
}

/// A [`SharedPtr`] is nullable by itself, so the indirect comparison and hashing functors of
/// [`indirect`](crate::engine::util::indirect) take it directly.
impl<T: ?Sized> crate::engine::util::indirect::NullablePointer for SharedPtr<T> {
    type Element = T;
    #[inline]
    fn get(&self) -> Option<&T> {
        // SAFETY: an engaged shared pointer refers to a live object, which it keeps alive at
        // least as long as `self` is borrowed.
        SharedPtr::get(self).map(|p| unsafe { &*p.as_ptr() })
    }
}

/// An [`LwSharedPtr`] is nullable by itself, as a [`SharedPtr`] is.
impl<T> crate::engine::util::indirect::NullablePointer for LwSharedPtr<T> {
    type Element = T;
    #[inline]
    fn get(&self) -> Option<&T> {
        // SAFETY: as for `SharedPtr`; a null pointer gives `None`.
        unsafe { LwSharedPtr::get(self).as_ref() }
    }
}
