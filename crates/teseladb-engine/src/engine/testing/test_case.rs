//! Declaring reactor tests.
//!
//! [`engine_test_case!`](crate::engine_test_case) declares a test whose body returns a
//! `Future<()>`. It generates a test object registered with the
//! [`engine_test`](crate::engine::testing::engine_test) registry and a `#[test]` function
//! that runs it on the test runner's reactor through the standard Rust harness.
//! Attributes such as `#[ignore]` or `#[should_panic]` are written before the test name.

/// Declares a test whose body returns a `Future<()>` and runs on the reactor of the test
/// runner.
///
/// ```no_run
/// use std::time::Duration;
/// use teseladb::engine::core::sleep::sleep_steady as sleep;
/// use teseladb::engine_test_case;
///
/// engine_test_case!(test_sleep, {
///     sleep(Duration::from_millis(1))
/// });
/// # fn main() {}
/// ```
#[macro_export]
macro_rules! engine_test_case {
    ($(#[$attr:meta])* $name:ident, $body:block) => {
        $crate::engine_test_case_with_deco!($(#[$attr])* $name, $body);
    };
}

/// Like [`engine_test_case!`](crate::engine_test_case); the attributes given before the
/// name are applied to the generated `#[test]` function.
///
/// The test object lives in a module named after the test (the `#[test]` function takes the
/// name in the value namespace, the module in the type namespace), which sees the items of the
/// enclosing module, and is registered before `main` for
/// [`run_registered_tests`](crate::engine::testing::engine_test::testing::run_registered_tests).
#[macro_export]
macro_rules! engine_test_case_with_deco {
    ($(#[$attr:meta])* $name:ident, $body:block) => {
        #[doc(hidden)]
        #[allow(non_snake_case)]
        mod $name {
            #[allow(unused_imports)]
            use super::*;

            /// The test object.
            pub(super) struct EngineTestCase;

            impl $crate::engine::testing::engine_test::testing::EngineTest for EngineTestCase {
                fn name(&self) -> &'static str {
                    stringify!($name)
                }
                fn get_test_file(&self) -> &'static str {
                    file!()
                }
                fn get_test_line(&self) -> u32 {
                    line!()
                }
                fn run_test_case(&self) -> $crate::engine::core::future::Future<()> $body
            }

            pub(super) static INSTANCE: EngineTestCase = EngineTestCase;

            $crate::engine_register_test!(INSTANCE);
        }

        // Test names may contain double underscores (`test_x__y`), which are not snake case.
        #[allow(non_snake_case)]
        #[test]
        $(#[$attr])*
        fn $name() {
            $crate::engine::testing::engine_test::testing::run_harness_test(&$name::INSTANCE);
        }
    };
}

/// Like [`engine_test_case!`](crate::engine_test_case), without attributes.
#[macro_export]
macro_rules! engine_test_case_without_deco {
    ($name:ident, $body:block) => {
        $crate::engine_test_case_with_deco!($name, $body);
    };
}
