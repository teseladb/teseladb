//! A test case whose body runs in an engine thread, where it can wait on futures with `get()`.
//!
//! The coverage tests use it in place of the engine's `engine_thread_test_case!`, whose
//! expansion puts its `$body:block` fragment after `-> ()` in a closure, which rustc rejects
//! ("cannot use a `block` macro fragment here"). This one wraps the block in a closure without a
//! return type.

/// Registers `$name` as an engine test whose body runs in an engine thread; a panic of the body
/// fails the test.
macro_rules! thread_case {
    ($name:ident, $body:block) => {
        teseladb::engine_test_case!($name, {
            teseladb::engine::core::thread::async_(move || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || $body))
                    .map_err(teseladb::engine::core::future::ExceptionPtr::from_panic)
            })
        });
    };
}
