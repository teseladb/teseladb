//! An allocator for tests that counts allocations (012 FR-008): a test binary installs it as its
//! global allocator and checks that an operation of the hot path makes a bounded number of them.
//! It also counts the bytes the process holds and the most it held since [`reset_peak`], for the
//! tests of the memory a stage keeps (010 T066).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static HELD: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// The system's allocator, counting each allocation and reallocation of every thread.
#[derive(Debug, Default)]
pub struct CountingAlloc;

/// Allocations and reallocations made so far by the process.
pub fn allocations() -> u64 {
    ALLOCATIONS.load(Ordering::Relaxed)
}

/// The bytes the process holds now.
pub fn held_bytes() -> usize {
    HELD.load(Ordering::Relaxed)
}

/// The most bytes the process held since the last [`reset_peak`].
pub fn peak_bytes() -> usize {
    PEAK.load(Ordering::Relaxed)
}

/// Starts a new peak from the bytes held now; their number.
pub fn reset_peak() -> usize {
    let now = held_bytes();
    PEAK.store(now, Ordering::Relaxed);
    now
}

/// Counts `size` more bytes held when `p` was given.
fn gained(p: *mut u8, size: usize) -> *mut u8 {
    if !p.is_null() {
        let now = HELD.fetch_add(size, Ordering::Relaxed) + size;
        PEAK.fetch_max(now, Ordering::Relaxed);
    }
    p
}

// SAFETY: every call goes to `System` with the same arguments; counting does not allocate.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the caller's contract for `alloc` is `System::alloc`'s.
        gained(unsafe { System.alloc(layout) }, layout.size())
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: as above.
        gained(unsafe { System.alloc_zeroed(layout) }, layout.size())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator, which is `System`.
        unsafe { System.dealloc(ptr, layout) };
        HELD.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: as above.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if p.is_null() {
            return p;
        }
        HELD.fetch_sub(layout.size(), Ordering::Relaxed);
        gained(p, new_size)
    }
}
