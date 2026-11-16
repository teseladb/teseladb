//! Tests of the allocator's smaller controls: the byte allocator over the global allocator,
//! the guard that suspends abort-on-failure, the backtrace guard, allocation sites, the heap
//! profiling interface of a build without it, page-size arithmetic, and large allocations
//! taken and returned directly.
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::collections::hash_map::DefaultHasher;
use std::ffi::c_void;
use std::hash::{Hash, Hasher};

use teseladb::engine::core::memory::{
    self, AllocationSite, DisableAbortOnAllocFailureTemporarily, DisableBacktraceTemporarily,
    MallocAllocator, ScopedHeapProfiling, get_heap_profiling_sample_rate,
    is_abort_on_allocation_failure, set_abort_on_allocation_failure,
    set_heap_profiling_sampling_rate, stats,
};
use teseladb::engine_thread_test_case;

const PAGE: usize = 4096;
/// A request no shard can meet.
const HUGE: usize = isize::MAX as usize - 4096;

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
}

fn hash_of(site: &AllocationSite) -> u64 {
    let mut h = DefaultHasher::new();
    site.hash(&mut h);
    h.finish()
}

engine_thread_test_case!(byte_allocator_and_guards, {
    let a = MallocAllocator;
    let empty = a.allocate(0);
    assert!(!empty.is_null(), "a zero-byte request returns a dangling pointer");
    // SAFETY: a zero-byte release does nothing.
    unsafe { a.deallocate(empty, 0) };
    let p = a.allocate(64);
    assert!(!p.is_null());
    // SAFETY: `p` holds 64 bytes and is released once with the same size.
    unsafe {
        std::ptr::write_bytes(p, 7, 64);
        assert_eq!(*p.add(63), 7);
        a.deallocate(p, 64);
    }

    // With abort-on-failure on, a guard keeps a failed allocation from aborting. The system
    // allocator of a `--cfg engine_default_allocator` build never aborts and counts no
    // failure (as with ANOTHERPRODUCT_DEFAULT_ALLOCATOR).
    let engine_allocator = cfg!(not(engine_default_allocator));
    let was = is_abort_on_allocation_failure();
    #[allow(deprecated)]
    memory::enable_abort_on_allocation_failure();
    assert_eq!(is_abort_on_allocation_failure(), engine_allocator);
    let failed = stats().failed_allocations();
    {
        let _outer = DisableAbortOnAllocFailureTemporarily::default();
        let _inner = DisableAbortOnAllocFailureTemporarily::new();
        // SAFETY: a plain allocation call; the null result is checked.
        let q = std::hint::black_box(unsafe { malloc(HUGE) });
        assert!(q.is_null());
    }
    set_abort_on_allocation_failure(was);
    if engine_allocator {
        assert!(stats().failed_allocations() > failed);
    }
    let _no_backtrace = DisableBacktraceTemporarily::default();
});

engine_thread_test_case!(allocation_sites_compare_by_backtrace, {
    let a = AllocationSite::default();
    let b = a.clone();
    b.count.set(10);
    b.size.set(1000);
    assert!(a == b, "the counters do not take part");
    assert_eq!(hash_of(&a), hash_of(&b));
    assert!(a.next.get().is_null() && a.prev.get().is_null());
});

engine_thread_test_case!(heap_profiling_is_absent_from_this_build, {
    set_heap_profiling_sampling_rate(1000);
    {
        let _scope = ScopedHeapProfiling::new(100);
        assert_eq!(get_heap_profiling_sample_rate(), 0);
    }
    assert_eq!(get_heap_profiling_sample_rate(), 0);
    assert!(memory::sampled_memory_profile().is_empty());
    let mut sites = vec![AllocationSite::default(); 4];
    assert_eq!(memory::sampled_memory_profile_into(&mut sites), 0);
    set_heap_profiling_sampling_rate(0);
});

// The page arithmetic and the direct large allocations belong to the engine's own allocator:
// a build with `--cfg engine_default_allocator` (the debug and sanitizer builds, as
// `ANOTHERPRODUCT_DEFAULT_ALLOCATOR`) has none of them.
#[cfg(not(engine_default_allocator))]
engine_thread_test_case!(page_arithmetic_and_large_allocations, {
    assert!(memory::is_page_aligned(0));
    assert!(memory::is_page_aligned(3 * PAGE));
    assert!(!memory::is_page_aligned(PAGE + 1));
    assert_eq!(memory::next_page_aligned(1), PAGE);
    assert_eq!(memory::next_page_aligned(PAGE), PAGE);
    assert_eq!(memory::next_page_aligned(PAGE + 1), 2 * PAGE);

    let free_before = memory::free_memory();
    let big = memory::allocate_large(5 * PAGE, false);
    assert!(!big.is_null());
    assert_eq!(big as usize % PAGE, 0, "large allocations are page aligned");
    assert!(memory::object_size(big) >= 5 * PAGE);
    assert!(memory::free_memory() < free_before);
    memory::free_large(big);
    assert!(memory::free_memory() + PAGE >= free_before);

    let small = memory::allocate(100);
    assert!(memory::object_size(small) >= 100);
    memory::free(small);
});
