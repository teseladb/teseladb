//! Memory aligned to a page, for `O_DIRECT` and for the buffers registered in io_uring (012
//! FR-003): the kernel reads and writes them straight to the device, which needs the address, the
//! length and the file offset aligned to the logical block.

use std::alloc::{self, Layout};
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

/// Alignment of every [`AlignedBuf`]: the 4 KiB of a page and of the engine's blocks.
pub const ALIGN: usize = 4096;

/// A zeroed buffer whose address and length are multiples of [`ALIGN`].
pub struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the buffer owns its memory like a `Vec<u8>`; nothing else points to it.
unsafe impl Send for AlignedBuf {}
// SAFETY: shared references only read it, as with `&[u8]`.
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    /// A zeroed buffer of `len` bytes rounded up to [`ALIGN`] (at least one block).
    pub fn new(len: usize) -> AlignedBuf {
        let len = len.max(1).next_multiple_of(ALIGN);
        let layout = Layout::from_size_align(len, ALIGN).expect("aligned buffer layout");
        // SAFETY: `layout` has a non-zero size.
        let raw = unsafe { alloc::alloc_zeroed(layout) };
        let Some(ptr) = NonNull::new(raw) else { alloc::handle_alloc_error(layout) };
        AlignedBuf { ptr, len }
    }

    /// Bytes of the buffer.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Never: a buffer has at least one block.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Address of the first byte, for the kernel.
    pub fn addr(&self) -> u64 {
        self.ptr.as_ptr() as u64
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: `ptr` points to `len` initialized bytes owned by `self`.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as in `deref`, and `&mut self` makes the access exclusive.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.len, ALIGN).expect("aligned buffer layout");
        // SAFETY: allocated in `new` with this same layout.
        unsafe { alloc::dealloc(self.ptr.as_ptr(), layout) };
    }
}

impl std::fmt::Debug for AlignedBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlignedBuf").field("len", &self.len).finish()
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn buffers_are_aligned_zeroed_and_rounded_up() {
        for len in [0, 1, 4096, 4097, 256 << 10] {
            let mut b = AlignedBuf::new(len);
            assert_eq!(b.addr() as usize % ALIGN, 0);
            assert_eq!(b.len() % ALIGN, 0);
            assert!(b.len() >= len.max(1));
            assert!(!b.is_empty());
            assert!(b.iter().all(|&x| x == 0));
            b[0] = 7;
            let last = b.len() - 1;
            b[last] = 9;
            assert_eq!((b[0], b[last]), (7, 9));
            assert!(format!("{b:?}").contains("len"));
        }
    }
}
