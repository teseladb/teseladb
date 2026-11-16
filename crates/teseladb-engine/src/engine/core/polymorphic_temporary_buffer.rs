//! Temporary buffers backed by a caller-chosen allocator.
//!
//! [`make_temporary_buffer`] returns a [`TemporaryBuffer`] whose memory comes from a
//! [`PolymorphicAllocator`]; the buffer's deleter returns the memory to that allocator
//! when the last share of the buffer is dropped. The default malloc allocator takes the
//! ordinary [`TemporaryBuffer::new`] path, with no extra deleter.

use crate::engine::core::deleter::{Deleter, make_deleter_with};
use crate::engine::core::memory::{MALLOC_ALLOCATOR, MallocAllocator};
use crate::engine::core::temporary_buffer::TemporaryBuffer;

/// A runtime-selected byte allocator: allocates and frees blocks of `n` bytes.
pub trait PolymorphicAllocator {
    /// Allocates `n` bytes and returns the start of the block.
    fn allocate(&self, n: usize) -> *mut u8;
    /// Frees a block of `n` bytes previously returned by [`PolymorphicAllocator::allocate`].
    ///
    /// # Safety
    /// `p` was returned by `allocate(n)` on this allocator, with the same `n`, and is not
    /// freed again.
    unsafe fn deallocate(&self, p: *mut u8, n: usize);
}

impl PolymorphicAllocator for MallocAllocator {
    #[inline]
    fn allocate(&self, n: usize) -> *mut u8 {
        MallocAllocator::allocate(self, n)
    }

    #[inline]
    unsafe fn deallocate(&self, p: *mut u8, n: usize) {
        // SAFETY: the trait's contract is `MallocAllocator::deallocate`'s.
        unsafe { MallocAllocator::deallocate(self, p, n) }
    }
}

/// Creates a [`TemporaryBuffer`] of `size` bytes allocated by `allocator`.
///
/// When `allocator` is the global [`MALLOC_ALLOCATOR`] the buffer is created with
/// [`TemporaryBuffer::new`]; otherwise its deleter calls `allocator.deallocate` once
/// the buffer and all its shares are gone.
pub fn make_temporary_buffer<A: PolymorphicAllocator + 'static>(
    allocator: &'static A,
    size: usize,
) -> TemporaryBuffer<u8> {
    if std::ptr::eq(
        allocator as *const A as *const (),
        MALLOC_ALLOCATOR as *const MallocAllocator as *const (),
    ) {
        return TemporaryBuffer::new(size);
    }
    let buffer = allocator.allocate(size);
    let buffer_addr = buffer as usize;
    // SAFETY: `buffer` holds `size` bytes from `allocator`, which the deleter returns to it
    // once the buffer and its shares are gone; the deleter runs once, so `deallocate` gets
    // the pointer and size of that `allocate(size)` exactly once.
    unsafe {
        TemporaryBuffer::with_deleter(
            buffer,
            size,
            make_deleter_with(Deleter::new(), move || {
                allocator.deallocate(buffer_addr as *mut u8, size)
            }),
        )
    }
}
