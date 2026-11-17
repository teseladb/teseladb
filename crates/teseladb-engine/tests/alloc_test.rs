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

    /// One exchange of the test below: shard 1 allocates COUNT objects of 12 bytes, this
    /// shard frees them, and shard 1 drains its cross-shard list and allocates COUNT objects
    /// again. Returns how many of those are the objects freed here.
    fn cross_cpu_exchange() -> teseladb::engine::core::future::Future<usize> {
        use teseladb::engine::core::smp::Smp;
        const OBJECT_SIZE: usize = 12;
        const COUNT: usize = 64;
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
                // SAFETY: malloc has no preconditions.
                let again: Vec<usize> =
                    (0..COUNT).map(|_| unsafe { malloc(OBJECT_SIZE) } as usize).collect();
                let reused = again.iter().filter(|a| addrs.contains(a)).count();
                for &a in &again {
                    // SAFETY: a live block of this shard, freed once.
                    unsafe { free(a as *mut c_void) };
                }
                make_ready_future(reused)
            })
        })
    }

    // The 12-byte size class lays its objects every 12 bytes, so every other one is only
    // 4-aligned. Freeing them from another shard links them into the owner's cross-shard list
    // through the objects themselves, and the owner's drain walks those links back into the
    // pool, which hands them out again. Not on the first exchange: the pool then holds the
    // objects of a whole new span, and the drained ones, at the head of its free list, take it
    // past its limit, so trimming it gives exactly them back to their span (the reference does
    // the same: none of the 64 comes back on its first exchange, all 64 on the next ones).
    engine_test_case!(test_cross_cpu_free_of_objects_not_pointer_aligned, {
        use teseladb::engine::core::smp::Smp;
        if Smp::count() < 2 {
            return make_ready_future(());
        }
        cross_cpu_exchange().then(|_| cross_cpu_exchange()).then(|reused: usize| {
            assert_eq!(reused, 64, "only {reused} of 64 objects came back");
            make_ready_future(())
        })
    });

    /// Allocates `count` blocks of `size` bytes on this shard, each filled with a byte derived
    /// from its index, and returns their addresses.
    fn alloc_filled(size: usize, count: usize, seed: u8) -> Vec<usize> {
        (0..count)
            .map(|i| {
                // SAFETY: malloc has no preconditions.
                let p = unsafe { malloc(size) } as *mut u8;
                assert!(!p.is_null());
                // SAFETY: `p` is a fresh block of `size` bytes.
                unsafe { std::ptr::write_bytes(p, seed.wrapping_add(i as u8), size) };
                p as usize
            })
            .collect()
    }

    /// Checks that every block still holds the bytes `alloc_filled` wrote and that no two
    /// blocks overlap.
    fn check_filled(addrs: &[usize], size: usize, seed: u8, ctx: &str) {
        let mut sorted = addrs.to_vec();
        sorted.sort_unstable();
        for w in sorted.windows(2) {
            assert!(w[0] + size <= w[1], "{ctx}: blocks {:#x} and {:#x} overlap", w[0], w[1]);
        }
        for (i, &a) in addrs.iter().enumerate() {
            // SAFETY: a live block of `size` bytes.
            let bytes = unsafe { std::slice::from_raw_parts(a as *const u8, size) };
            let want = seed.wrapping_add(i as u8);
            assert!(bytes.iter().all(|&b| b == want), "{ctx}: block {i} at {a:#x} overwritten");
        }
    }

    // The same exchange for several size classes (10, 12 and 14 bytes give objects that are
    // not pointer-aligned), four rounds each, checking every block's contents: shard 1
    // allocates and fills 64 blocks, this shard frees them, shard 1 drains its cross-shard list
    // and allocates and fills 64 again. The blocks must be intact and disjoint, and from the
    // second round on they are exactly the drained ones. The reference, run the same way, gives
    // 64 of 64 in every round except the first round of the 12-byte class (0 of 64, the trim
    // described above).
    engine_test_case!(test_cross_cpu_free_reuse_and_integrity_by_size, {
        use teseladb::engine::core::smp::Smp;
        use teseladb::engine::core::thread::async_ as thread_async;
        const COUNT: usize = 64;
        const ROUNDS: usize = 4;
        if Smp::count() < 2 {
            return make_ready_future(());
        }
        thread_async(|| {
            for size in [10usize, 12, 14, 20, 24, 40] {
                for round in 0..ROUNDS {
                    let ctx = format!("size {size} round {round}");
                    let seed = (size * 31 + round * 7) as u8;
                    let addrs = Smp::submit_to(1, move || {
                        make_ready_future(alloc_filled(size, COUNT, seed))
                    })
                    .get()?;
                    if size < 16 && size % 8 != 0 {
                        assert!(addrs.iter().any(|a| a % 8 != 0), "{ctx}: all pointer-aligned");
                    }
                    check_filled(&addrs, size, seed, &ctx);
                    let before = memory::stats().cross_cpu_frees();
                    for &a in &addrs {
                        // SAFETY: a live block of shard 1, freed once.
                        unsafe { free(a as *mut c_void) };
                    }
                    assert_eq!(memory::stats().cross_cpu_frees() - before, COUNT as u64, "{ctx}");
                    let reused = Smp::submit_to(1, move || {
                        // The reactor's poller may have drained the list already.
                        memory::drain_cross_cpu_freelist();
                        let again = alloc_filled(size, COUNT, !seed);
                        check_filled(&again, size, !seed, &ctx);
                        let reused = again.iter().filter(|a| addrs.contains(a)).count();
                        for &a in &again {
                            // SAFETY: a live block of this shard, freed once.
                            unsafe { free(a as *mut c_void) };
                        }
                        make_ready_future(reused)
                    })
                    .get()?;
                    if round > 0 {
                        assert_eq!(reused, COUNT, "size {size} round {round}: objects came back");
                    }
                }
            }
            Ok(())
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

        /// Fills the slice with `malloc_wrapper(size)` blocks from one call site, which every
        /// expansion of the macro makes a site of its own. The reference keeps its loops whole
        /// with `#pragma nounroll`: an unrolled loop calls from several addresses, and each
        /// becomes an allocation site of its own, and so does a peeled first iteration. Here
        /// the index passes through `black_box` at the start and on every step, so the compiler
        /// knows neither the trip count nor the first iteration, and the loop stays whole.
        macro_rules! fill_from_one_call_site {
            ($ptrs:expr, $size:expr) => {{
                let ptrs: &mut [*mut u8] = $ptrs;
                let mut i = std::hint::black_box(0usize);
                while i < ptrs.len() {
                    ptrs[i] = malloc_wrapper($size);
                    i = std::hint::black_box(i + 1);
                }
            }};
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
            fill_from_one_call_site!(&mut ptrs[..count / 2], 10);
            fill_from_one_call_site!(&mut ptrs[count / 2..], 10);

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

            fill_from_one_call_site!(&mut ptrs[..count / 2], 100000);
            fill_from_one_call_site!(&mut ptrs[count / 2..], 100000);

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

            fill_from_one_call_site!(&mut ptrs[..], 10);

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

    // Every allocation entry point that consults the allocation failure injector fails when
    // it fires, the way the reference's `try_trigger_error_injector()` callers do: the
    // C entry points and the non-throwing object allocations return null (posix_memalign
    // ENOMEM, its out-pointer untouched), realloc leaves the old block live, and none of them
    // reaches the allocator (no allocation is counted, and no failure either, as the
    // reference's injected failures happen before `allocate`).
    #[cfg(feature = "alloc_failure_injection")]
    mod injection {
        use std::ffi::c_void;

        use super::{aligned_alloc, free, malloc, memalign, posix_memalign, realloc};
        use teseladb::engine::core::future::make_ready_future;
        use teseladb::engine::core::memory;
        use teseladb::engine::util::alloc_failure_injector::memory::local_failure_injector;
        use teseladb::engine::util::critical_alloc_section::memory::ScopedCriticalAllocSection;
        use teseladb::engine_test_case;

        unsafe extern "C" {
            fn calloc(nmemb: usize, size: usize) -> *mut c_void;
        }

        /// ENOMEM on Linux.
        const ENOMEM: i32 = 12;

        /// Cancels the injector's scheduled failure when dropped, also when a check fails: the
        /// test cases share the shard, and with it the injector.
        struct Disarm;

        impl Drop for Disarm {
            fn drop(&mut self) {
                local_failure_injector().cancel();
            }
        }

        /// `malloc(size)`, kept by the compiler (see [`with_next_point_failing`]).
        fn kept_malloc(size: usize) -> *mut c_void {
            // SAFETY: malloc has no preconditions.
            std::hint::black_box(unsafe { malloc(std::hint::black_box(size)) })
        }

        /// Runs `f` with the injector set to fail the next allocation point, and checks that
        /// `f` met it and that the allocator did not count an allocation failure. (The
        /// injector's failure is a caught panic, whose payload is itself allocated, so the
        /// allocation count is not checked.)
        /// The result is passed through `black_box`: the compiler knows the C allocation
        /// functions and may otherwise drop a call whose block is only compared with null.
        fn with_next_point_failing<T>(name: &str, f: impl FnOnce() -> T) -> T {
            let before = memory::stats();
            let _disarm = Disarm;
            local_failure_injector().fail_after(0);
            let r = std::hint::black_box(f());
            let after = memory::stats();
            assert!(local_failure_injector().failed(), "{name}: the injector did not fire");
            assert_eq!(
                after.failed_allocations(),
                before.failed_allocations(),
                "{name}: counted as an allocation failure"
            );
            r
        }

        engine_test_case!(test_injected_failure_in_c_entry_points, {
            // SAFETY: (every call below) the C allocation functions have no preconditions for
            // these arguments; they return null here, which nothing dereferences.
            unsafe {
                assert!(with_next_point_failing("malloc", || malloc(16)).is_null());
                assert!(with_next_point_failing("calloc", || calloc(4, 16)).is_null());
                assert!(
                    with_next_point_failing("realloc(null)", || realloc(std::ptr::null_mut(), 16))
                        .is_null()
                );
                assert!(with_next_point_failing("memalign", || memalign(64, 16)).is_null());
                assert!(
                    with_next_point_failing("aligned_alloc", || aligned_alloc(64, 64)).is_null()
                );
                assert!(
                    with_next_point_failing("__libc_malloc", || memory::__libc_malloc(16))
                        .is_null()
                );
                assert!(
                    with_next_point_failing("__libc_calloc", || memory::__libc_calloc(2, 8))
                        .is_null()
                );
                assert!(
                    with_next_point_failing("__libc_memalign", || memory::__libc_memalign(32, 8))
                        .is_null()
                );
            }
            make_ready_future(())
        });

        engine_test_case!(test_injected_failure_in_posix_memalign_keeps_the_out_pointer, {
            let marker = 0x5a5a_usize as *mut c_void;
            let mut p = marker;
            // SAFETY: `p` is a local out-pointer; 64 is a power of two multiple of a pointer.
            let r = with_next_point_failing("posix_memalign", || unsafe {
                posix_memalign(&mut p, 64, 100)
            });
            assert_eq!(r, ENOMEM);
            assert_eq!(p, marker, "posix_memalign wrote its out-pointer on an injected failure");
            let mut q = marker;
            // SAFETY: as above.
            let r = with_next_point_failing("__libc_posix_memalign", || unsafe {
                memory::__libc_posix_memalign(&mut q, 64, 100)
            });
            assert_eq!(r, ENOMEM);
            assert_eq!(q, marker);
            // With the injector idle the same call succeeds.
            // SAFETY: as above; the block is freed once.
            unsafe {
                assert_eq!(posix_memalign(&mut p, 64, 100), 0);
                assert!(!p.is_null() && (p as usize).is_multiple_of(64));
                free(p);
            }
            make_ready_future(())
        });

        // An aligned buffer whose posix_memalign fails with ENOMEM is a BadAlloc error, as the
        // reference's allocate_aligned_buffer throws std::bad_alloc.
        engine_test_case!(test_injected_failure_in_an_aligned_buffer_is_bad_alloc, {
            use teseladb::engine::core::future::BadAlloc;
            let r = with_next_point_failing("allocate_aligned_buffer_impl", || {
                memory::allocate_aligned_buffer_impl(4096, 4096)
            });
            match r {
                Err(e) => assert!(e.is::<BadAlloc>(), "failed with {e:?}"),
                Ok(p) => panic!("allocated {p:p} under the injector"),
            }
            let p = memory::allocate_aligned_buffer_impl(4096, 4096).expect("no injection");
            assert!((p as usize).is_multiple_of(4096));
            // SAFETY: a live block from posix_memalign, freed once.
            unsafe { free(p) };
            make_ready_future(())
        });

        engine_test_case!(test_injected_failure_in_realloc_keeps_the_block, {
            for (old, new) in [(16usize, 64usize), (64, 16), (40000, 80000), (80000, 100)] {
                // SAFETY: malloc has no preconditions; the block is written within its size.
                let p = unsafe { malloc(old) } as *mut u8;
                assert!(!p.is_null());
                // SAFETY: `p` holds `old` bytes.
                unsafe { std::ptr::write_bytes(p, 0x3c, old) };
                // SAFETY: `p` is a live block of this allocator.
                let q = with_next_point_failing("realloc", || unsafe {
                    realloc(p as *mut c_void, new)
                });
                assert!(q.is_null(), "realloc({old} -> {new}) succeeded under the injector");
                // SAFETY: the failed realloc left `p` live with its `old` bytes.
                let bytes = unsafe { std::slice::from_raw_parts(p, old) };
                assert!(
                    bytes.iter().all(|&b| b == 0x3c),
                    "realloc({old} -> {new}) touched the block"
                );
                // SAFETY: as above.
                let q = with_next_point_failing("__libc_realloc", || unsafe {
                    memory::__libc_realloc(p as *mut c_void, new)
                });
                assert!(q.is_null());
                // SAFETY: `p` is still live; freed once.
                unsafe { free(p as *mut c_void) };
            }
            make_ready_future(())
        });

        engine_test_case!(test_injected_failure_in_nothrow_object_allocations, {
            assert!(
                with_next_point_failing("operator_new_nothrow", || memory::operator_new_nothrow(8))
                    .is_null()
            );
            assert!(
                with_next_point_failing("operator_new_aligned_nothrow", || {
                    memory::operator_new_aligned_nothrow(8, 64, std::ptr::null())
                })
                .is_null()
            );
            assert!(
                with_next_point_failing("operator_new_array_aligned_nothrow", || {
                    memory::operator_new_array_aligned_nothrow(8, 64, std::ptr::null())
                })
                .is_null()
            );
            // The array form without alignment does not consult the injector, as in the
            // reference (`operator new[](size_t, std::nothrow_t)` is a plain `allocate`).
            let disarm = Disarm;
            local_failure_injector().fail_after(0);
            let p = std::hint::black_box(memory::operator_new_array_nothrow(8));
            assert!(!local_failure_injector().failed());
            drop(disarm);
            assert!(!p.is_null());
            memory::operator_delete_array_nothrow(p);
            make_ready_future(())
        });

        // A critical allocation section suppresses the injection: the allocation succeeds and
        // the scheduled failure stays pending until the section ends.
        engine_test_case!(test_injected_failure_suppressed_in_critical_section, {
            let disarm = Disarm;
            local_failure_injector().fail_after(0);
            let p = {
                let _cs = ScopedCriticalAllocSection::new();
                kept_malloc(32)
            };
            assert!(!p.is_null());
            assert!(!local_failure_injector().failed());
            // The injector is still armed: this call meets it and returns null.
            let q = kept_malloc(32);
            assert!(q.is_null());
            assert!(local_failure_injector().failed());
            drop(disarm);
            // SAFETY: `p` is live; freed once.
            unsafe { free(p) };
            make_ready_future(())
        });

        // The injector counts the allocation points of the C entry points: N plain mallocs
        // are N points, and `fail_after(k)` fails exactly the k-th one from now.
        engine_test_case!(test_injected_failure_counts_allocation_points, {
            let inj = local_failure_injector();
            let start = inj.alloc_count();
            let mut ptrs = Vec::with_capacity(8);
            let mut counted_before_failure = 0;
            let disarm = Disarm;
            inj.fail_after(5);
            for i in 0..8 {
                if i == 5 {
                    counted_before_failure = inj.alloc_count() - start;
                }
                ptrs.push(kept_malloc(24));
            }
            drop(disarm);
            let failed: Vec<usize> =
                ptrs.iter().enumerate().filter(|(_, p)| p.is_null()).map(|(i, _)| i).collect();
            assert_eq!(failed, vec![5], "the wrong allocations failed");
            // (The failure is a caught panic, whose report may allocate through the C entry
            // points too, so only the points before it are counted exactly.)
            assert_eq!(counted_before_failure, 5, "allocation points counted");
            for p in ptrs {
                // SAFETY: each pointer is null or a live block, freed once.
                unsafe { free(p) };
            }
            make_ready_future(())
        });
    }
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
