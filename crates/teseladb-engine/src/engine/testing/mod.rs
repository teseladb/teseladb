//! Test and benchmark support.
//!
//! Tests that need a running reactor are started through [`test_runner`], which boots the
//! engine once on its own thread and executes each test body as a task on its reactor.
//! [`test_case`](mod@test_case) and [`thread_test_case`] provide the macros that declare such tests,
//! [`engine_test`] holds the registry, and [`entry_point`] the `main` for test binaries.
//! [`perf_tests`] is the micro-benchmark framework used by `benches/`, with
//! [`linux_perf_event`] for hardware counters. [`exchanger`], [`random`] and
//! [`on_internal_error`] are small helpers for writing tests.

pub mod engine_test;
pub mod entry_point;
pub mod exchanger;
pub mod linux_perf_event;
pub mod on_internal_error;
pub mod perf_tests;
pub mod random;
pub mod test_case;
pub mod test_runner;
pub mod thread_test_case;
