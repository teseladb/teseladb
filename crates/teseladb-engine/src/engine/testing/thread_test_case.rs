//! Tests whose body runs in an engine thread.
//!
//! [`engine_thread_test_case!`](crate::engine_thread_test_case) declares a test like
//! [`engine_test_case!`](crate::engine_test_case), but runs the body inside an engine
//! thread (a stackful context on the reactor), so the body may block on futures with
//! `.get()` instead of composing them.

/// Declares a test whose body runs in an engine thread, so it may wait on futures with
/// `.get()`.
///
/// Usage: `engine_thread_test_case!(name, { body })`; attributes such as `#[ignore]` go
/// before the name.
#[macro_export]
macro_rules! engine_thread_test_case {
    ($(#[$attr:meta])* $name:ident, $body:block) => {
        $crate::engine_thread_test_case_with_deco!($(#[$attr])* $name, $body);
    };
}

/// Like [`engine_thread_test_case!`](crate::engine_thread_test_case); the attributes given
/// before the name are applied to the generated `#[test]` function.
#[macro_export]
macro_rules! engine_thread_test_case_with_deco {
    ($(#[$attr:meta])* $name:ident, $body:block) => {
        $crate::engine_test_case_with_deco!($(#[$attr])* $name, {
            // A panic of the body (a failed assertion) is caught on the thread's own stack
            // and turned into the test's error, which fails the test.
            $crate::engine::core::thread::async_(move || -> ::std::result::Result<(), $crate::engine::core::future::ExceptionPtr> {
                ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(move || -> () $body))
                    .map_err($crate::engine::core::future::ExceptionPtr::from_panic)
            })
        });
    };
}

/// Like [`engine_thread_test_case!`](crate::engine_thread_test_case), without attributes.
#[macro_export]
macro_rules! engine_thread_test_case_without_deco {
    ($name:ident, $body:block) => {
        $crate::engine_thread_test_case_with_deco!($name, $body);
    };
}
