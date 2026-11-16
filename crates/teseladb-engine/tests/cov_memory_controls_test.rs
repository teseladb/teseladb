//! Tests of the allocator's smaller controls: the byte allocator over the global allocator,
//! the guard that suspends abort-on-failure, the backtrace guard, allocation sites, the heap
//! profiling interface of a build without it, page-size arithmetic, and large allocations
//! taken and returned directly.
#![cfg(target_os = "linux")]

use std::collections::hash_map::DefaultHasher;
use std::ffi::c_void;
use std::hash::{Hash, Hasher};

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::memory::{
    self, AllocationSite, DisableAbortOnAllocFailureTemporarily, DisableBacktraceTemporarily,
    MallocAllocator, ScopedHeapProfiling, get_heap_profiling_sample_rate,
    is_abort_on_allocation_failure, set_abort_on_allocation_failure,
    set_heap_profiling_sampling_rate, stats,
};
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine_test_case;

/// A test case whose body runs in an engine thread, where it can wait on futures with `get()`.
macro_rules! thread_case {
    ($name:ident, $body:block) => {
        engine_test_case!($name, {
            thread_async(move || {
                // A failed assertion is caught on the thread's own stack and fails the test.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || $body))
                    .map_err(ExceptionPtr::from_panic)
            })
        });
    };
}

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

thread_case!(byte_allocator_and_guards, {
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

    // With abort-on-failure on, a guard keeps a failed allocation from aborting.
    let was = is_abort_on_allocation_failure();
    #[allow(deprecated)]
    memory::enable_abort_on_allocation_failure();
    assert!(is_abort_on_allocation_failure());
    let failed = stats().failed_allocations();
    {
        let _outer = DisableAbortOnAllocFailureTemporarily::default();
        let _inner = DisableAbortOnAllocFailureTemporarily::new();
        // SAFETY: a plain allocation call; the null result is checked.
        let q = std::hint::black_box(unsafe { malloc(HUGE) });
        assert!(q.is_null());
    }
    set_abort_on_allocation_failure(was);
    assert!(stats().failed_allocations() > failed);
    let _no_backtrace = DisableBacktraceTemporarily::default();
});

thread_case!(allocation_sites_compare_by_backtrace, {
    let a = AllocationSite::default();
    let b = a.clone();
    b.count.set(10);
    b.size.set(1000);
    assert!(a == b, "the counters do not take part");
    assert_eq!(hash_of(&a), hash_of(&b));
    assert!(a.next.get().is_null() && a.prev.get().is_null());
});

thread_case!(heap_profiling_is_absent_from_this_build, {
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

thread_case!(page_arithmetic_and_large_allocations, {
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
