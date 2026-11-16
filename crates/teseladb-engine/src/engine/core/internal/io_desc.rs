//! The completion header shared by every object the kernel completes.
//!
//! Objects that travel to the kernel do so as a single pointer (the `user_data` of a linux-aio
//! control block or of an io_uring submission entry). A Rust fat `dyn` pointer does not fit in
//! that word, so every such object starts with a [`KernelCompletion`] header holding a pointer
//! to a static table of functions at offset 0. A thin `*mut KernelCompletion` then identifies
//! the object and dispatches to it, with no allocation and a single indirection.

/// The virtual function table of [`KernelCompletion`].
pub struct KernelCompletionVtable {
    /// Delivers the kernel's result to the object.
    pub complete_with: unsafe fn(this: *mut KernelCompletion, res: isize),
}

/// The header of every object that the kernel completes.
///
/// It must be the first field of a `#[repr(C)]` derived type, so that a pointer to the
/// derived object and a pointer to its `KernelCompletion` are the same address.
#[repr(C)]
pub struct KernelCompletion {
    vtable: &'static KernelCompletionVtable,
}

/// Implemented by every type that derives from [`KernelCompletion`].
///
/// # Safety
///
/// The implementing type must be `#[repr(C)]` and have a `KernelCompletion` built with
/// [`KernelCompletion::new::<Self>()`] at offset 0.
pub unsafe trait KernelCompletionImpl: Sized {
    /// Delivers the kernel's result `res` (a byte count or a negative errno). It may destroy
    /// the object and re-enter the reactor, so it receives the object's pointer.
    ///
    /// # Safety
    /// `this` points to a live object, with no reference to it live during the call.
    unsafe fn complete_with(this: *mut Self, res: isize);
}

unsafe fn complete_with_thunk<T: KernelCompletionImpl>(this: *mut KernelCompletion, res: isize) {
    // SAFETY: by the contract of `KernelCompletionImpl`, `this` points to the header at
    // offset 0 of a live `T`.
    unsafe { T::complete_with(this.cast::<T>(), res) }
}

/// The vtable of a concrete `T`, as a promoted constant (one per type).
struct VtableOf<T>(std::marker::PhantomData<T>);

impl<T: KernelCompletionImpl> VtableOf<T> {
    const VTABLE: KernelCompletionVtable =
        KernelCompletionVtable { complete_with: complete_with_thunk::<T> };
}

impl KernelCompletion {
    /// A header for the type `T` whose first field it is.
    #[inline]
    pub const fn new<T: KernelCompletionImpl>() -> Self {
        KernelCompletion { vtable: &VtableOf::<T>::VTABLE }
    }

    /// Builds a header with an explicit vtable, for header types that dispatch further
    /// themselves (such as the I/O completion).
    #[inline]
    pub const fn with_vtable(vtable: &'static KernelCompletionVtable) -> Self {
        KernelCompletion { vtable }
    }

    /// Delivers `res` to the object, dispatched through the vtable.
    ///
    /// # Safety
    ///
    /// `this` must point to the `KernelCompletion` header of a live object; the call may
    /// destroy the object.
    #[inline]
    pub unsafe fn complete_with(this: *mut KernelCompletion, res: isize) {
        // SAFETY: the caller guarantees that `this` is live; the vtable is 'static.
        unsafe { ((*this).vtable.complete_with)(this, res) }
    }

    /// The header of a derived object, as the thin pointer that goes to the kernel.
    #[inline]
    pub fn as_ptr<T: KernelCompletionImpl>(obj: &mut T) -> *mut KernelCompletion {
        (obj as *mut T).cast::<KernelCompletion>()
    }
}
