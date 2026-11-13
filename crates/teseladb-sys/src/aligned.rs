//! Memory aligned to a page, for `O_DIRECT` and for the buffers registered in io_uring (012
//! FR-003): the kernel reads and writes them straight to the device, which needs the address, the
//! length and the file offset aligned to the logical block.
//!
//! [`allocate_huge`] puts a set of buffers on one mapping of 2 MiB pages (`MAP_HUGETLB`, 012
//! FR-032) when the system has enough of them reserved, and on normal pages otherwise: the
//! kernel then walks one page table entry per 2 MiB of the buffers it reads and writes.

use std::alloc::{self, Layout};
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::Arc;

/// Alignment of every [`AlignedBuf`]: the 4 KiB of a page and of the engine's blocks.
pub const ALIGN: usize = 4096;

/// Bytes of a huge page.
pub const HUGE_PAGE: usize = 2 << 20;

/// A zeroed buffer whose address and length are multiples of [`ALIGN`].
pub struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
    /// The mapping of huge pages the buffer is a part of; `None`: its own heap allocation.
    huge: Option<Arc<HugeMap>>,
}

/// A mapping of huge pages, unmapped when its last buffer goes.
struct HugeMap {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is plain memory owned by this value; the buffers carved from it never
// overlap, so each is accessed through its own `AlignedBuf` only.
unsafe impl Send for HugeMap {}
// SAFETY: as above; `HugeMap` itself is never read or written through a shared reference.
unsafe impl Sync for HugeMap {}

impl Drop for HugeMap {
    fn drop(&mut self) {
        crate::os::unmap(self.ptr, self.len);
    }
}

/// `count` buffers of `len` bytes (rounded up to [`ALIGN`]), carved from one mapping of huge
/// pages when the system has them reserved and of normal heap memory otherwise (012 FR-032).
pub fn allocate_huge(count: usize, len: usize) -> Vec<AlignedBuf> {
    let len = len.max(1).next_multiple_of(ALIGN);
    let total = (count * len).next_multiple_of(HUGE_PAGE);
    // Without enough reserved pages the mapping would fail: no call is made then.
    let enough = count > 0 && crate::os::huge_pages_free() * HUGE_PAGE >= total;
    let Some(ptr) = enough.then(|| crate::os::map_huge(total)).flatten() else {
        return (0..count).map(|_| AlignedBuf::new(len)).collect();
    };
    let map = Arc::new(HugeMap { ptr, len: total });
    (0..count)
        .map(|i| {
            // SAFETY: `i * len + len <= count * len <= total`, so the offset stays inside the
            // mapping, and the ranges of two buffers do not overlap.
            let at = unsafe { NonNull::new_unchecked(ptr.as_ptr().add(i * len)) };
            AlignedBuf { ptr: at, len, huge: Some(Arc::clone(&map)) }
        })
        .collect()
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
        AlignedBuf { ptr, len, huge: None }
    }

    /// Whether the buffer lives on huge pages.
    pub fn is_huge(&self) -> bool {
        self.huge.is_some()
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
        if self.huge.is_some() {
            // Part of a mapping, unmapped with its last buffer.
            return;
        }
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

    #[test]
    fn huge_buffers_are_aligned_apart_and_zeroed_on_either_kind_of_page() {
        // 012 FR-032: with no huge pages reserved, the buffers come from the heap.
        let mut bufs = allocate_huge(5, 256 << 10);
        assert_eq!(bufs.len(), 5);
        let huge = bufs[0].is_huge();
        assert!(bufs.iter().all(|b| b.is_huge() == huge));
        assert_eq!(
            huge,
            crate::os::huge_pages_free() * HUGE_PAGE
                >= (5 * (256usize << 10)).next_multiple_of(HUGE_PAGE)
        );
        for (i, b) in bufs.iter_mut().enumerate() {
            assert_eq!(b.addr() as usize % ALIGN, 0);
            assert_eq!(b.len(), 256 << 10);
            assert!(b.iter().all(|&x| x == 0));
            b.fill(i as u8 + 1);
        }
        for (i, b) in bufs.iter().enumerate() {
            assert!(b.iter().all(|&x| x == i as u8 + 1), "buffer {i} was overwritten");
        }
        drop(bufs);
        assert!(allocate_huge(0, 4096).is_empty());
    }
}
