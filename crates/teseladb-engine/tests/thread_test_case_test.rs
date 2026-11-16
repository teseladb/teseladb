//! Tests of the `engine_thread_test_case!` family of macros: each declares a test whose body
//! runs in an engine thread, where it may wait on futures with `get()`.

#![cfg(target_os = "linux")]

use std::time::Duration;

use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::thread::Thread;
use teseladb::{
    engine_thread_test_case, engine_thread_test_case_with_deco,
    engine_thread_test_case_without_deco,
};

engine_thread_test_case!(body_runs_in_a_thread_and_waits_with_get, {
    assert!(Thread::running_in_thread());
    // The sleep suspends the thread; get() returns once the reactor resolves it.
    sleep(Duration::from_millis(1)).get().unwrap();
    assert_eq!(make_ready_future(7_u32).get().unwrap(), 7);
});

engine_thread_test_case_with_deco!(
    /// The attributes before the name reach the generated test function.
    #[allow(clippy::unit_cmp)]
    with_deco_takes_attributes,
    {
        assert!(Thread::running_in_thread());
        assert_eq!(make_ready_future(()).get().unwrap(), ());
    }
);

engine_thread_test_case_without_deco!(without_deco_runs_the_body, {
    assert!(Thread::running_in_thread());
    let value = make_ready_future(String::from("engine")).get().unwrap();
    assert_eq!(value, "engine");
});
