//! Tests of `AppTemplate::run`: the default memory settings start the reactor, and the exit
//! status of the main function becomes the result of `run`. A program with its own `main`
//! (`harness = false`): one engine per process, so each case runs this program again with
//! `APP_TEMPLATE_CASE` naming it. As three tests of the default harness they started three
//! engines at once in one process, which crashed or never ended now and then (18-11-2026).

#![cfg(target_os = "linux")]

use std::process::Command;
use std::time::{Duration, Instant};

use teseladb::engine::core::app_template::{AppTemplate, EngineOptions};
use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::smp_options::MemoryAllocator;
use teseladb::engine::util::log::LogLevel;

const CASE_VAR: &str = "APP_TEMPLATE_CASE";
const CASE_TIMEOUT: Duration = Duration::from_secs(120);

/// The exit codes of a case's process: its check held, it did not, or nobody knows the case.
const HELD: i32 = 0;
const BROKEN: i32 = 1;
const UNKNOWN: i32 = 2;

/// A status no default gives.
const STATUS: i32 = 42;

// Always run this (regression: the reactor with the standard memory allocator settings).
fn app_standard_memory_allocator() -> bool {
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
    let args = vec!["prog".to_string()];
    let actual_status = app.run_with_exit_code(args, move || {
        // Add a small sleep to ensure the reactor does
        // some of its background stuff, pollers etc for example.
        // We only need to ensure we get put on the waiting task queue
        // to provoke the problem, thus a short (probably even to long here)
        // sleep will do.
        sleep(Duration::from_secs(2)).then(move |()| make_ready_future::<i32>(STATUS))
    });
    actual_status == STATUS
}

fn return_0_for_func_returning_void() -> bool {
    let mut app = AppTemplate::new(Default::default());
    let args = vec!["prog".to_string()];
    app.run(args, || make_ready_future(())) == 0
}

fn return_status_for_func_returning_int() -> bool {
    let mut app = AppTemplate::new(Default::default());
    let args = vec!["prog".to_string()];
    app.run_with_exit_code(args, move || make_ready_future::<i32>(STATUS)) == STATUS
}

type Case = (&'static str, fn() -> bool);

const CASES: &[Case] = &[
    ("app_standard_memory_allocator", app_standard_memory_allocator),
    ("return_0_for_func_returning_void", return_0_for_func_returning_void),
    ("return_status_for_func_returning_int", return_status_for_func_returning_int),
];

fn run_case(name: &str) -> i32 {
    match CASES.iter().find(|(n, _)| *n == name) {
        Some((_, case)) if case() => HELD,
        Some(_) => BROKEN,
        None => UNKNOWN,
    }
}

fn run_all() -> i32 {
    let exe = std::env::current_exe().expect("this program");
    let mut failures = Vec::new();
    for (name, _) in CASES {
        let start = Instant::now();
        let mut child = Command::new(&exe).env(CASE_VAR, name).spawn().expect("the case's process");
        let status = loop {
            if let Some(status) = child.try_wait().expect("the case's status") {
                break Some(status);
            }
            if start.elapsed() > CASE_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let code = status.and_then(|s| s.code());
        println!("case {name}: exit {code:?} (expected {HELD})");
        if code != Some(HELD) {
            failures.push(*name);
        }
    }
    if failures.is_empty() {
        HELD
    } else {
        eprintln!("cases that failed: {failures:?}");
        BROKEN
    }
}

pub(crate) fn main() {
    let code = match std::env::var(CASE_VAR) {
        Ok(name) => run_case(&name),
        Err(_) => run_all(),
    };
    std::process::exit(code);
}
