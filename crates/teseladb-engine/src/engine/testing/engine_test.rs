//! The registry of reactor tests.
//!
//! Tests declared with [`engine_test_case!`](crate::engine_test_case) or
//! [`engine_thread_test_case!`](crate::engine_thread_test_case) register into a
//! process-wide list before `main` runs (see
//! [`engine_register_test!`](crate::engine_register_test)). A test program with its own
//! `main` runs that list with [`testing::run_registered_tests`] (through the test entry
//! point). Each test is also a `#[test]` of the standard Rust harness, which runs it
//! through the same global test runner with [`testing::run_harness_test`].
//!
//! All test bodies run serially on the runner's reactor, one at a time.

/// Registers the `static` test object `$instance` (a `&'static dyn EngineTest`) before
/// `main`.
///
/// It places a registration function in the platform's initializer table
/// (`.init_array` on ELF, `__mod_init_func` on Mach-O), which the loader runs before
/// `main`. The function also blocks every signal except `SIGSEGV` on the main thread, so
/// threads spawned later inherit the mask.
#[macro_export]
macro_rules! engine_register_test {
    ($instance:path) => {
        #[used]
        #[cfg_attr(
            any(target_os = "linux", target_os = "android", target_os = "freebsd"),
            unsafe(link_section = ".init_array")
        )]
        #[cfg_attr(target_vendor = "apple", unsafe(link_section = "__DATA,__mod_init_func"))]
        static __ENGINE_REGISTER_TEST: extern "C" fn() = {
            extern "C" fn register() {
                $crate::engine::testing::test_runner::testing::block_signals();
                $crate::engine::testing::engine_test::testing::register_test(&$instance);
            }
            register
        };
    };
}

/// Invokes the macro `$func` with the remaining arguments: `engine_test_invoke!(m, a, b)`
/// expands to `m!(a, b)`.
#[macro_export]
macro_rules! engine_test_invoke {
    ($func:ident, $($args:tt)*) => {
        $func!($($args)*)
    };
}

/// The test registry and runners.
pub mod testing {
    use std::sync::Mutex;

    use crate::engine::core::future::{ExceptionPtr, Future};
    use crate::engine::core::on_internal_error::set_abort_on_internal_error;
    use crate::engine::testing::test_runner::testing::global_test_runner;

    /// A reactor test case: `run_test_case` runs on the reactor of the test runner.
    pub trait EngineTest: Sync {
        /// The name of the test.
        fn name(&self) -> &'static str;

        /// The source file that declares the test.
        fn get_test_file(&self) -> &'static str;

        /// The line of the test's declaration in its file, used to run the tests of a file
        /// in declaration order (0 when unknown).
        fn get_test_line(&self) -> u32 {
            0
        }

        /// The test body.
        fn run_test_case(&self) -> Future<()>;

        /// Runs the test on the reactor and returns its error, if any.
        ///
        /// # Errors
        ///
        /// Returns the error the test body's future resolved to.
        fn run(&'static self) -> Result<(), ExceptionPtr>
        where
            Self: Sized,
        {
            run_test(self)
        }
    }

    /// The name of the running test. Process-wide: it is written and read on the reactor
    /// thread, where the body runs.
    static CURRENT_TEST: Mutex<&'static str> = Mutex::new("");

    /// Runs `t` on the reactor of the test runner, with abort-on-internal-error enabled.
    ///
    /// # Errors
    ///
    /// Returns the error the test body's future resolved to.
    pub fn run_test(t: &'static dyn EngineTest) -> Result<(), ExceptionPtr> {
        set_abort_on_internal_error(true);
        global_test_runner().run_sync(Box::new(move || {
            // Set inside the serialized run, so a test run from another harness thread at the
            // same time cannot overwrite it while this one runs.
            match CURRENT_TEST.lock() {
                Ok(mut n) => *n = t.name(),
                Err(p) => *p.into_inner() = t.name(),
            }
            t.run_test_case()
        }))
    }

    /// Returns the name of the test being run (empty before the first test).
    pub fn get_name() -> &'static str {
        match CURRENT_TEST.lock() {
            Ok(n) => *n,
            Err(p) => *p.into_inner(),
        }
    }

    /// The body of the `#[test]` function that
    /// [`engine_test_case!`](crate::engine_test_case) generates: starts the global runner
    /// (once per process) and runs `t`.
    ///
    /// # Panics
    ///
    /// Panics (failing the Rust test) when the test fails or the reactor did not start.
    pub fn run_harness_test(t: &'static dyn EngineTest) {
        let args: Vec<String> = std::env::args().take(1).collect();
        let runner = global_test_runner();
        runner.start(&args);
        let result = run_test(t);
        // A reactor that does not start makes run_sync() return without running the
        // body: the test fails instead of passing unrun.
        if runner.reactor_failed() {
            panic!("{}: the engine reactor failed to start; the test body did not run", t.name());
        }
        if let Err(e) = result {
            panic!("{}", e);
        }
    }

    /// The registered tests.
    static TESTS: Mutex<Vec<&'static dyn EngineTest>> = Mutex::new(Vec::new());

    /// Adds `t` to the tests [`run_registered_tests`] runs.
    pub fn register_test(t: &'static dyn EngineTest) {
        match TESTS.lock() {
            Ok(mut v) => v.push(t),
            Err(p) => p.into_inner().push(t),
        }
    }

    /// The `main` of a test program: starts the runner with `init`, then runs the
    /// registered tests, in declaration order per file.
    ///
    /// With `--run_test=<name>` in `args`, only the test with that exact name runs. Each
    /// failure is printed to standard error as `<file>: fatal error: in "<name>": <error>`.
    ///
    /// Returns the number of failures (capped at 255), or 200 when `init` fails.
    pub fn run_registered_tests(init: fn(&[String]) -> bool, args: &[String]) -> i32 {
        if !init(args) {
            return 200;
        }
        let filter = args.iter().find_map(|a| a.strip_prefix("--run_test="));
        let mut tests: Vec<&'static dyn EngineTest> = match TESTS.lock() {
            Ok(v) => v.clone(),
            Err(p) => p.into_inner().clone(),
        };
        // The registration functions run before `main` in the order the linker lays them
        // out, which follows code units rather than source order, so the declaration
        // order is restored from each test's file and line.
        tests.sort_by_key(|t| (t.get_test_file(), t.get_test_line()));
        let mut failures = 0;
        for t in tests {
            if filter.is_some_and(|f| f != t.name()) {
                continue;
            }
            if let Err(e) = run_test(t) {
                eprintln!("{}: fatal error: in \"{}\": {}", t.get_test_file(), t.name(), e);
                failures += 1;
            }
        }
        failures.min(255)
    }

    /// Predicates on the message of an error, for tests that check which error a call
    /// failed with.
    pub mod exception_predicate {
        use crate::engine::core::future::ExceptionPtr;

        /// The message of the error; empty for a null pointer.
        fn what(e: &ExceptionPtr) -> String {
            e.get().map(|x| x.what()).unwrap_or_default()
        }

        /// Returns a predicate that is true when the error's message is exactly
        /// `expected_message`; on mismatch it prints both messages to standard error.
        pub fn message_equals(expected_message: &'static str) -> impl Fn(&ExceptionPtr) -> bool {
            move |e: &ExceptionPtr| {
                let error = what(e);
                if error == expected_message {
                    true
                } else {
                    eprintln!("Expected \"{}\" but got \"{}\"", expected_message, error);
                    false
                }
            }
        }

        /// Returns a predicate that is true when the error's message contains
        /// `expected_message`; on mismatch it prints both messages to standard error.
        pub fn message_contains(expected_message: &'static str) -> impl Fn(&ExceptionPtr) -> bool {
            move |e: &ExceptionPtr| {
                let error = what(e);
                if error.contains(expected_message) {
                    true
                } else {
                    eprintln!("Expected \"{}\" but got \"{}\"", expected_message, error);
                    false
                }
            }
        }
    }
}
