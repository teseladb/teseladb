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

#[cfg(not(engine_heapprof))]
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

// With heap profiling compiled in, the rate is the shard's: a new non-zero rate is ignored
// while profiling is on, nested guards keep the outer rate, and only the last guard's drop
// turns profiling off (the reference's set_heap_profiling_sampling_rate and
// scoped_heap_profiling).
#[cfg(engine_heapprof)]
engine_thread_test_case!(heap_profiling_rate_and_nested_guards, {
    assert_eq!(get_heap_profiling_sample_rate(), 0);
    set_heap_profiling_sampling_rate(1000);
    assert_eq!(get_heap_profiling_sample_rate(), 1000);
    set_heap_profiling_sampling_rate(2000);
    assert_eq!(get_heap_profiling_sample_rate(), 1000, "a rate change while on is ignored");
    set_heap_profiling_sampling_rate(0);
    assert_eq!(get_heap_profiling_sample_rate(), 0);
    set_heap_profiling_sampling_rate(0);
    assert_eq!(get_heap_profiling_sample_rate(), 0, "turning it off twice is harmless");
    {
        let _outer = ScopedHeapProfiling::new(100);
        assert_eq!(get_heap_profiling_sample_rate(), 100);
        {
            let _inner = ScopedHeapProfiling::new(500);
            assert_eq!(get_heap_profiling_sample_rate(), 100, "the inner rate is ignored");
        }
        assert_eq!(get_heap_profiling_sample_rate(), 100, "the outer guard still holds");
    }
    assert_eq!(get_heap_profiling_sample_rate(), 0);
});

// The array form of the profile copies at most the room it is given.
#[cfg(engine_heapprof)]
engine_thread_test_case!(heap_profile_into_a_short_array, {
    #[inline(never)]
    fn site_a(n: usize) -> *mut c_void {
        // SAFETY: malloc has no preconditions.
        std::hint::black_box(unsafe { malloc(std::hint::black_box(n)) })
    }
    #[inline(never)]
    fn site_b(n: usize) -> *mut c_void {
        // SAFETY: malloc has no preconditions.
        std::hint::black_box(unsafe { malloc(std::hint::black_box(n)) })
    }
    unsafe extern "C" {
        fn free(ptr: *mut c_void);
    }
    // Rate 1: every allocated byte is sampled, so each allocation is.
    set_heap_profiling_sampling_rate(1);
    let a = site_a(64);
    let b = site_b(64);
    set_heap_profiling_sampling_rate(0);
    let all = memory::sampled_memory_profile();
    assert!(all.len() >= 2, "both sites are sampled: {}", all.len());
    let mut one = vec![AllocationSite::default(); 1];
    assert_eq!(memory::sampled_memory_profile_into(&mut one), 1);
    assert!(all.contains(&one[0]));
    let mut none: Vec<AllocationSite> = Vec::new();
    assert_eq!(memory::sampled_memory_profile_into(&mut none), 0);
    set_heap_profiling_sampling_rate(1);
    // SAFETY: both are live blocks from malloc, freed once (while profiling, so their sites
    // are released).
    unsafe {
        free(a);
        free(b);
    }
    set_heap_profiling_sampling_rate(0);
});

// Sampled blocks of every kind keep their site and give it back: an aligned small block, a
// small block written over its whole usable size (which excludes the site's slot), a block of
// a whole-page class (whose site lives in the page structure), a large block shrunk in place
// (its site's size follows), and a large span taken and returned directly. Once all are
// freed, the profile is empty again.
#[cfg(engine_heapprof)]
engine_thread_test_case!(heap_profiling_tracks_every_kind_of_sampled_block, {
    unsafe extern "C" {
        fn free(ptr: *mut c_void);
        fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void;
    }
    const LARGE: usize = 1 << 20;
    const SHRUNK: usize = 100_000;
    // Rate 1: every allocation is sampled.
    set_heap_profiling_sampling_rate(1);
    let aligned = std::hint::black_box(memory::allocate_aligned(64, 48));
    // SAFETY: malloc has no preconditions.
    let small = std::hint::black_box(unsafe { malloc(100) });
    // SAFETY: as above; 8190 bytes leave no room for the site before the page end.
    let whole_page = std::hint::black_box(unsafe { malloc(8190) });
    // SAFETY: as above.
    let large = std::hint::black_box(unsafe { malloc(LARGE) });
    let direct = std::hint::black_box(memory::allocate_large(LARGE, true));
    set_heap_profiling_sampling_rate(0);

    assert!(!aligned.is_null() && (aligned as usize).is_multiple_of(64));
    let usable = memory::object_size(small);
    assert!(usable >= 100, "usable size {usable}");
    assert!(memory::object_size(whole_page) >= 8190);
    // SAFETY: each block is live and at least as large as written.
    unsafe {
        std::ptr::write_bytes(small as *mut u8, 0x5c, usable);
        std::ptr::write_bytes(whole_page as *mut u8, 0x5c, 8190);
    }
    let sites = memory::sampled_memory_profile();
    let total: usize = sites.iter().map(|s| s.count.get()).sum();
    assert!(total >= 5, "{total} sampled blocks");
    let large_sites =
        |sites: &[AllocationSite]| sites.iter().filter(|s| s.size.get() >= LARGE).count();
    let before = large_sites(&sites);
    assert!(before >= 2, "{before} sites of large blocks");
    drop(sites);

    // Shrinking a large block in place moves its site's size with it: 256 pages halve while
    // the half still holds SHRUNK bytes, down to 32 pages.
    // SAFETY: `large` is a live block of this allocator.
    let shrunk = unsafe { realloc(large, SHRUNK) };
    assert_eq!(shrunk, large, "a local large block shrinks in place");
    let sites = memory::sampled_memory_profile();
    assert_eq!(large_sites(&sites), before - 1);
    assert!(sites.iter().any(|s| s.size.get() == 32 * PAGE), "no site of the shrunk block");
    drop(sites);

    set_heap_profiling_sampling_rate(1);
    memory::free_aligned(aligned, 64, 48);
    // SAFETY: each block is live and freed once.
    unsafe {
        free(small);
        free(whole_page);
        free(shrunk);
    }
    memory::free_large(direct);
    set_heap_profiling_sampling_rate(0);
    let left = memory::sampled_memory_profile();
    assert!(left.is_empty(), "{} sites left after the frees", left.len());
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
