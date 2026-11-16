//! Tests of `AppTemplate::run`: the default memory settings start the reactor, and the
//! exit status of the main function becomes the result of `run`.

#![cfg(target_os = "linux")]

use std::time::Duration;

use teseladb::engine::core::app_template::{AppTemplate, EngineOptions};
use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::smp_options::MemoryAllocator;
use teseladb::engine::util::log::LogLevel;

// Always run this (regression: the reactor with the standard memory allocator settings).
#[test]
fn app_standard_memory_allocator() {
    // by default, use conservative settings instead of maxing out the performance
    // for testing app_template and underlying reactor's handling of different
    // settings
    let mut opts = EngineOptions::new();
    opts.smp_opts.thread_affinity.set_value(false);
    opts.smp_opts.mbind.set_value(false);
    opts.smp_opts.smp.set_value(1);
    opts.smp_opts.lock_memory.set_value(false);
    opts.smp_opts.memory_allocator = MemoryAllocator::Standard;
    opts.log_opts.default_log_level.set_value(LogLevel::Error);
    let mut app = AppTemplate::with_options(opts);
    // app.run() takes `char**` not `char* const *`, so appease it
    let args = vec!["prog".to_string()];
    let expected_status = 42;
    let actual_status = app.run_with_exit_code(args, move || {
        // Add a small sleep to ensure the reactor does
        // some of its background stuff, pollers etc for example.
        // We only need to ensure we get put on the waiting task queue
        // to provoke the problem, thus a short (probably even to long here)
        // sleep will do.
        sleep(Duration::from_secs(2)).then(move |()| make_ready_future::<i32>(expected_status))
    });
    assert_eq!(actual_status, expected_status);
}

#[test]
fn return_0_for_func_returning_void() {
    let mut app = AppTemplate::new(Default::default());
    let args = vec!["prog".to_string()];
    let status = app.run(args, || make_ready_future(()));
    assert_eq!(status, 0);
}

#[test]
fn return_status_for_func_returning_int() {
    let mut app = AppTemplate::new(Default::default());
    let args = vec!["prog".to_string()];
    let expected_status = 42;
    let actual_status =
        app.run_with_exit_code(args, move || make_ready_future::<i32>(expected_status));
    assert_eq!(actual_status, expected_status);
}
