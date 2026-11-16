//! Non-owning, allocation-free weak references to single-shard objects.
//!
//! An object that wants weak references embeds a [`WeaklyReferencable`] field (declared with
//! [`weakly_referencable!`](crate::weakly_referencable)). Every linked [`WeakPtr`] to it sits in an intrusive auto-unlink
//! list owned by that field, and the field's destructor walks the list and nulls each pointer.
//! Nothing is allocated, and [`WeakPtr::get`] reads the pointer stored in the weak pointer
//! itself. Like the rest of the per-shard data, weak pointers are not thread-safe.
//!
//! Because the list stores the addresses of the weak pointers, a linked weak pointer must not
//! move: the operations that link one take it pinned. A [`WeakPtr`] moves freely while it is
//! null, and [`weak_from_this`], [`WeakPtr::assign`] and [`WeakPtr::assign_from`] fill a pinned
//! slot. The referenced object is pinned too, since its address is captured by the weak
//! pointers.
//!
//! The referenced object and the weak pointers are `!Unpin` (they embed a `ListHook`), so the
//! hook writes that a weak pointer makes into the object's list (the sentinel) do not conflict
//! with the uniqueness of a `&mut` to that object.

use std::cell::Cell;
use std::marker::PhantomData;
use std::pin::Pin;
use std::ptr::{self, NonNull};

use crate::engine::core::api::intrusive::{Adapter, List, ListHook};

/// A type a [`WeakPtr`] can point to: every sized type, and the trait objects declared with
/// [`weak_ptr_target!`](crate::weak_ptr_target), which let a weak pointer refer to an object through a trait it
/// implements.
///
/// A weak pointer keeps the object's address in its list node, where the object's destructor
/// nulls it; `Meta` is what it keeps besides, to rebuild a `*mut Self` from that address:
/// nothing for a sized type, so a `WeakPtr<T>` stays one hook and one pointer.
///
/// # Safety
///
/// `from_parts(p.cast(), meta_of(p))` must give back `p`.
pub unsafe trait WeakTarget {
    /// What is kept besides the address.
    type Meta: Copy;
    /// The `Meta` of a null weak pointer.
    const NO_META: Self::Meta;
    /// The `Meta` of a pointer to an object.
    fn meta_of(p: NonNull<Self>) -> Self::Meta;
    /// The pointer with address `addr` and `meta`.
    ///
    /// # Safety
    ///
    /// `meta` must be the `meta_of` a pointer whose address is `addr`.
    unsafe fn from_parts(addr: NonNull<()>, meta: Self::Meta) -> NonNull<Self>;
}

// SAFETY: a thin pointer is its address.
unsafe impl<T> WeakTarget for T {
    type Meta = ();
    const NO_META: () = ();
    #[inline(always)]
    fn meta_of(_p: NonNull<T>) {}
    #[inline(always)]
    unsafe fn from_parts(addr: NonNull<()>, _meta: ()) -> NonNull<T> {
        addr.cast()
    }
}

/// Declares the trait object `dyn $tr` a [`WeakTarget`], so a `WeakPtr<dyn $tr>` can refer to
/// an object whose type implements the trait, taken over from a
/// `WeakPtr` to it with [`WeakPtr::take_from`]. The weak pointer keeps the whole trait object
/// pointer besides the address.
#[macro_export]
macro_rules! weak_ptr_target {
    (dyn $tr:path) => {
        // SAFETY: `Meta` is the whole pointer, given back as is.
        unsafe impl $crate::engine::core::weak_ptr::WeakTarget for dyn $tr {
            type Meta = ::std::option::Option<::std::ptr::NonNull<dyn $tr>>;
            const NO_META: Self::Meta = ::std::option::Option::None;
            #[inline(always)]
            fn meta_of(p: ::std::ptr::NonNull<dyn $tr>) -> Self::Meta {
                ::std::option::Option::Some(p)
            }
            #[inline(always)]
            unsafe fn from_parts(
                _addr: ::std::ptr::NonNull<()>,
                meta: Self::Meta,
            ) -> ::std::ptr::NonNull<dyn $tr> {
                // SAFETY: by the caller, `meta` is the `meta_of` a pointer, so it is `Some`.
                unsafe { meta.unwrap_unchecked() }
            }
        }
    };
}

/// The part of a weak pointer that the referenced object's list links and nulls: the hook
/// and the object's address. It is the same for a weak pointer of any type, so weak pointers
/// to the object as itself and as a base share the object's list.
#[repr(C)]
struct WeakPtrNode {
    _hook: ListHook,
    _ptr: Cell<*mut ()>,
}

/// A non-owning reference to an object.
///
/// When the object is destroyed, it nulls every weak pointer to it; a weak pointer to a
/// destroyed object is equivalent to a null pointer.
///
/// The referenced object must embed a [`WeaklyReferencable`]; weak pointers to it are obtained
/// only through [`weak_from_this`] (or by copying or moving another weak pointer).
#[repr(C)]
pub struct WeakPtr<T: ?Sized + WeakTarget> {
    // First field: a pointer to the weak pointer is a pointer to its node.
    _node: WeakPtrNode,
    _meta: Cell<T::Meta>,
    _marker: PhantomData<*mut T>,
}

/// The adapter of the list of weak pointers to one object, linking through their `_hook`.
struct WeakPtrLink;

// SAFETY: OFFSET is the offset of the `_hook` field (a `ListHook`) inside `WeakPtrNode`.
unsafe impl Adapter for WeakPtrLink {
    type Value = WeakPtrNode;
    const OFFSET: usize = std::mem::offset_of!(WeakPtrNode, _hook);
}

impl<T: ?Sized + WeakTarget> WeakPtr<T> {
    /// A null, unlinked weak pointer.
    #[inline]
    pub const fn new() -> Self {
        Self {
            _node: WeakPtrNode { _hook: ListHook::new(), _ptr: Cell::new(ptr::null_mut()) },
            _meta: Cell::new(T::NO_META),
            _marker: PhantomData,
        }
    }

    /// Unlinks the weak pointer and makes it null.
    #[inline]
    pub fn clear(&self) {
        self._node._hook.unlink();
        self._node._ptr.set(ptr::null_mut());
        self._meta.set(T::NO_META);
    }

    /// The node of this weak pointer, with the provenance of the whole weak pointer (it is
    /// stored in the object's list and turned back into the node there).
    #[inline]
    fn node_ptr(&self) -> *mut WeakPtrNode {
        ptr::from_ref(self).cast_mut().cast::<WeakPtrNode>()
    }

    /// Takes over the reference of `o` (move), which becomes null. Any reference `self` held
    /// is released first.
    #[inline]
    pub fn assign_from(self: Pin<&mut Self>, o: Pin<&mut WeakPtr<T>>) {
        if ptr::eq(&*self, &*o) {
            return;
        }
        let meta = o._meta.get();
        // SAFETY: both are pinned; `o` keeps its meta until it is cleared below.
        unsafe { self.take_node(&o, meta) };
    }

    /// Takes over the reference of `o`, a weak pointer to the same object as a `U`, which
    /// becomes null. `convert` turns the pointer to `U` into a pointer to `T` (an unsizing cast,
    /// `|p| p as *mut dyn Base`, which keeps the address).
    #[inline]
    pub fn take_from<U: ?Sized + WeakTarget>(
        self: Pin<&mut Self>,
        o: Pin<&mut WeakPtr<U>>,
        convert: impl FnOnce(NonNull<U>) -> NonNull<T>,
    ) {
        let meta = match o.as_ptr() {
            Some(p) => T::meta_of(convert(p)),
            None => T::NO_META,
        };
        // SAFETY: both are pinned; `meta` is that of the converted pointer to `o`'s object.
        unsafe { self.take_node(&o, meta) };
    }

    /// The move itself: this weak pointer takes `o`'s place in the object's list and its
    /// address, with `meta`, and `o` is left null and unlinked.
    ///
    /// # Safety
    ///
    /// `self` and `o` must be pinned and distinct, and `meta` must go with `o`'s address.
    #[inline]
    unsafe fn take_node<U: ?Sized + WeakTarget>(
        self: Pin<&mut Self>,
        o: &WeakPtr<U>,
        meta: T::Meta,
    ) {
        self.clear();
        let addr = o._node._ptr.get();
        if addr.is_null() {
            return;
        }
        self._node._ptr.set(addr);
        self._meta.set(meta);
        // SAFETY: `o` is linked (a non-null weak pointer is in its object's list) and pinned,
        // and `self` is pinned and was just unlinked; its hook pointer comes from the pointer
        // to the whole weak pointer.
        unsafe { o._node._hook.replace_with(WeakPtrLink::hook(self.node_ptr())) };
        o._node._ptr.set(ptr::null_mut());
        o._meta.set(U::NO_META);
    }

    /// Whether the object is alive.
    #[inline]
    pub fn is_some(&self) -> bool {
        !self._node._ptr.get().is_null()
    }

    /// The object as a pointer that may be unsized, or `None` once it is destroyed.
    #[inline]
    pub fn as_ptr(&self) -> Option<NonNull<T>> {
        let addr = NonNull::new(self._node._ptr.get())?;
        // SAFETY: the meta was set with this address (`link_to`, `take_node`).
        Some(unsafe { T::from_parts(addr, self._meta.get()) })
    }

    /// The object as a shared reference, if alive.
    ///
    /// # Safety
    ///
    /// No mutable reference to the object may be used while the returned reference is live
    /// (the owner does not know about its weak pointers), and the object must not be
    /// destroyed while it is live.
    #[inline]
    pub unsafe fn as_ref<'a>(&self) -> Option<&'a T> {
        // SAFETY: a non-null pointer is a live object; the rest is the caller's contract.
        self.as_ptr().map(|p| unsafe { &*p.as_ptr() })
    }

    /// The object as a mutable reference, if alive.
    ///
    /// # Safety
    ///
    /// No other reference to the object may be used while the returned reference is live,
    /// and the object must not be destroyed or moved out of its pin while it is live.
    #[inline]
    pub unsafe fn as_mut<'a>(&self) -> Option<&'a mut T> {
        // SAFETY: a non-null pointer is a live object; the rest is the caller's contract.
        self.as_ptr().map(|p| unsafe { &mut *p.as_ptr() })
    }
}

impl<T> WeakPtr<T> {
    /// Links this (pinned) weak pointer into the list of the live object `p`.
    ///
    /// # Safety
    ///
    /// `p` must point to a live, pinned object, and `self` must be unlinked and pinned.
    #[inline]
    unsafe fn link_to(&self, p: *mut T)
    where
        T: WeaklyReferencableObject,
    {
        self._node._ptr.set(p.cast());
        // SAFETY: by the caller, `p` is live and pinned, so its embedded
        // `WeaklyReferencable` (at `T::OFFSET`) is live and in place, and `self` stays in
        // place while linked; it unlinks itself on drop.
        unsafe {
            let wr = p.byte_add(T::OFFSET).cast::<WeaklyReferencable<T>>();
            (*wr)._ptr_list.push_back(self.node_ptr());
        }
    }

    /// Makes this weak pointer refer to the object `o` refers to (copy), or null if `o` is
    /// null. Any reference `self` held is released first.
    #[inline]
    pub fn assign(self: Pin<&mut Self>, o: &WeakPtr<T>)
    where
        T: WeaklyReferencableObject,
    {
        if ptr::eq(&*self, o) {
            return;
        }
        self.clear();
        let p = o.get();
        if !p.is_null() {
            // SAFETY: a non-null pointer in a weak pointer is a live object (its destructor
            // nulls every weak pointer first), pinned by `weak_from_this`; `self` is pinned
            // and was just unlinked.
            unsafe { self.link_to(p) };
        }
    }

    /// The object, or null once it is destroyed.
    #[inline]
    pub fn get(&self) -> *mut T {
        self._node._ptr.get().cast()
    }
}

impl<T: ?Sized + WeakTarget> Default for WeakPtr<T> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<T: ?Sized + WeakTarget> PartialEq for WeakPtr<T> {
    /// Both point to the same object, or both are null.
    #[inline]
    fn eq(&self, o: &Self) -> bool {
        self._node._ptr.get() == o._node._ptr.get()
    }
}

impl<T: ?Sized + WeakTarget> Eq for WeakPtr<T> {}

impl<T: ?Sized + WeakTarget> std::fmt::Debug for WeakPtr<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("WeakPtr").field(&self._node._ptr.get()).finish()
    }
}

/// The field that lets an object hand out non-owning references ([`WeakPtr`]) to itself.
///
/// A live weak pointer does not keep the object alive. The pointer it holds is valid as long as
/// the object is alive; when the object is dropped, this field's destructor nulls every weak
/// pointer to it.
///
/// The object embeds this field, declares it with [`weakly_referencable!`](crate::weakly_referencable), and fills a pinned
/// [`WeakPtr`] slot with [`weak_from_this`] from a pinned object. A holder of the weak pointer
/// checks [`WeakPtr::is_some`] (or that [`WeakPtr::get`] is not null) before using it.
pub struct WeaklyReferencable<T> {
    _ptr_list: List<WeakPtrLink>,
    _marker: PhantomData<*mut T>,
}

impl<T> WeaklyReferencable<T> {
    /// The field with no weak pointer yet.
    #[inline]
    pub const fn new() -> Self {
        Self { _ptr_list: List::new(), _marker: PhantomData }
    }
}

impl<T> Default for WeaklyReferencable<T> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for WeaklyReferencable<T> {
    /// Unlinks and nulls every weak pointer to the object.
    fn drop(&mut self) {
        while let Some(wp) = self._ptr_list.pop_front() {
            // SAFETY: a linked node is the node of a live, pinned weak pointer (of this type
            // or of a base); `_ptr` is a Cell.
            unsafe { (*wp.as_ptr())._ptr.set(ptr::null_mut()) };
        }
    }
}

/// An object that embeds a [`WeaklyReferencable<Self>`]. Implement it with
/// [`weakly_referencable!`](crate::weakly_referencable).
///
/// # Safety
///
/// `OFFSET` must be the byte offset of a `WeaklyReferencable<Self>` field of `Self`, and
/// `Self` must not implement `Unpin` (the embedded list makes it `!Unpin`; a manual
/// `impl Unpin` would let a pinned object move while weak pointers point to it).
pub unsafe trait WeaklyReferencableObject: Sized {
    /// `offset_of!(Self, field)` of the embedded `WeaklyReferencable<Self>`.
    const OFFSET: usize;
}

/// Declares `$ty` a [`WeaklyReferencableObject`] whose `WeaklyReferencable<$ty>` is the field
/// `$field`.
#[macro_export]
macro_rules! weakly_referencable {
    ($ty:ty { $field:ident }) => {
        // SAFETY: offset_of! gives the offset of the field; the field's list makes the type
        // !Unpin, and the macro adds no Unpin impl.
        unsafe impl $crate::engine::core::weak_ptr::WeaklyReferencableObject for $ty {
            const OFFSET: usize = ::std::mem::offset_of!($ty, $field);
        }
    };
}

/// Makes the pinned slot `slot` a weak pointer to `this`; any reference the slot held is
/// released first.
#[inline]
pub fn weak_from_this<T: WeaklyReferencableObject>(this: Pin<&mut T>, slot: Pin<&mut WeakPtr<T>>) {
    // SAFETY: the object is not moved; only its address is taken.
    let p: *mut T = unsafe { this.get_unchecked_mut() };
    slot.clear();
    // SAFETY: `p` is live and pinned (`T: !Unpin` by the trait contract, so the pin keeps it
    // in place until it is dropped, and its drop nulls the slot); the slot is pinned and was
    // just unlinked.
    unsafe { slot.link_to(p) };
}
