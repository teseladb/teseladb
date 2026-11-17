//! A shard gives back to the system the memory a peak touched and no longer uses
//! (`memory::release_free_memory`): a free span of at least a huge page that stayed free from
//! one call to the next leaves the resident set, and the next allocation of it works. A block
//! in use, or a span taken again between two calls, keeps its pages. Residency is read with
//! mincore(2).

#![cfg(target_os = "linux")]
// This file calls the allocator's C entry points and mincore(2) directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::ffi::c_void;

use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::core::memory;
use teseladb::engine_test_case;

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
    fn mincore(addr: *mut c_void, length: usize, vec: *mut u8) -> i32;
}

const PAGE: usize = 4096;
const MIB: usize = 1 << 20;
const SIZE: usize = 64 * MIB;

/// Resident pages of `[start, start + len)`, counted in `vec` (allocated before, so that
/// counting allocates nothing between two calls of the release).
fn resident(start: *mut u8, len: usize, vec: &mut [u8]) -> usize {
    let pages = len.div_ceil(PAGE);
    // SAFETY: the range lies in the shard's mapping, page aligned, and `vec` has a byte per page.
    let r = unsafe { mincore(start.cast(), len, vec.as_mut_ptr()) };
    assert_eq!(r, 0, "mincore failed");
    vec[..pages].iter().filter(|&&b| b & 1 != 0).count()
}

/// `size` bytes, every page written.
fn touched(size: usize) -> *mut u8 {
    // SAFETY: malloc has no preconditions.
    let p = unsafe { malloc(size) } as *mut u8;
    assert!(!p.is_null(), "malloc({size}) failed");
    // SAFETY: `p` is a fresh block of `size` bytes.
    unsafe { std::ptr::write_bytes(p, 0x5a, size) };
    p
}

/// Whether the `size` bytes at `p` all read `0x5a`.
fn intact(p: *mut u8, size: usize) -> bool {
    // SAFETY: `p` is a live block of `size` bytes.
    unsafe { std::slice::from_raw_parts(p, size) }.iter().all(|&b| b == 0x5a)
}

engine_test_case!(memory_free_since_the_last_call_goes_back_to_the_system, {
    let mut vec = vec![0u8; SIZE / PAGE];
    let kept = touched(16 * MIB);
    let big = touched(SIZE);
    assert_eq!(resident(big, SIZE, &mut vec), SIZE / PAGE);
    // SAFETY: a live block, freed once.
    unsafe { free(big.cast()) };
    // The first call only notes the free spans: none was free for a whole period yet.
    memory::release_free_memory();
    assert_eq!(resident(big, SIZE, &mut vec), SIZE / PAGE, "nothing goes at the first call");
    let released = memory::release_free_memory();
    assert!(released >= SIZE, "the freed 64 MiB are among the spans given back: {released}");
    assert_eq!(resident(big, SIZE, &mut vec), 0, "the freed block left the resident set");
    // What was given back once is not given back again while it stays free.
    assert_eq!(memory::release_free_memory(), 0);
    // The block in use kept its pages and its bytes.
    assert_eq!(resident(kept, 16 * MIB, &mut vec), 16 * MIB / PAGE);
    assert!(intact(kept, 16 * MIB));
    // The memory given back is allocated again and used as any other.
    let again = touched(SIZE);
    assert!(intact(again, SIZE));
    memory::release_free_memory();
    memory::release_free_memory();
    assert_eq!(resident(again, SIZE, &mut vec), SIZE / PAGE, "a block in use stays");
    assert!(intact(again, SIZE));
    // SAFETY: live blocks, each freed once.
    unsafe {
        free(again.cast());
        free(kept.cast());
    }
    make_ready_future(())
});

engine_test_case!(a_span_taken_again_between_two_calls_keeps_its_pages, {
    let mut vec = vec![0u8; SIZE / PAGE];
    let first = touched(SIZE);
    // SAFETY: a live block, freed once.
    unsafe { free(first.cast()) };
    memory::release_free_memory();
    // Taken and written between the two calls: the second call must not drop what it holds.
    let second = touched(SIZE);
    memory::release_free_memory();
    assert!(intact(second, SIZE));
    assert_eq!(resident(second, SIZE, &mut vec), SIZE / PAGE);
    // SAFETY: a live block, freed once.
    unsafe { free(second.cast()) };
    make_ready_future(())
});
