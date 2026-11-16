//! The `main` of reactor test programs.
//!
//! [`testing::entry_point`] installs placeholder handlers for `SIGSEGV` and `SIGABRT`,
//! starts the global test runner, runs every test registered in
//! [`engine_test`](crate::engine::testing::engine_test) with
//! [`run_registered_tests`](crate::engine::testing::engine_test::testing::run_registered_tests),
//! stops the runner and returns the process exit code.

/// The test entry point.
pub mod testing {
    use std::ffi::c_int;

    use crate::engine::testing::engine_test::testing::run_registered_tests;
    use crate::engine::testing::test_runner::testing::global_test_runner;

    fn init_unit_test_suite(args: &[String]) -> bool {
        global_test_runner().start(args)
    }

    extern "C" fn dummy_handler(_sig: c_int) {
        // This handler should have been replaced.
        // SAFETY: _exit is async-signal-safe.
        unsafe { _exit(1) };
    }

    /// `struct sigaction` and `sigaction()` on Linux, as declared by the reactor.
    #[cfg(target_os = "linux")]
    use crate::engine::core::reactor::{Sigaction, sigaction};

    /// `struct sigaction` as libc lays it out on the other supported platforms.
    #[cfg(not(target_os = "linux"))]
    #[repr(C)]
    struct Sigaction {
        sa_handler: extern "C" fn(c_int),
        sa_mask: u32,
        sa_flags: c_int,
    }

    #[cfg(not(target_os = "linux"))]
    unsafe extern "C" {
        // The reactor declares it with the Linux `struct sigaction`; this is the platform's own.
        #[allow(clashing_extern_declarations)]
        fn sigaction(sig: c_int, act: *const Sigaction, old: *mut Sigaction) -> c_int;
    }

    unsafe extern "C" {
        fn _exit(code: c_int) -> !;
    }

    /// SIGSEGV
    const SIGSEGV: c_int = 11;
    /// SIGABRT
    const SIGABRT: c_int = 6;

    fn install_dummy_handler(sig: c_int) {
        #[cfg(target_os = "linux")]
        let sa = Sigaction {
            sa_sigaction: dummy_handler as extern "C" fn(c_int) as usize,
            ..Sigaction::default()
        };
        #[cfg(not(target_os = "linux"))]
        let sa = Sigaction { sa_handler: dummy_handler, sa_mask: 0, sa_flags: 0 };
        // SAFETY: `sa` is a fully initialized `struct sigaction` that outlives the call, and
        // a null `old` pointer is allowed.
        unsafe { sigaction(sig, &sa, std::ptr::null_mut()) };
    }

    /// Runs the registered tests on a reactor and returns the process exit code.
    ///
    /// The code is the test failure count (or 200 if the runner did not start) when
    /// non-zero; otherwise the exit code of the runner's reactor.
    pub fn entry_point(args: Vec<String>) -> i32 {
        // Install placeholder handlers for SIGSEGV and SIGABRT before anything else runs,
        // so that no other component installs its own handlers first; the reactor
        // replaces them with its backtrace-printing handlers when it is constructed.
        // They are installed even in AddressSanitizer builds, which have their own
        // handlers.
        for sig in [SIGSEGV, SIGABRT] {
            install_dummy_handler(sig);
        }
        let tests_exit_code = run_registered_tests(init_unit_test_suite, &args);
        let engine_exit_code = global_test_runner().finalize();
        if tests_exit_code != 0 {
            return tests_exit_code;
        }
        engine_exit_code
    }
}
