//! Allocation of C structures that end in a variable-length array.
//!
//! Some C interfaces take a structure whose last member is a zero-length array followed, in the
//! same allocation, by `n` elements, for example `struct xx { int a; float b[0]; }`.
//! [`make_struct_with_vla`] allocates such a block with `malloc`, default-initialises the fixed
//! part and returns it as a [`StructWithVla`], which frees the block on drop.

use std::ptr::NonNull;

unsafe extern "C" {
    fn malloc(size: usize) -> *mut std::ffi::c_void;
    fn free(p: *mut std::ffi::c_void);
}

/// Owns an `S` placed in `malloc` storage and followed by its variable-length array.
///
/// Dropping it releases the storage with `free` without running `S`'s destructor: `S` is meant
/// to be a plain C structure with nothing to drop.
pub struct StructWithVla<S> {
    p: NonNull<S>,
}

impl<S> StructWithVla<S> {
    /// Raw pointer to the structure, to pass to the C interface; the trailing array starts at
    /// `size_of::<S>()` bytes past it.
    #[inline]
    pub fn get(&self) -> *mut S {
        self.p.as_ptr()
    }
}

impl<S> std::ops::Deref for StructWithVla<S> {
    type Target = S;
    #[inline]
    fn deref(&self) -> &S {
        // SAFETY: `p` holds an `S` initialised by `make_struct_with_vla` and owned by `self`.
        unsafe { self.p.as_ref() }
    }
}

impl<S> std::ops::DerefMut for StructWithVla<S> {
    #[inline]
    fn deref_mut(&mut self) -> &mut S {
        // SAFETY: `p` holds an initialised `S` uniquely owned by `self`, and `&mut self` excludes
        // any other borrow.
        unsafe { self.p.as_mut() }
    }
}

impl<S> Drop for StructWithVla<S> {
    fn drop(&mut self) {
        // SAFETY: `p` was returned by `malloc` and is freed exactly once, here. `S`'s destructor
        // is deliberately not run: `S` is a plain C struct.
        unsafe { free(self.p.as_ptr().cast()) };
    }
}

/// Allocates an `S` followed by `nr` elements of type `E`.
///
/// `offset` is the byte offset of the trailing array inside `S` (`offset_of!(S, last)`), which
/// must equal `size_of::<S>()`. The fixed part is set to `S::default()`; the trailing elements
/// are left uninitialised.
///
/// # Panics
///
/// Fails an internal assertion (aborting) when `offset != size_of::<S>()`, and calls
/// [`std::alloc::handle_alloc_error`] when `malloc` returns null.
#[inline]
pub fn make_struct_with_vla<S: Default, E>(offset: usize, nr: usize) -> StructWithVla<S> {
    let element_size = std::mem::size_of::<E>();
    if offset != std::mem::size_of::<S>() {
        crate::engine::util::assert::internal::assert_fail(
            "offset == sizeof(S)",
            file!(),
            line!() as i32,
            "make_struct_with_vla",
        );
    }
    // SAFETY: malloc returns storage aligned for any fundamental type, of the requested size.
    let p = unsafe { malloc(offset + element_size * nr) }.cast::<S>();
    let Some(p) = NonNull::new(p) else {
        std::alloc::handle_alloc_error(std::alloc::Layout::new::<S>());
    };
    // SAFETY: `p` is fresh, non-null, suitably aligned storage of at least `size_of::<S>()`
    // bytes, so writing an `S` into it is valid.
    unsafe { p.as_ptr().write(S::default()) };
    StructWithVla { p }
}
