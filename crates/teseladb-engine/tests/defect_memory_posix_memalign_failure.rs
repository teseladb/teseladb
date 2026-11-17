#![cfg(all(target_os = "linux", not(engine_default_allocator)))]
//! `posix_memalign` leaves its output pointer unchanged when the allocation fails, as POSIX
//! requires, instead of overwriting it with null.

use std::ffi::c_void;

use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::core::memory::PAGE_SIZE;
use teseladb::engine_test_case;

unsafe extern "C" {
    fn posix_memalign(memptr: *mut *mut c_void, alignment: usize, size: usize) -> i32;
}

const ENOMEM: i32 = 12;

engine_test_case!(test_posix_memalign_failure_preserves_pointer, {
    // 1 TiB: a page count that fits a span, far beyond the memory of a shard.
    let size = 1usize << 40;
    let mut sentinel = 0u8;
    let original = (&mut sentinel as *mut u8).cast::<c_void>();
    let mut p = original;
    // Called through a volatile function pointer so that the compiler cannot apply the
    // standard posix_memalign failure semantics and drop the check below.
    let call: unsafe extern "C" fn(*mut *mut c_void, usize, usize) -> i32 =
        // SAFETY: reading a local function pointer.
        unsafe { std::ptr::read_volatile(&(posix_memalign as _)) };
    // SAFETY: `p` is a valid out pointer.
    let r = unsafe { call(&mut p, PAGE_SIZE, size) };
    assert_eq!(r, ENOMEM);
    assert_eq!(p, original, "posix_memalign overwrote the pointer on failure");
    make_ready_future(())
});
