//! Owned, aligned byte buffers released with `free()`.
//!
//! [`allocate_aligned_buffer`] returns an [`AlignedBuffer`] whose start is aligned to the
//! requested boundary, as needed for direct (DMA) file I/O. The buffer frees its memory
//! with the C library `free()` when dropped, unless ownership is given up with
//! [`AlignedBuffer::release`].

use std::ptr::NonNull;

use crate::engine::core::future::{BadAlloc, ExceptionPtr};

unsafe extern "C" {
    fn free(p: *mut std::ffi::c_void);
}

/// Internal allocation entry point.
pub mod internal {
    /// Allocates `size` bytes aligned to `align`; implemented by the memory module.
    pub use crate::engine::core::memory::allocate_aligned_buffer_impl;
}

/// A deleter that releases memory with the C library `free()`.
#[derive(Clone, Copy, Default, Debug)]
pub struct FreeDeleter;

impl FreeDeleter {
    /// Frees `p` with `free()`.
    ///
    /// # Safety
    /// `p` is null or a live allocation of the malloc family, not used afterwards.
    #[inline]
    pub unsafe fn call(&self, p: *mut std::ffi::c_void) {
        // SAFETY: guaranteed by the caller.
        unsafe { free(p) };
    }
}

/// An owned byte buffer, aligned at allocation, freed with `free()` on drop.
pub struct AlignedBuffer {
    p: NonNull<u8>,
}

impl AlignedBuffer {
    /// The start of the buffer.
    #[inline]
    pub fn get(&self) -> *mut u8 {
        self.p.as_ptr()
    }

    /// Gives up ownership and returns the start of the buffer; the caller must free it
    /// with `free()`.
    #[inline]
    pub fn release(self) -> *mut u8 {
        let p = self.p.as_ptr();
        std::mem::forget(self);
        p
    }
}

impl Drop for AlignedBuffer {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: the buffer owns its allocation, made by allocate_aligned_buffer_impl
        // with the malloc family, and nothing uses it after drop.
        unsafe { FreeDeleter.call(self.p.as_ptr().cast()) };
    }
}

/// Allocates `size` bytes aligned to `align` (a power of two).
///
/// # Errors
/// Returns a [`BadAlloc`] exception when the allocation fails or yields a null pointer.
#[inline]
pub fn allocate_aligned_buffer(size: usize, align: usize) -> Result<AlignedBuffer, ExceptionPtr> {
    let ret = internal::allocate_aligned_buffer_impl(size, align)?;
    match NonNull::new(ret.cast::<u8>()) {
        Some(p) => Ok(AlignedBuffer { p }),
        None => Err(ExceptionPtr::from(BadAlloc)),
    }
}
