//! Tests of the per-shard allocator's statistics and controls: allocation counters, free and
//! minimum free memory, the memory layout, large-allocation warning thresholds, the
//! diagnostics report with an application producer, the allocation-failure settings, the
//! `malloc` family entry points, and cross-shard frees.
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::ffi::c_void;
use std::rc::Rc;

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::memory::{
    self, ScopedLargeAllocationWarningDisable, ScopedLargeAllocationWarningThreshold,
    disable_large_allocation_warning, do_dump_memory_diagnostics, free_memory,
    generate_memory_diagnostics_report, get_large_allocation_warning_threshold, get_memory_layout,
    is_abort_on_allocation_failure, min_free_memory, set_abort_on_allocation_failure,
    set_additional_diagnostics_producer, set_dump_memory_diagnostics_on_alloc_failure_kind_str,
    set_large_allocation_warning_threshold, set_min_free_pages, stats,
};
use teseladb::engine::core::smp::Smp;
use teseladb::engine_thread_test_case;

const PAGE: usize = 4096;

engine_thread_test_case!(statistics_count_allocations_and_pages, {
    let before = stats();
    let blocks: Vec<Box<[u8; 256]>> = (0..100).map(|_| Box::new([1u8; 256])).collect();
    let during = stats();
    assert!(during.mallocs() >= before.mallocs() + 100);
    assert!(during.live_objects() >= 100);
    drop(blocks);
    let after = stats();
    assert!(after.frees() >= during.frees() + 100);
    assert_eq!(after.total_memory(), during.total_memory());
    assert!(after.allocated_memory() <= after.total_memory());
    assert_eq!(after.free_memory() + after.allocated_memory(), after.total_memory());
    let _ = (after.cross_cpu_frees(), after.reclaims(), after.failed_allocations());
    let _ = (after.foreign_mallocs(), after.foreign_frees(), after.foreign_cross_frees());

    // A large allocation takes whole pages from the free memory and gives them back.
    let free0 = free_memory();
    let used0 = stats().allocated_memory();
    let big = std::hint::black_box(vec![7u8; 64 * PAGE * 16]);
    assert!(stats().allocated_memory() >= used0 + big.len(), "the pages come from the pool");
    assert!(free_memory() < free0);
    drop(big);
    assert!(free_memory() + 64 * PAGE >= free0);

    let layout = get_memory_layout().unwrap();
    assert!(layout.start < layout.end);
    let probe = Box::new(0u64);
    let addr = &*probe as *const u64 as usize;
    assert!((layout.start..layout.end).contains(&addr), "the shard allocates from its pool");
});

engine_thread_test_case!(minimum_free_memory_is_adjustable, {
    let old = min_free_memory() / PAGE;
    set_min_free_pages(old + 16).unwrap();
    assert_eq!(min_free_memory(), (old + 16) * PAGE);
    set_min_free_pages(old).unwrap();
    assert_eq!(min_free_memory(), old * PAGE);
    // More pages than the shard has cannot be kept free.
    assert!(set_min_free_pages(usize::MAX / PAGE).is_err());
});

engine_thread_test_case!(large_allocation_warning_thresholds_nest, {
    let original = get_large_allocation_warning_threshold();
    set_large_allocation_warning_threshold(1 << 20);
    assert_eq!(get_large_allocation_warning_threshold(), 1 << 20);
    {
        let _scoped = ScopedLargeAllocationWarningThreshold::new(1 << 22);
        assert_eq!(get_large_allocation_warning_threshold(), 1 << 22);
        {
            let _off = ScopedLargeAllocationWarningDisable::default();
            assert_eq!(get_large_allocation_warning_threshold(), usize::MAX);
            // A large allocation while disabled does not warn (nor fail).
            drop(vec![0u8; 4 << 20]);
        }
        assert_eq!(get_large_allocation_warning_threshold(), 1 << 22);
    }
    assert_eq!(get_large_allocation_warning_threshold(), 1 << 20);
    // Above the threshold the allocation still succeeds; it is only logged.
    drop(vec![0u8; 2 << 20]);
    disable_large_allocation_warning();
    assert_eq!(get_large_allocation_warning_threshold(), usize::MAX);
    set_large_allocation_warning_threshold(original);
});

engine_thread_test_case!(diagnostics_report_includes_the_producer, {
    let calls = Rc::new(Cell::new(0));
    let c = calls.clone();
    set_additional_diagnostics_producer(Box::new(move |w| {
        c.set(c.get() + 1);
        w("cov producer line\n");
    }));
    let mut out = String::new();
    do_dump_memory_diagnostics(&mut out).unwrap();
    assert!(out.contains("cov producer line"), "{out}");
    assert!(out.contains("Dumping"), "{out}");
    let report = generate_memory_diagnostics_report();
    assert!(report.as_str().contains("cov producer line"));
    assert!(calls.get() >= 2);
    let mut rl = teseladb::engine::util::log::RateLimit::new(std::time::Duration::ZERO);
    memory::dump_memory_diagnostics(teseladb::engine::util::log::LogLevel::Debug, &mut rl);
    memory::log_memory_diagnostics_report_impl(teseladb::engine::util::log::LogLevel::Debug);
    set_additional_diagnostics_producer(Box::new(|_| {}));
});

engine_thread_test_case!(allocation_failure_settings, {
    let was = is_abort_on_allocation_failure();
    set_abort_on_allocation_failure(true);
    assert!(is_abort_on_allocation_failure());
    set_abort_on_allocation_failure(false);
    assert!(!is_abort_on_allocation_failure());
    set_abort_on_allocation_failure(was);
    for kind in ["none", "critical", "all", "bogus"] {
        set_dump_memory_diagnostics_on_alloc_failure_kind_str(kind);
    }
    set_dump_memory_diagnostics_on_alloc_failure_kind_str("critical");
    // A failed allocation is reported as an error, not an abort.
    let huge = std::alloc::Layout::from_size_align(isize::MAX as usize - 4096, 8).unwrap();
    let failed_before = stats().failed_allocations();
    // SAFETY: a non-zero layout; a null result is checked.
    // The pointer is passed to an outside function, so the allocation cannot be elided.
    let p = std::hint::black_box(unsafe { std::alloc::alloc(huge) });
    // SAFETY: a null pointer has no usable size.
    assert_eq!(unsafe { malloc_usable_size(p as *mut c_void) }, 0);
    assert!(p.is_null());
    // SAFETY: as above, through the C entry point.
    let q = std::hint::black_box(unsafe { malloc(isize::MAX as usize - 4096) });
    // SAFETY: as above.
    assert_eq!(unsafe { malloc_usable_size(q) }, 0);
    assert!(q.is_null());
    assert!(stats().failed_allocations() > failed_before);
});

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(p: *mut c_void);
    fn malloc_usable_size(p: *mut c_void) -> usize;
    fn malloc_trim(pad: usize) -> i32;
    fn realloc(p: *mut c_void, size: usize) -> *mut c_void;
}

engine_thread_test_case!(malloc_family_uses_the_shard_allocator, {
    // SAFETY: plain allocation calls, each pointer freed once.
    unsafe {
        let p = malloc(100);
        assert!(!p.is_null());
        assert!(malloc_usable_size(p) >= 100);
        let q = realloc(p, 10_000);
        assert!(malloc_usable_size(q) >= 10_000);
        free(q);
        assert_eq!(malloc_usable_size(std::ptr::null_mut()), 0);
        assert!(malloc_trim(0) >= 0);
    }
});

engine_thread_test_case!(memory_freed_on_another_shard_returns_home, {
    if Smp::count() < 2 {
        return;
    }
    let remote_frees = || Smp::submit_to(1, || Ok::<u64, ExceptionPtr>(stats().cross_cpu_frees()));
    let before = remote_frees().get().unwrap();
    let blocks: Vec<usize> =
        (0..64).map(|i| Box::into_raw(Box::new([i as u8; 64])) as usize).collect();
    Smp::submit_to(1, move || {
        for b in blocks {
            // SAFETY: each pointer came from Box::into_raw above and is freed once.
            drop(unsafe { Box::from_raw(b as *mut [u8; 64]) });
        }
    })
    .get()
    .unwrap();
    // The frees were queued for this shard; draining collects them.
    memory::drain_cross_cpu_freelist();
    // The freeing shard counts them as cross-shard frees.
    let after = remote_frees().get().unwrap();
    assert!(after >= before + 64, "{before} -> {after}");
});
