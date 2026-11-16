//! Benchmarks that test raw overhead of almost empty perf tests
//! in all the basic variations.

// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/perf_tests.rs"]
mod perf_tests;

use std::sync::atomic::AtomicI32;

use teseladb::engine::core::future::{Future, as_ready_future, make_ready_future};
use teseladb::engine::core::reactor::yield_;
use teseladb::engine::coroutine::maybe_yield::maybe_yield;
use teseladb::engine::testing::perf_tests::perf_tests::do_not_optimize;

/// `volatile int sink`
static SINK: AtomicI32 = AtomicI32::new(0);

const ITER_COUNT: usize = 100;

/// The fixture of the runner's own benchmarks; it holds no state.
#[derive(Default)]
#[allow(non_camel_case_types)]
pub struct fixture {}

/// A read of `sink`: a volatile read, which the compiler can neither drop nor
/// fold (a relaxed load of an atomic that is never written is folded into a constant).
#[inline(always)]
fn sink() -> i32 {
    // SAFETY: the pointer is the atomic's own, valid and aligned for the whole program, and
    // nothing writes it.
    unsafe { SINK.as_ptr().read_volatile() }
}

/// A counted loop with the increment after the body.
/// (`for i in 0..ITER_COUNT` increments in `Range::next`, before the body, so the old and the
/// new value of `i` are both live across `do_not_optimize` and cost a register copy.)
#[inline(always)]
fn loop_() -> usize {
    let mut i: usize = 0;
    while i < ITER_COUNT {
        do_not_optimize(&i);
        i += 1;
    }
    ITER_COUNT
}

teseladb::perf_test!(perf_tests, test_simple_1, -> () { do_not_optimize(&sink()); });

teseladb::perf_test!(perf_tests, test_simple_n, -> usize { loop_() });

teseladb::perf_test!(perf_tests, test_ready_async_1, -> Future<()> {
    // `now()` is a ready future.
    make_ready_future(())
});

teseladb::perf_test!(perf_tests, test_ready_async_n, -> Future<usize> { as_ready_future(loop_()) });

teseladb::perf_test!(perf_tests, test_unready_async_1, -> Future<()> { yield_() });

teseladb::perf_test!(perf_tests, test_unready_async_n, -> Future<usize> {
    let i = loop_();
    yield_().then(move |()| make_ready_future(i))
});

teseladb::perf_test_f!(fixture, test_fixture_1, |_s| -> () {
    do_not_optimize(&sink());
});

teseladb::perf_test_f!(fixture, test_fixture_n, |_s| -> usize { loop_() });

teseladb::perf_test_c!(fixture, test_coro_1, |_s| {
    // without the next line, compiler will optimize away the coroutine nature of
    // this function and compile/inline it as a regular function
    maybe_yield().await;
    Ok(())
});

teseladb::perf_test_cn!(fixture, test_coro_n, |_s| {
    maybe_yield().await;
    Ok(loop_())
});

pub(crate) fn main() {
    test_simple_1::register();
    test_simple_n::register();
    test_ready_async_1::register();
    test_ready_async_n::register();
    test_unready_async_1::register();
    test_unready_async_n::register();
    test_fixture_1::register();
    test_fixture_n::register();
    test_coro_1::register();
    test_coro_n::register();
    perf_tests::main();
}
