//! An MD5 digest allocates nothing: the whole blocks are read from the input and the padded tail
//! is built on the stack (a copy of the input was allocated, and grown, for every digest; the
//! node takes one per retryable write). The digests themselves are checked by `tests/md5.rs`
//! (RFC 1321 and every length across the block boundaries).
// The counting allocator implements `GlobalAlloc`, an unsafe trait.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use teseladb_hash::md5;

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: every call forwards to the system allocator with the same arguments; the thread-local
// counter has a constant initializer and no destructor, so counting never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.with(|c| c.set(c.get() + 1));
        // SAFETY: forwarded as received.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded as received.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.with(|c| c.set(c.get() + 1));
        // SAFETY: forwarded as received.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

#[test]
fn a_digest_allocates_nothing() {
    let data: Vec<u8> = (0..300u32).map(|i| (i * 7 % 251) as u8).collect();
    for len in [0, 1, 28, 55, 56, 57, 63, 64, 65, 119, 120, 127, 128, 129, 300] {
        let before = ALLOCATIONS.with(Cell::get);
        let digest = md5(&data[..len]);
        let after = ALLOCATIONS.with(Cell::get);
        assert_eq!(after - before, 0, "{len} bytes");
        std::hint::black_box(digest);
    }
}
