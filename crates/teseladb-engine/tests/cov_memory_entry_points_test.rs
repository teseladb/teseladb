//! Tests of every allocation entry point the engine allocator exports: the C library family
//! under its public and internal names (zeroing, resizing up, down and to nothing, aligned
//! allocations), and the exported `operator new` and `operator delete` symbols in their plain,
//! array, sized, non-throwing and aligned forms. Every allocation must come from the shard's
//! pool and keep its contents and alignment.
#![cfg(target_os = "linux")]

use std::ffi::c_void;

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::memory::{get_memory_layout, stats};
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

unsafe extern "C" {
    fn __libc_malloc(n: usize) -> *mut c_void;
    fn __libc_free(p: *mut c_void);
    fn __libc_calloc(n: usize, m: usize) -> *mut c_void;
    fn calloc(n: usize, m: usize) -> *mut c_void;
    fn __libc_realloc(p: *mut c_void, n: usize) -> *mut c_void;
    fn realloc(p: *mut c_void, n: usize) -> *mut c_void;
    fn posix_memalign(out: *mut *mut c_void, align: usize, size: usize) -> i32;
    fn __libc_posix_memalign(out: *mut *mut c_void, align: usize, size: usize) -> i32;
    fn memalign(align: usize, size: usize) -> *mut c_void;
    fn __libc_memalign(align: usize, size: usize) -> *mut c_void;
    fn aligned_alloc(align: usize, size: usize) -> *mut c_void;
    fn cfree(p: *mut c_void);
    fn __libc_cfree(p: *mut c_void);
    fn malloc_usable_size(p: *mut c_void) -> usize;
    fn free(p: *mut c_void);

    #[link_name = "_Znwm"]
    fn op_new(n: usize) -> *mut c_void;
    #[link_name = "_Znam"]
    fn op_new_array(n: usize) -> *mut c_void;
    #[link_name = "_ZdlPv"]
    fn op_delete(p: *mut c_void);
    #[link_name = "_ZdaPv"]
    fn op_delete_array(p: *mut c_void);
    #[link_name = "_ZdlPvm"]
    fn op_delete_sized(p: *mut c_void, n: usize);
    #[link_name = "_ZdaPvm"]
    fn op_delete_array_sized(p: *mut c_void, n: usize);
    #[link_name = "_ZnwmSt9nothrow_t"]
    fn op_new_nothrow(n: usize, tag: *const c_void) -> *mut c_void;
    #[link_name = "_ZnamSt9nothrow_t"]
    fn op_new_array_nothrow(n: usize, tag: *const c_void) -> *mut c_void;
    #[link_name = "_ZdlPvSt9nothrow_t"]
    fn op_delete_nothrow(p: *mut c_void, tag: *const c_void);
    #[link_name = "_ZdaPvSt9nothrow_t"]
    fn op_delete_array_nothrow(p: *mut c_void, tag: *const c_void);
    #[link_name = "_ZdlPvmSt9nothrow_t"]
    fn op_delete_sized_nothrow(p: *mut c_void, n: usize, tag: *const c_void);
    #[link_name = "_ZdaPvmSt9nothrow_t"]
    fn op_delete_array_sized_nothrow(p: *mut c_void, n: usize, tag: *const c_void);
    #[link_name = "_ZnwmSt11align_val_t"]
    fn op_new_aligned(n: usize, a: usize) -> *mut c_void;
    #[link_name = "_ZnamSt11align_val_t"]
    fn op_new_array_aligned(n: usize, a: usize) -> *mut c_void;
    #[link_name = "_ZnwmSt11align_val_tRKSt9nothrow_t"]
    fn op_new_aligned_nothrow(n: usize, a: usize, tag: *const c_void) -> *mut c_void;
    #[link_name = "_ZnamSt11align_val_tRKSt9nothrow_t"]
    fn op_new_array_aligned_nothrow(n: usize, a: usize, tag: *const c_void) -> *mut c_void;
    #[link_name = "_ZdlPvSt11align_val_t"]
    fn op_delete_aligned(p: *mut c_void, a: usize);
    #[link_name = "_ZdaPvSt11align_val_t"]
    fn op_delete_array_aligned(p: *mut c_void, a: usize);
    #[link_name = "_ZdlPvmSt11align_val_t"]
    fn op_delete_sized_aligned(p: *mut c_void, n: usize, a: usize);
    #[link_name = "_ZdaPvmSt11align_val_t"]
    fn op_delete_array_sized_aligned(p: *mut c_void, n: usize, a: usize);
    #[link_name = "_ZdlPvSt11align_val_tRKSt9nothrow_t"]
    fn op_delete_aligned_nothrow(p: *mut c_void, a: usize, tag: *const c_void);
    #[link_name = "_ZdaPvSt11align_val_tRKSt9nothrow_t"]
    fn op_delete_array_aligned_nothrow(p: *mut c_void, a: usize, tag: *const c_void);
}

/// A request no shard can meet; the allocator reports it and returns null.
const HUGE: usize = isize::MAX as usize - 4096;

/// Checks that `p` is a non-null allocation of the shard's pool aligned to `align`, writes
/// `len` bytes into it and reads them back.
fn check(p: *mut c_void, len: usize, align: usize) -> *mut c_void {
    assert!(!p.is_null());
    let layout = get_memory_layout().unwrap();
    assert!((layout.start..layout.end).contains(&(p as usize)), "{p:p} is in the pool");
    assert_eq!(p as usize % align, 0, "{p:p} is aligned to {align}");
    // SAFETY: `p` holds at least `len` bytes.
    unsafe {
        std::ptr::write_bytes(p.cast::<u8>(), 0x5a, len);
        assert!(std::slice::from_raw_parts(p.cast::<u8>(), len).iter().all(|&b| b == 0x5a));
    }
    p
}

thread_case!(c_library_entry_points_allocate_from_the_pool, {
    let mallocs = stats().mallocs();
    // SAFETY: each allocation is checked, used within its size and freed once.
    unsafe {
        let p = check(__libc_malloc(40), 40, 8);
        __libc_free(p);

        // Zeroed allocations, and the overflow of their size.
        for z in [calloc(16, 16), __libc_calloc(3, 1000)] {
            assert!(!z.is_null());
            let n = malloc_usable_size(z);
            assert!(std::slice::from_raw_parts(z.cast::<u8>(), n.min(256)).iter().all(|&b| b == 0));
            free(z);
        }

        // Resizing: from nothing, up (the contents move), down (in place), the same size
        // and to nothing.
        let r = check(realloc(std::ptr::null_mut(), 64), 64, 8);
        let r = __libc_realloc(r, 8192);
        assert!(std::slice::from_raw_parts(r.cast::<u8>(), 64).iter().all(|&b| b == 0x5a));
        let usable = malloc_usable_size(r);
        assert_eq!(realloc(r, usable), r, "the same size keeps the block");
        let small = realloc(r, 100);
        assert!(!small.is_null());
        assert!(std::slice::from_raw_parts(small.cast::<u8>(), 64).iter().all(|&b| b == 0x5a));
        assert!(realloc(small, 0).is_null(), "resizing to nothing frees");

        // Aligned allocations.
        let mut out: *mut c_void = std::ptr::null_mut();
        assert_eq!(posix_memalign(&mut out, 256, 1000), 0);
        free(check(out, 1000, 256));
        assert_eq!(__libc_posix_memalign(&mut out, 4096, 100), 0);
        cfree(check(out, 100, 4096));
        __libc_cfree(check(memalign(64, 33), 33, 64));
        free(check(__libc_memalign(512, 1), 1, 512));
        free(check(aligned_alloc(1024, 2048), 2048, 1024));
    }
    assert!(stats().mallocs() >= mallocs + 8);
});

thread_case!(new_and_delete_operators_allocate_from_the_pool, {
    let tag = std::ptr::null();
    // SAFETY: each allocation is checked, used within its size and released by the matching
    // operator once.
    unsafe {
        op_delete(check(op_new(24), 24, 8));
        op_delete_array(check(op_new_array(240), 240, 8));
        op_delete_sized(check(op_new(48), 48, 8), 48);
        op_delete_array_sized(check(op_new_array(480), 480, 8), 480);
        op_delete_nothrow(check(op_new_nothrow(16, tag), 16, 8), tag);
        op_delete_array_nothrow(check(op_new_array_nothrow(160, tag), 160, 8), tag);
        op_delete_sized_nothrow(check(op_new_nothrow(32, tag), 32, 8), 32, tag);
        op_delete_array_sized_nothrow(check(op_new_array_nothrow(320, tag), 320, 8), 320, tag);
        op_delete_aligned(check(op_new_aligned(100, 128), 100, 128), 128);
        op_delete_array_aligned(check(op_new_array_aligned(1000, 256), 1000, 256), 256);
        op_delete_sized_aligned(check(op_new_aligned(64, 64), 64, 64), 64, 64);
        op_delete_array_sized_aligned(check(op_new_array_aligned(640, 512), 640, 512), 640, 512);
        op_delete_aligned_nothrow(check(op_new_aligned_nothrow(10, 32, tag), 10, 32), 32, tag);
        op_delete_array_aligned_nothrow(
            check(op_new_array_aligned_nothrow(90, 4096, tag), 90, 4096),
            4096,
            tag,
        );
        // The non-throwing operators return null when the request cannot be met.
        assert!(op_new_nothrow(HUGE, tag).is_null());
        assert!(op_new_array_aligned_nothrow(HUGE, 64, tag).is_null());
    }
});
