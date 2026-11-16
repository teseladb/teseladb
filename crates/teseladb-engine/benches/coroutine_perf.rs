//! Microbenchmarks of awaiting in a coroutine: an empty body, a ready future with and
//! without the preemption check, and `maybe_yield`.

#[path = "common/perf_tests.rs"]
mod perf_tests;

use teseladb::engine::core::coroutine::coroutine::WithoutPreemptionCheck;
use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::coroutine::maybe_yield::maybe_yield;

/// The fixture of the coroutine benchmarks; it holds no state.
#[derive(Default)]
#[allow(non_camel_case_types)]
pub struct coroutine_test {}

teseladb::perf_test_c!(coroutine_test, empty, |_s| { Ok(()) });

teseladb::perf_test_c!(coroutine_test, without_preemption_check, |_s| {
    WithoutPreemptionCheck::new(make_ready_future(())).await?;
    Ok(())
});

teseladb::perf_test_c!(coroutine_test, ready, |_s| {
    make_ready_future(()).await?;
    Ok(())
});

teseladb::perf_test_c!(coroutine_test, maybe_yield, |_s| {
    maybe_yield().await;
    Ok(())
});

pub(crate) fn main() {
    empty::register();
    without_preemption_check::register();
    ready::register();
    maybe_yield::register();
    perf_tests::main();
}
