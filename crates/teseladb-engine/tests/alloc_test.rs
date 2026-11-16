//! Tests of the engine allocator through its C and object allocation entry points: zero
//! sizes, alignment, sized deallocation, cross-shard frees and reallocs, alien threads,
//! allocation failure, diagnostics and heap profiling samples. The library installs its
//! allocator when `all(target_os = "linux", not(engine_default_allocator))`. The C functions
//! are called by their C names, which the engine allocator exports, and the object
//! allocation entry points through `teseladb::engine::core::memory`.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::ffi::c_void;

use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::core::memory;
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::testing::perf_tests::perf_tests;
use teseladb::engine_test_case;

// The engine allocator is the global allocator of every program that links the crate,
// so the test binary's own Rust allocations go through it too.

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
    fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void;
    fn aligned_alloc(alignment: usize, size: usize) -> *mut c_void;
}

/// The object allocation entry points: the engine allocator's when the library installs it,
/// and otherwise a default over `malloc` that allocates one byte for a zero size.
#[cfg(all(target_os = "linux", not(engine_default_allocator)))]
use teseladb::engine::core::memory::{
    operator_delete, operator_delete_array, operator_delete_sized, operator_new, operator_new_array,
};

#[cfg(not(all(target_os = "linux", not(engine_default_allocator))))]
fn operator_new(size: usize) -> *mut c_void {
    // SAFETY: malloc has no preconditions.
    let p = unsafe { malloc(size.max(1)) };
    assert!(!p.is_null(), "allocation failed");
    p
}

#[cfg(not(all(target_os = "linux", not(engine_default_allocator))))]
fn operator_new_array(size: usize) -> *mut c_void {
    operator_new(size)
}

#[cfg(not(all(target_os = "linux", not(engine_default_allocator))))]
fn operator_delete(ptr: *mut c_void) {
    // SAFETY: `ptr` comes from `operator_new` (malloc) and is freed once.
    unsafe { free(ptr) };
}

#[cfg(not(all(target_os = "linux", not(engine_default_allocator))))]
fn operator_delete_array(ptr: *mut c_void) {
    operator_delete(ptr);
}

#[cfg(not(all(target_os = "linux", not(engine_default_allocator))))]
fn operator_delete_sized(ptr: *mut c_void, _size: usize) {
    operator_delete(ptr);
}

engine_test_case!(alloc_almost_all_and_realloc_it_with_a_smaller_size, {
    #[cfg(all(target_os = "linux", not(engine_default_allocator)))]
    {
        let all = memory::stats().total_memory();
        let reserve = (0.02 * all as f64) as usize;
        let mut to_alloc = all - (reserve + (10 << 20));
        let orig_to_alloc = to_alloc;
        // SAFETY: malloc has no preconditions.
        let mut obj = unsafe { malloc(to_alloc) };
        while obj.is_null() {
            to_alloc = (to_alloc as f64 * 0.9) as usize;
            // SAFETY: as above.
            obj = unsafe { malloc(to_alloc) };
        }
        assert!(to_alloc > orig_to_alloc / 4);
        assert!(!obj.is_null());
        // SAFETY: `obj` is a live block of `to_alloc` bytes.
        let obj2 = unsafe { realloc(obj, to_alloc - (1 << 20)) };
        assert!(obj == obj2);
        // SAFETY: `obj2` is live.
        unsafe { free(obj2) };
    }
    make_ready_future(())
});

engine_test_case!(malloc_0_and_free_it, {
    #[cfg(all(target_os = "linux", not(engine_default_allocator)))]
    {
        // SAFETY: malloc has no preconditions.
        let obj = unsafe { malloc(0) };
        assert!(!obj.is_null());
        // SAFETY: `obj` is live.
        unsafe { free(obj) };
    }
    make_ready_future(())
});

engine_test_case!(new_0, {
    {
        // new must always return a non-null pointer, even for 0 size
        let obj = operator_new(0);
        assert!(!obj.is_null());
        operator_delete(obj);
    }
    {
        // same test but with a zero length array
        let obj = operator_new_array(0);
        assert!(!obj.is_null());
        operator_delete_array(obj);
    }
    make_ready_future(())
});

engine_test_case!(test_live_objects_counter_with_cross_cpu_free, {
    Smp::submit_to(1, || {
        let mut ret: Vec<Option<Box<bool>>> = (0..1_000_000).map(|_| None).collect();
        for o in ret.iter_mut() {
            *o = Some(Box::new(false));
        }
        make_ready_future(ret)
    })
    .then(|mut vec: Vec<Option<Box<bool>>>| {
        vec.clear(); // cause cross-cpu free
        assert!(memory::stats().live_objects() < usize::MAX / 2);
    })
});

engine_test_case!(test_aligned_alloc, {
    let mut align = std::mem::size_of::<*const c_void>();
    while align <= 65536 {
        let mut size = align;
        while size <= align * 2 {
            // SAFETY: `align` is a power of two and `size` a multiple of it.
            let p = unsafe { aligned_alloc(align, size) };
            assert!(!p.is_null());
            assert!((p as usize).is_multiple_of(align));
            // SAFETY: `p` is a live block of `size` bytes.
            unsafe {
                std::ptr::write_bytes(p as *mut u8, 0, size);
                free(p);
            }
            size <<= 1;
        }
        align <<= 1;
    }
    make_ready_future(())
});

// Sized deallocation is always available.
engine_test_case!(test_sized_delete, {
    for size in 0..=65536usize {
        let p0 = operator_new(size);
        let p1 = operator_new(size);
        assert!(!p0.is_null());
        assert!(!p1.is_null());
        // SAFETY: both are live blocks of `size` bytes.
        unsafe {
            std::ptr::write_bytes(p0 as *mut u8, 1, size);
            std::ptr::write_bytes(p1 as *mut u8, 2, size);
        }
        perf_tests::do_not_optimize(&p0);
        perf_tests::do_not_optimize(&p1);
        operator_delete_sized(p0, size);
        operator_delete_sized(p1, size);
    }
    make_ready_future(())
});

engine_test_case!(test_temporary_buffer_aligned, {
    let mut align = std::mem::size_of::<*const c_void>();
    while align <= 65536 {
        let mut size = align;
        while size <= align * 2 {
            let mut buf = TemporaryBuffer::<u8>::aligned(align, size);
            let p = buf.get_write() as *mut c_void;
            assert!(!p.is_null());
            assert!((p as usize).is_multiple_of(align));
            // SAFETY: the buffer owns `size` writable bytes at `p`.
            unsafe { std::ptr::write_bytes(p as *mut u8, 0, size) };
            size <<= 1;
        }
        align <<= 1;
    }
    make_ready_future(())
});

engine_test_case!(test_memory_diagnostics, {
    let report = memory::generate_memory_diagnostics_report();
    #[cfg(not(all(target_os = "linux", not(engine_default_allocator))))]
    {
        assert!(report.size() == 0); // empty report with default allocator
    }
    #[cfg(all(target_os = "linux", not(engine_default_allocator)))]
    {
        // since the output format is unstructured text, not much
        // to do except test that we get a non-empty string
        assert!(report.size() > 0);
        // useful while debugging diagnostics
        // print!("--------------------\n{}--------------------", report);
    }
    make_ready_future(())
});

engine_test_case!(test_cross_thread_realloc, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        // Tests that realloc seems to do the right thing with various sizes of
        // buffer, including cases where the initial allocation is on another
        // shard.
        // Needs at least 2 shards to usefully test the cross-shard aspect but
        // still passes when only 1 shard is used.
        let do_xshard_realloc =
            |cross_shard: bool,
             initial_size: usize,
             realloc_size: usize|
             -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
                let ctx = format!(
                    "cross_shard={}, initial={}, realloc_size={}",
                    cross_shard, initial_size, realloc_size
                );
                let other_shard = (this_shard_id() + cross_shard as u32) % Smp::count();
                // SAFETY: malloc has no preconditions.
                let p = unsafe { malloc(initial_size) } as *mut u8;
                // write some sentinels and check them after realloc
                // x start of region
                // y end of realloc'd region (if it falls within the initial size)
                // z end of initial region
                if initial_size > 0 {
                    // SAFETY: `p` is a live block of `initial_size` bytes.
                    unsafe {
                        *p = b'x';
                        *p.add(initial_size - 1) = b'z';
                        if realloc_size > 0 && realloc_size <= initial_size {
                            *p.add(realloc_size - 1) = b'y';
                        }
                    }
                }
                // The pointer crosses shards as an address.
                let addr = p as usize;
                Smp::submit_to(other_shard, move || {
                    // SAFETY: the block is live and no one else uses it.
                    let p2 = unsafe { realloc(addr as *mut c_void, realloc_size) } as *mut u8;
                    if initial_size > 0 && realloc_size > 0 {
                        // SAFETY: `p2` is a live block of `realloc_size` bytes holding the old contents.
                        unsafe {
                            assert_eq!(*p2, b'x', "{}", ctx);
                            if realloc_size <= initial_size {
                                assert_eq!(*p2.add(realloc_size - 1), b'y', "{}", ctx);
                            }
                            if realloc_size > initial_size {
                                assert_eq!(*p2.add(initial_size - 1), b'z', "{}", ctx);
                            }
                        }
                    }
                    // SAFETY: `p2` is live (or null).
                    unsafe { free(p2 as *mut c_void) };
                })
                .get()
            };

        for cross_shard in [false, true] {
            do_xshard_realloc(cross_shard, 0, 0)?;
            do_xshard_realloc(cross_shard, 0, 1)?;
            do_xshard_realloc(cross_shard, 1, 0)?;
            do_xshard_realloc(cross_shard, 50, 100)?;
            do_xshard_realloc(cross_shard, 100, 50)?;
            do_xshard_realloc(cross_shard, 100000, 500000)?;
            do_xshard_realloc(cross_shard, 500000, 100000)?;
        }
        Ok(())
    })
});

#[cfg(all(target_os = "linux", not(engine_default_allocator)))]
mod engine_allocator_tests {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicPtr, Ordering};

    use super::{aligned_alloc, free, malloc, realloc};
    use teseladb::engine::core::future::make_ready_future;
    use teseladb::engine::core::memory;
    use teseladb::engine::core::sstring::Sstring;
    use teseladb::engine::util::log::LogLevel;
    use teseladb::engine_test_case;

    /// The allocation statistics of a thread, and the block it allocated.
    struct ThreadAllocInfo {
        before: memory::Statistics,
        after: memory::Statistics,
        ptr: usize,
    }

    /// Runs `f` on a new (alien) thread and waits for its result.
    fn std_async<R: Send + 'static, F: FnOnce() -> R + Send + 'static>(f: F) -> R {
        match std::thread::spawn(f).join() {
            Ok(r) => r,
            Err(e) => std::panic::resume_unwind(e),
        }
    }

    fn run_with_stats(f: fn() -> *mut c_void) -> ThreadAllocInfo {
        std_async(move || {
            let before = memory::stats();
            let ptr = f() as usize;
            let after = memory::stats();
            ThreadAllocInfo { before, after, ptr }
        })
    }

    fn test_allocation_function(f: fn() -> *mut c_void) {
        // alien alloc and free
        let alloc_info = run_with_stats(f);
        let p = alloc_info.ptr;
        let free_info = std_async(move || {
            let before = memory::stats();
            // SAFETY: `p` is the live block allocated by `f`.
            unsafe { free(p as *mut c_void) };
            let after = memory::stats();
            ThreadAllocInfo { before, after, ptr: 0 }
        });
        // there were mallocs
        assert!(alloc_info.after.foreign_mallocs() - alloc_info.before.foreign_mallocs() > 0);
        // mallocs balanced with frees
        assert!(
            alloc_info.after.foreign_mallocs() - alloc_info.before.foreign_mallocs()
                == free_info.after.foreign_frees() - free_info.before.foreign_frees()
        );

        // alien alloc reactor free
        let info = run_with_stats(f);
        let before_cross_frees = memory::stats().foreign_cross_frees();
        // SAFETY: `info.ptr` is the live block allocated by `f`.
        unsafe { free(info.ptr as *mut c_void) };
        assert!(memory::stats().foreign_cross_frees() - before_cross_frees == 1);

        // reactor alloc, alien free
        let p = f() as usize;
        let alien_cross_frees = std_async(move || {
            let frees_before = memory::stats().cross_cpu_frees();
            // SAFETY: `p` is the live block allocated by `f`.
            unsafe { free(p as *mut c_void) };
            memory::stats().cross_cpu_frees() - frees_before
        });
        assert!(alien_cross_frees == 1);
    }

    // The 12-byte size class lays its objects every 12 bytes, so every other one is only
    // 4-aligned. Freeing them from another shard links them into the owner's cross-shard list
    // through the objects themselves, and the owner's drain walks those links back.
    engine_test_case!(test_cross_cpu_free_of_objects_not_pointer_aligned, {
        use teseladb::engine::core::smp::Smp;
        const OBJECT_SIZE: usize = 12;
        const COUNT: usize = 64;
        if Smp::count() < 2 {
            return make_ready_future(());
        }
        Smp::submit_to(1, || {
            // SAFETY: malloc has no preconditions.
            let addrs: Vec<usize> =
                (0..COUNT).map(|_| unsafe { malloc(OBJECT_SIZE) } as usize).collect();
            make_ready_future(addrs)
        })
        .then(|addrs: Vec<usize>| {
            assert!(addrs.iter().any(|a| a % 8 != 0), "every object is pointer-aligned");
            let before = memory::stats().cross_cpu_frees();
            for &a in &addrs {
                // SAFETY: a live block of shard 1, freed once.
                unsafe { free(a as *mut c_void) };
            }
            assert_eq!(memory::stats().cross_cpu_frees() - before, COUNT as u64);
            Smp::submit_to(1, move || {
                // The reactor's poller may have drained the list already.
                memory::drain_cross_cpu_freelist();
                // The drained objects are back in their pool, which hands them out again.
                // SAFETY: malloc has no preconditions.
                let again: Vec<usize> =
                    (0..COUNT).map(|_| unsafe { malloc(OBJECT_SIZE) } as usize).collect();
                let reused = again.iter().filter(|a| addrs.contains(a)).count();
                for &a in &again {
                    // SAFETY: a live block of this shard, freed once.
                    unsafe { free(a as *mut c_void) };
                }
                assert!(reused > COUNT / 2, "only {reused} of {COUNT} objects came back");
                make_ready_future(())
            })
        })
    });

    engine_test_case!(test_foreign_function_use_glibc_malloc, {
        // SAFETY: the allocation functions have no preconditions for these arguments.
        test_allocation_function(|| unsafe { malloc(1) });
        // SAFETY: realloc of a null pointer is a plain allocation; the block is freed by test_allocation_function.
        test_allocation_function(|| unsafe { realloc(std::ptr::null_mut(), 10) });
        // SAFETY: `p` comes from malloc and is passed to realloc once; the result is freed by test_allocation_function.
        test_allocation_function(|| unsafe {
            let p = malloc(1);
            realloc(p, 1000)
        });
        // SAFETY: 4 is a power of two and 1024 a multiple of it, as aligned_alloc requires; the block is freed by test_allocation_function.
        test_allocation_function(|| unsafe { aligned_alloc(4, 1024) });
        make_ready_future(())
    });

    unsafe extern "C" {
        fn posix_memalign(memptr: *mut *mut c_void, alignment: usize, size: usize) -> i32;
        fn memalign(alignment: usize, size: usize) -> *mut c_void;
    }

    /// Alignments from one above the plain allocation's guarantee up to several pages.
    const SMALL_SIZE_ALIGNMENTS: [usize; 10] =
        [32, 64, 128, 256, 512, 1024, 4096, 8192, 16384, 65536];

    /// Sizes smaller than `align`, the case where a block aligned only to its own rounded
    /// size is under-aligned: one byte, the smallest class, sizes just above a power of two
    /// and just below `align`.
    fn sizes_below(align: usize) -> Vec<usize> {
        let mut sizes = vec![1, 8, 9, 12, align / 2 - 1, align / 2 + 1, align - 1];
        sizes.retain(|&s| s >= 1 && s < align);
        sizes.sort_unstable();
        sizes.dedup();
        sizes
    }

    /// Checks `alloc(align, size)` for every alignment and every size below it: the block is
    /// not null, aligned to `align`, writable over `size` bytes, and its usable size holds
    /// `align` bytes. `release(ptr, align, size)` frees it.
    fn check_aligned_entry_point(
        name: &str,
        alloc: impl Fn(usize, usize) -> *mut c_void,
        release: impl Fn(*mut c_void, usize, usize),
    ) {
        for align in SMALL_SIZE_ALIGNMENTS {
            for size in sizes_below(align) {
                let p = alloc(align, size);
                assert!(!p.is_null(), "{name}({align}, {size}) returned null");
                assert!(
                    (p as usize).is_multiple_of(align),
                    "{name}({align}, {size}) returned {p:p}, not aligned to {align}"
                );
                assert!(
                    memory::object_size(p) >= align,
                    "{name}({align}, {size}): block smaller than {align}"
                );
                // SAFETY: `p` is a live block of at least `size` bytes.
                unsafe { std::ptr::write_bytes(p as *mut u8, 0xa5, size) };
                release(p, align, size);
            }
        }
    }

    engine_test_case!(test_aligned_entry_points_with_size_below_alignment, {
        check_aligned_entry_point(
            "posix_memalign",
            |align, size| {
                let mut p = std::ptr::null_mut();
                // SAFETY: `align` is a power of two at least the size of a
                // pointer, `p` is a local out-pointer, and each block is freed once.
                let r = unsafe { posix_memalign(&mut p, align, size) };
                assert_eq!(r, 0, "posix_memalign({align}, {size}) failed");
                p
            },
            // SAFETY: (release closure) `p` is the live block posix_memalign just returned, freed here once.
            |p, _, _| unsafe { free(p) },
        );
        // SAFETY: `align` is a power of two and the allocator accepts any size; the block is freed once by the release closure.
        check_aligned_entry_point(
            "aligned_alloc",
            |align, size| unsafe { aligned_alloc(align, size) },
            // SAFETY: `p` is the live block the allocation closure just returned, freed here once.
            |p, _, _| unsafe { free(p) },
        );
        // SAFETY: `align` is a power of two; the block is freed once by the release closure.
        check_aligned_entry_point(
            "memalign",
            |align, size| unsafe { memalign(align, size) },
            // SAFETY: `p` is the live block the allocation closure just returned, freed here once.
            |p, _, _| unsafe { free(p) },
        );
        check_aligned_entry_point(
            "operator_new_aligned",
            |align, size| memory::operator_new_aligned(size, align),
            |p, align, size| memory::operator_delete_sized_aligned(p, size, align),
        );
        check_aligned_entry_point(
            "operator_new_array_aligned",
            |align, size| memory::operator_new_array_aligned(size, align),
            |p, align, _| memory::operator_delete_array_aligned(p, align),
        );
        check_aligned_entry_point(
            "operator_new_aligned_nothrow",
            |align, size| memory::operator_new_aligned_nothrow(size, align, std::ptr::null()),
            |p, align, _| memory::operator_delete_aligned_nothrow(p, align, std::ptr::null()),
        );
        check_aligned_entry_point(
            "operator_new_array_aligned_nothrow",
            |align, size| memory::operator_new_array_aligned_nothrow(size, align, std::ptr::null()),
            |p, align, size| memory::operator_delete_array_sized_aligned(p, size, align),
        );
        check_aligned_entry_point(
            "allocate_aligned",
            memory::allocate_aligned,
            memory::free_aligned,
        );
        make_ready_future(())
    });

    // So the compiler won't optimize the call to realloc(nullptr, size)
    // and call malloc directly.
    static TEST_NULLPTR: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

    engine_test_case!(test_realloc_nullptr, {
        // SAFETY: realloc of a null pointer is malloc; the blocks are freed below.
        unsafe {
            let mut p0 = realloc(TEST_NULLPTR.load(Ordering::Relaxed), 8);
            assert!(!p0.is_null());
            assert_eq!(realloc(p0, 0), std::ptr::null_mut());

            p0 = realloc(TEST_NULLPTR.load(Ordering::Relaxed), 0);
            assert!(!p0.is_null());
            let p1 = malloc(0);
            assert!(!p1.is_null());
            free(p0);
            free(p1);
        }
        make_ready_future(())
    });

    engine_test_case!(test_enable_abort_on_oom, {
        let saved = memory::is_abort_on_allocation_failure();

        memory::set_abort_on_allocation_failure(false);
        assert!(!memory::is_abort_on_allocation_failure());

        memory::set_abort_on_allocation_failure(true);
        assert!(memory::is_abort_on_allocation_failure());

        memory::set_abort_on_allocation_failure(saved);

        make_ready_future(())
    });

    /// Where the allocations are stored so that they are not optimized away.
    static SINK: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

    engine_test_case!(test_bad_alloc_throws, {
        // test that a large allocation fails
        let mut stats = memory::stats();

        // this allocation cannot be satisfied (at least when the engine
        // allocator is used, which it is for this test)
        let size = stats.total_memory() * 2;

        let failed_allocs = |stats: &memory::Statistics| {
            memory::stats().failed_allocations() - stats.failed_allocations()
        };

        // A failing object allocation would abort, so
        // the failure is observed through the non-aborting entry points, which share its path and
        // return null.

        // test that new fails
        stats = memory::stats();
        let p = memory::operator_new_nothrow(size);
        SINK.store(p, Ordering::Relaxed);
        assert!(p.is_null(), "the allocation should have failed");
        assert_eq!(failed_allocs(&stats), 1);

        // test that new[] fails
        stats = memory::stats();
        let p = memory::operator_new_array_nothrow(size);
        SINK.store(p, Ordering::Relaxed);
        assert!(p.is_null(), "the allocation should have failed");
        assert_eq!(failed_allocs(&stats), 1);

        // test that huge malloc returns null
        stats = memory::stats();
        // SAFETY: malloc has no preconditions.
        assert_eq!(unsafe { malloc(size) }, std::ptr::null_mut());
        assert_eq!(failed_allocs(&stats), 1);

        // test that huge realloc on nullptr returns null
        stats = memory::stats();
        // SAFETY: realloc of null is malloc.
        assert_eq!(unsafe { realloc(std::ptr::null_mut(), size) }, std::ptr::null_mut());
        assert_eq!(failed_allocs(&stats), 1);

        // test that huge realloc on an existing ptr returns null
        stats = memory::stats();
        // SAFETY: `p` is live until freed below; a failed realloc leaves it live.
        unsafe {
            let p = malloc(1);
            assert!(!p.is_null());
            let p2 = realloc(p, size);
            assert_eq!(p2, std::ptr::null_mut());
            assert_eq!(failed_allocs(&stats), 1);
            free(if p2.is_null() { p } else { p2 });
        }

        make_ready_future(())
    });

    engine_test_case!(test_diagnostics_failures, {
        // test that an allocation failure is reflected in the diagnostics
        let stats = memory::stats();
        let size = stats.total_memory() * 2; // cannot be satisfied

        // we expect that the failure is immediately reflected in the diagnostics
        // (the allocation above fails and is stored in the sink).
        SINK.store(memory::operator_new_nothrow(size), Ordering::Relaxed);

        let report: Sstring = memory::generate_memory_diagnostics_report();

        // +1 because we caused one additional hard failure from the allocation above
        let expected = format!("Hard failures: {}", stats.failed_allocations() + 1);

        if !report.to_string().contains(&expected) {
            panic!("Did not find expected message: {} in\n{}\n", expected, report);
        }

        make_ready_future(())
    });

    fn check_function_allocation<F: FnOnce()>(name: &str, expected_allocs: u64, f: F) {
        let before = memory::stats();
        f();
        let after = memory::stats();

        assert_eq!(expected_allocs, after.mallocs() - before.mallocs(), "After function: {}", name);
    }

    engine_test_case!(test_diagnostics_allocation, {
        check_function_allocation("empty", 0, || {});

        check_function_allocation("operator new", 1, || {
            // note that many pairs of malloc/free-alikes can just be optimized
            // away, but not the object allocation entry point
            let p = std::hint::black_box(memory::operator_new(1));
            memory::operator_delete(p);
        });

        // The meat of this test. Dump the diagnostics report to the log and ensure it
        // doesn't allocate. Doing it lots is important because it may alloc only occasionally:
        // a real example being the optimized timestamp logging which (used to) make an allocation
        // only once a second.
        check_function_allocation("log_memory_diagnostics_report", 0, || {
            for _ in 0..1000 {
                memory::internal::log_memory_diagnostics_report(LogLevel::Info);
            }
        });

        make_ready_future(())
    });

    #[cfg(engine_heapprof)]
    mod heapprof {
        use super::{free, malloc};
        use teseladb::engine::core::future::make_ready_future;
        use teseladb::engine::core::memory;
        use teseladb::engine_test_case;

        // small wrapper to disincentivize gcc from unrolling the loop
        #[inline(never)]
        fn malloc_wrapper(size: usize) -> *mut u8 {
            // SAFETY: malloc has no preconditions.
            let ret = unsafe { malloc(size) } as *mut u8;
            // SAFETY: `ret` is a live block of at least one byte.
            unsafe { std::ptr::write_volatile(ret, b'c') }; // to prevent compiler from considering this a dead allocation and optimizing it out
            ret
        }

        /// The text form of an allocation site.
        fn site_str(site: &memory::AllocationSite) -> String {
            format!("allocation_site[count: {}, size: {}]", site.count.get(), site.size.get())
        }

        fn free_all(ptrs: &[*mut u8]) {
            for &ptr in ptrs {
                // SAFETY: every pointer is a live block from malloc_wrapper (or null).
                unsafe { free(ptr as *mut std::ffi::c_void) };
            }
        }

        engine_test_case!(test_sampled_profile_collection_small, {
            {
                let stats = memory::sampled_memory_profile();
                assert_eq!(stats.len(), 0);
            }

            let count: usize = 100;
            let mut ptrs: Vec<*mut u8> = vec![std::ptr::null_mut(); count];
            memory::set_heap_profiling_sampling_rate(100);
            for ptr in ptrs.iter_mut().take(count / 2) {
                *ptr = malloc_wrapper(10);
            }
            for ptr in ptrs.iter_mut().skip(count / 2) {
                *ptr = malloc_wrapper(10);
            }

            let get_samples = || {
                let stats0 = memory::sampled_memory_profile();
                let stats1 = memory::sampled_memory_profile();
                // two back-to-back copies of the sample should have the same value
                assert!(stats0 == stats1);

                // check that we get the same value from the raw array iterface
                let mut stats2: Vec<memory::AllocationSite> =
                    vec![memory::AllocationSite::default(); stats0.len()];
                let sz2 = memory::sampled_memory_profile_into(&mut stats2);
                assert_eq!(stats0.len(), sz2);
                assert!(stats0 == stats2);

                // check with +1 size, we expect to still only get size elements
                let mut stats3: Vec<memory::AllocationSite> =
                    vec![memory::AllocationSite::default(); stats0.len() + 1];
                let sz3 = memory::sampled_memory_profile_into(&mut stats3);
                assert_eq!(stats0.len(), sz3);

                stats3.truncate(sz3);
                assert!(stats0 == stats3, "{:?}", stats0.iter().map(site_str).collect::<Vec<_>>());

                stats0
            };

            // NB: the test framework allocates
            memory::set_heap_profiling_sampling_rate(0);
            {
                let stats = get_samples();
                assert_eq!(stats.len(), 2);
                assert_eq!(stats[0].size.get(), stats[0].count.get() * 100);
            }

            memory::set_heap_profiling_sampling_rate(100);
            free_all(&ptrs);
            memory::set_heap_profiling_sampling_rate(0);
            {
                let stats = get_samples();
                assert_eq!(stats.len(), 0);
            }

            make_ready_future(())
        });

        engine_test_case!(test_sampled_profile_collection_large, {
            {
                let stats = memory::sampled_memory_profile();
                assert_eq!(stats.len(), 0);
            }

            let count: usize = 100;
            let mut ptrs: Vec<*mut u8> = vec![std::ptr::null_mut(); count];

            memory::set_heap_profiling_sampling_rate(1000000);

            for ptr in ptrs.iter_mut().take(count / 2) {
                *ptr = malloc_wrapper(100000);
            }
            for ptr in ptrs.iter_mut().skip(count / 2) {
                *ptr = malloc_wrapper(100000);
            }

            // NB: the test framework allocate
            memory::set_heap_profiling_sampling_rate(0);
            {
                let stats = memory::sampled_memory_profile();
                assert_eq!(stats.len(), 2);
                assert_eq!(stats[0].size.get(), stats[0].count.get() * 1000000);
            }

            memory::set_heap_profiling_sampling_rate(1000000);
            free_all(&ptrs);
            memory::set_heap_profiling_sampling_rate(0);
            {
                let stats = memory::sampled_memory_profile();
                // NOTE this is because right now the tracking structure doesn't delete call sites ever
                assert_eq!(stats.len(), 0);
            }

            make_ready_future(())
        });

        engine_test_case!(test_sampled_profile_collection_max_sites, {
            let count: usize = 1010;
            let mut ptrs: Vec<*mut u8> = vec![std::ptr::null_mut(); count];
            memory::set_heap_profiling_sampling_rate(100);
            // Every iteration must be its own call site, so the
            // loop is unrolled by a macro into 1010 distinct malloc calls.
            let mut i = 0usize;
            macro_rules! one {
                () => {{
                    // SAFETY: malloc has no preconditions.
                    let ptr = unsafe { malloc(1000) } as *mut u8;
                    // SAFETY: `ptr` is a live block of 1000 bytes.
                    unsafe { std::ptr::write_volatile(ptr, b'c') }; // to prevent compiler from considering this a dead allocation and optimizing it out
                    ptrs[i] = ptr;
                    i += 1;
                }};
            }
            macro_rules! ten {
                () => {{
                    one!();
                    one!();
                    one!();
                    one!();
                    one!();
                    one!();
                    one!();
                    one!();
                    one!();
                    one!();
                }};
            }
            macro_rules! hundred {
                () => {{
                    ten!();
                    ten!();
                    ten!();
                    ten!();
                    ten!();
                    ten!();
                    ten!();
                    ten!();
                    ten!();
                    ten!();
                }};
            }
            hundred!();
            hundred!();
            hundred!();
            hundred!();
            hundred!();
            hundred!();
            hundred!();
            hundred!();
            hundred!();
            hundred!();
            ten!();
            assert_eq!(i, count);

            memory::set_heap_profiling_sampling_rate(0);
            {
                let stats = memory::sampled_memory_profile();
                assert_eq!(stats.len(), 1000);
            }

            free_all(&ptrs);

            make_ready_future(())
        });

        engine_test_case!(test_change_sample_rate, {
            {
                let stats = memory::sampled_memory_profile();
                assert_eq!(stats.len(), 0);
            }

            let sample_rate: usize = 100;
            let count: usize = 10000;
            let mut ptrs: Vec<*mut u8> = vec![std::ptr::null_mut(); count];

            memory::set_heap_profiling_sampling_rate(sample_rate);

            for ptr in ptrs.iter_mut() {
                *ptr = malloc_wrapper(10);
            }

            // NB: the test framework allocates
            memory::set_heap_profiling_sampling_rate(0);
            let mut last_alloc_size;
            {
                let stats = memory::sampled_memory_profile();
                assert_eq!(stats.len(), 1);
                last_alloc_size = stats[0].size.get();
                assert_eq!(stats[0].size.get(), stats[0].count.get() * sample_rate);
            }

            memory::set_heap_profiling_sampling_rate(sample_rate);

            let mut free_iter = 0usize;

            // free some of the allocations to check size changes
            for _ in 0..count / 4 {
                free_all(&ptrs[free_iter..free_iter + 1]);
                free_iter += 1;
            }

            memory::set_heap_profiling_sampling_rate(0);
            {
                let stats = memory::sampled_memory_profile();
                assert_eq!(stats.len(), 1);
                assert_eq!(stats[0].size.get(), stats[0].count.get() * sample_rate);
                assert_ne!(stats[0].size.get(), last_alloc_size);
                assert!(stats[0].size.get() > 0);
                last_alloc_size = stats[0].size.get();
            }

            // now increase the sampling rate with outstanding allocations from the old rate
            memory::set_heap_profiling_sampling_rate(sample_rate * 100);

            for _ in 0..count / 4 {
                free_all(&ptrs[free_iter..free_iter + 1]);
                free_iter += 1;
            }

            memory::set_heap_profiling_sampling_rate(0);
            {
                let stats = memory::sampled_memory_profile();
                assert_eq!(stats.len(), 1);
                assert!(stats[0].size.get() < last_alloc_size); // should not have underflowed
            }

            memory::set_heap_profiling_sampling_rate(sample_rate);

            // free the rest
            for _ in 0..count / 2 {
                free_all(&ptrs[free_iter..free_iter + 1]);
                free_iter += 1;
            }

            memory::set_heap_profiling_sampling_rate(0);
            {
                let stats = memory::sampled_memory_profile();
                assert_eq!(stats.len(), 0);
            }

            make_ready_future(())
        });
    } // ENGINE_HEAPPROF
} // #ifndef ENGINE_DEFAULT_ALLOCATOR

engine_test_case!(test_large_allocation_warning_off_by_one, {
    #[cfg(all(target_os = "linux", not(engine_default_allocator)))]
    {
        const LARGE_ALLOC_THRESHOLD: usize = 1024 * 1024;
        let _mtg = memory::ScopedLargeAllocationWarningThreshold::new(LARGE_ALLOC_THRESHOLD);
        assert!(memory::get_large_allocation_warning_threshold() == LARGE_ALLOC_THRESHOLD);
        let old_large_allocs_count = memory::stats().large_allocations();
        // SAFETY: malloc has no preconditions.
        let obj = unsafe { malloc(LARGE_ALLOC_THRESHOLD) } as *mut u8;
        // SAFETY: `obj` is a live block of LARGE_ALLOC_THRESHOLD bytes.
        unsafe { std::ptr::write_volatile(obj, b'c') }; // to prevent compiler from considering this a dead allocation and optimizing it out
        // Verify large allocation was detected by allocator.
        assert!(memory::stats().large_allocations() == old_large_allocs_count + 1);
        // SAFETY: `obj` is live.
        unsafe { free(obj as *mut c_void) };
    }
    make_ready_future(())
});
