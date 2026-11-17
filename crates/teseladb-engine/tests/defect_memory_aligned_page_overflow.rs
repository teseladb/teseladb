#![cfg(all(target_os = "linux", not(engine_default_allocator)))]
//! An aligned allocation whose page count does not fit a span fails (null, `ENOMEM`, an
//! allocation error) and is counted as a failed allocation, as an unaligned one does, instead
//! of truncating the page count and returning a block far smaller than asked for.

use std::alloc::Layout;
use std::ffi::c_void;
use std::hint::black_box;

use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::core::memory::{self, PAGE_SIZE};
use teseladb::engine_test_case;

unsafe extern "C" {
    fn aligned_alloc(alignment: usize, size: usize) -> *mut c_void;
    fn memalign(alignment: usize, size: usize) -> *mut c_void;
    fn posix_memalign(memptr: *mut *mut c_void, alignment: usize, size: usize) -> i32;
}

const ENOMEM: i32 = 12;

fn failed_allocs_since(before: &memory::Statistics) -> u64 {
    memory::stats().failed_allocations() - before.failed_allocations()
}

engine_test_case!(test_aligned_allocation_with_overflowing_page_count_fails, {
    // One page more than a span's page count (u32) can hold: truncated, it would be one page.
    let overflowing_page_count = u64::from(u32::MAX) + 2;
    let size = usize::try_from(overflowing_page_count).expect("64-bit") * PAGE_SIZE;

    let stats = memory::stats();
    // SAFETY: aligned_alloc has no preconditions; a null result is checked.
    // black_box: the compiler may otherwise assume a known allocation function succeeds
    // when its result is only compared with null, and drop the call.
    let p = black_box(unsafe { aligned_alloc(PAGE_SIZE, size) });
    assert!(p.is_null(), "aligned_alloc returned a block for {size} bytes");
    assert_eq!(failed_allocs_since(&stats), 1);

    let stats = memory::stats();
    // SAFETY: as above.
    let p = black_box(unsafe { memalign(PAGE_SIZE, size) });
    assert!(p.is_null(), "memalign returned a block for {size} bytes");
    assert_eq!(failed_allocs_since(&stats), 1);

    let stats = memory::stats();
    let mut p: *mut c_void = std::ptr::null_mut();
    // SAFETY: `p` is a valid out pointer.
    let r = black_box(unsafe { posix_memalign(black_box(&mut p), PAGE_SIZE, size) });
    assert_eq!(r, ENOMEM);
    assert!(p.is_null());
    assert_eq!(failed_allocs_since(&stats), 1);

    let stats = memory::stats();
    let layout = Layout::from_size_align(size, PAGE_SIZE).expect("layout");
    // SAFETY: the layout has a non-zero size; a null result is checked.
    let p = black_box(unsafe { std::alloc::alloc(layout) });
    assert!(p.is_null(), "the aligned Rust allocation returned a block for {size} bytes");
    assert_eq!(failed_allocs_since(&stats), 1);
    make_ready_future(())
});
