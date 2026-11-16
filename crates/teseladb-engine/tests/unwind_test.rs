//! Tests that unwinding a failure out of a function keeps the thread's signal mask.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::mem::MaybeUninit;

use teseladb::engine::core::future::{ExceptionPtr, RuntimeError};
use teseladb::engine::core::posix::throw_pthread_error;
#[cfg(not(engine_backtrace_unimplemented))]
use teseladb::engine::util::backtrace::backtrace_with;

/// The C library's `sigset_t`: 1024 bits.
#[repr(C)]
#[derive(Clone, Copy)]
struct SigsetT {
    val: [u64; 16],
}

const SIG_SETMASK: i32 = 2;
/// The C library's `NSIG`.
const NSIG: i32 = 65;

unsafe extern "C" {
    fn sigfillset(set: *mut SigsetT) -> i32;
    fn sigismember(set: *const SigsetT, signum: i32) -> i32;
    fn pthread_sigmask(how: i32, set: *const SigsetT, oldset: *mut SigsetT) -> i32;
}

fn foo() -> Result<(), ExceptionPtr> {
    Err(ExceptionPtr::new(RuntimeError("foo".to_string())))
}

// Regression: unwinding must not change the signal mask.
#[test]
fn test_signal_mask_is_preserved_on_unwinding() {
    let mut mask = MaybeUninit::<SigsetT>::zeroed();
    let mut old = MaybeUninit::<SigsetT>::zeroed();
    // SAFETY: both sets are valid, writable sigset_t storage.
    unsafe { sigfillset(mask.as_mut_ptr()) };
    // SAFETY: as above.
    let res = unsafe { pthread_sigmask(SIG_SETMASK, mask.as_ptr(), old.as_mut_ptr()) };
    throw_pthread_error(res).expect("pthread_sigmask");

    // Check which signals we actually managed to block
    // SAFETY: a null set only queries the mask into `mask`.
    let res = unsafe { pthread_sigmask(SIG_SETMASK, std::ptr::null(), mask.as_mut_ptr()) };
    throw_pthread_error(res).expect("pthread_sigmask");

    if foo().is_err() {
        // ignore
    }

    // Check the backtrace too
    #[cfg(not(engine_backtrace_unimplemented))]
    {
        let mut count = 0usize;
        backtrace_with(|_| count += 1, false);
        assert!(count > 0);
    }

    {
        let mut mask2 = MaybeUninit::<SigsetT>::zeroed();
        // SAFETY: `old` was filled by the first call; `mask2` is writable.
        let res = unsafe { pthread_sigmask(SIG_SETMASK, old.as_ptr(), mask2.as_mut_ptr()) };
        throw_pthread_error(res).expect("pthread_sigmask");
        for i in 1..NSIG {
            // SAFETY: both sets are initialised.
            unsafe {
                assert_eq!(sigismember(mask2.as_ptr(), i), sigismember(mask.as_ptr(), i));
            }
        }
    }
}
