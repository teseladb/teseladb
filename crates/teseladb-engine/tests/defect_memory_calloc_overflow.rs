#![allow(unsafe_code)]
#![cfg(all(target_os = "linux", not(engine_default_allocator)))]
//! `calloc` whose `nmemb * size` overflows returns null, as the C standard requires, instead
//! of aborting the process.

use std::ffi::c_void;

use teseladb::engine::core::future::make_ready_future;
use teseladb::engine_test_case;

unsafe extern "C" {
    fn calloc(nmemb: usize, size: usize) -> *mut c_void;
}

engine_test_case!(calloc_size_overflow_returns_null, {
    // Read through a volatile so that the compiler cannot fold the call.
    // SAFETY: reading a local.
    let nmemb = unsafe { std::ptr::read_volatile(&usize::MAX) };
    // SAFETY: calloc has no preconditions; a null result is checked.
    // black_box: the compiler may otherwise assume a known allocation function succeeds
    // when its result is only compared with null, and drop the call.
    let obj = std::hint::black_box(unsafe { calloc(nmemb, 2) });
    assert!(obj.is_null());
    make_ready_future(())
});
