//! Microbenchmarks of the future combinators: `parallel_for_each` (plain and in
//! coroutines) and continuation chains that pass values or errors with `then` or `await`.

// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/perf_tests.rs"]
mod perf_tests;

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{
    ExceptionPtr, Future, Promise, RuntimeError, make_ready_future,
};
use teseladb::engine::core::loop_;
use teseladb::engine::core::reactor::yield_;
use teseladb::engine::coroutine::as_future::as_future;
use teseladb::engine::coroutine::exception::coroutine::return_exception_ptr;
use teseladb::engine::coroutine::parallel_for_each::ParallelForEach as CoroutineParallelForEach;
use teseladb::engine::testing::perf_tests::perf_tests::do_not_optimize;

const MAX_RANGE_SIZE: i32 = 100;

/// `struct parallel_for_each`
#[allow(non_camel_case_types)]
pub struct parallel_for_each {
    empty_range: Vec<i32>,
    range: Vec<i32>,
    value: i32,
}

impl Default for parallel_for_each {
    fn default() -> Self {
        parallel_for_each {
            empty_range: Vec::new(),
            range: (1..MAX_RANGE_SIZE).collect(),
            value: 0,
        }
    }
}

#[inline(never)]
fn immediate(v: i32, vs: &mut i32) -> Future<()> {
    *vs += v;
    make_ready_future(())
}

#[inline(never)]
fn suspend(v: i32, vs: &mut i32) -> Future<()> {
    *vs += v;
    yield_()
}

const SCALE: i32 = 32;

/// `struct chain`
#[derive(Default)]
#[allow(non_camel_case_types)]
pub struct chain {
    value: i32,
}

/// An empty value carried through the chains of futures and promises.
#[derive(Clone, Copy)]
struct Nullopt;

impl chain {
    #[inline(never)]
    fn do_then(&mut self, w: Future<()>, depth: i32) -> Future<()> {
        if depth > 0 {
            let this: *mut Self = self;
            self.do_then(w, depth - 1).then(move |()| {
                // SAFETY: the fixture outlives the run that waits for this chain.
                let me = unsafe { &mut *this };
                me.value += 1;
                do_not_optimize(&me.value);
                make_ready_future(())
            })
        } else {
            w
        }
    }

    #[inline(never)]
    fn do_await(&mut self, w: Future<()>, depth: i32) -> Future<()> {
        let this: *mut Self = self;
        make_coroutine(async move {
            // SAFETY: as above.
            let me = unsafe { &mut *this };
            if depth > 0 {
                me.do_await(w, depth - 1).await?;
                me.value += 1;
                do_not_optimize(&me.value);
            } else {
                w.await?;
            }
            Ok(())
        })
    }

    #[inline(never)]
    fn do_await_as_future(&mut self, w: Future<Nullopt>, depth: i32) -> Future<Nullopt> {
        let this: *mut Self = self;
        make_coroutine(async move {
            // SAFETY: as above.
            let me = unsafe { &mut *this };
            if depth > 0 {
                let fut = as_future(me.do_await_as_future(w, depth - 1)).await;
                if fut.failed() {
                    match return_exception_ptr(fut.get_exception()).await? {}
                }
                me.value += 1;
                do_not_optimize(&me.value);
                fut.get()
            } else {
                let fut = as_future(w).await;
                if fut.failed() {
                    match return_exception_ptr(fut.get_exception()).await? {}
                }
                fut.get()
            }
        })
    }
}

fn runtime_error() -> ExceptionPtr {
    ExceptionPtr::new(RuntimeError(String::new()))
}

teseladb::perf_test_f!(parallel_for_each, empty, |s| -> Future<()> {
    loop_::parallel_for_each(s.empty_range.iter().copied(), |_: i32| -> Future<()> {
        std::process::abort()
    })
});

teseladb::perf_test_f!(chain, then_value, |s| -> Future<usize> {
    let mut p = Promise::<()>::new();
    let f = s.do_then(p.get_future(), SCALE);
    p.set_value(());
    f.then(|()| make_ready_future(SCALE as usize))
});

teseladb::perf_test_f!(chain, await_value, |s| -> Future<usize> {
    let mut p = Promise::<()>::new();
    let f = s.do_await(p.get_future(), SCALE);
    p.set_value(());
    f.then(|()| make_ready_future(SCALE as usize))
});

teseladb::perf_test_f!(chain, await_value_as_future, |s| -> Future<usize> {
    let mut p = Promise::<Nullopt>::new();
    let f = s.do_await_as_future(p.get_future(), SCALE);
    p.set_value(Nullopt);
    f.then(|_: Nullopt| make_ready_future(SCALE as usize))
});

teseladb::perf_test_f!(chain, then_exception, |s| -> Future<usize> {
    let mut p = Promise::<()>::new();
    let f = s.do_then(p.get_future(), SCALE);
    p.set_exception(runtime_error());
    f.then_wrapped(|mut x: Future<()>| {
        x.ignore_ready_future();
        make_ready_future(SCALE as usize)
    })
});

teseladb::perf_test_f!(chain, await_exception, |s| -> Future<usize> {
    let mut p = Promise::<()>::new();
    let f = s.do_await(p.get_future(), SCALE);
    p.set_exception(runtime_error());
    f.then_wrapped(|mut x: Future<()>| {
        x.ignore_ready_future();
        make_ready_future(SCALE as usize)
    })
});

teseladb::perf_test_f!(chain, await_exception_as_future, |s| -> Future<usize> {
    let mut p = Promise::<Nullopt>::new();
    let f = s.do_await_as_future(p.get_future(), SCALE);
    p.set_exception(runtime_error());
    f.then_wrapped(|mut x: Future<Nullopt>| {
        x.ignore_ready_future();
        make_ready_future(SCALE as usize)
    })
});

/// A benchmark that runs `parallel_for_each` over the first `N` elements of the fixture's
/// range with `F(v, value)` as the body, then keeps `value` alive and returns `N`;
/// `N = 0` means the whole range.
macro_rules! pfe_test {
    ($name:ident, $f:ident, $n:expr) => {
        teseladb::perf_test_f!(parallel_for_each, $name, |s| -> Future<usize> {
            let n: usize = if $n == 0 { s.range.len() } else { $n };
            let this: *mut parallel_for_each = s;
            // The range is iterated in place (no copy per run).
            // SAFETY: the fixture's range outlives the loop.
            let items = unsafe { std::slice::from_raw_parts(s.range.as_ptr(), n) }.iter().copied();
            // SAFETY: the fixture outlives the run that waits for the loop.
            loop_::parallel_for_each(items, move |v: i32| $f(v, unsafe { &mut (*this).value }))
                .then(move |()| {
                    // SAFETY: as above.
                    do_not_optimize(unsafe { &(*this).value });
                    make_ready_future(n)
                })
        });
    };
}

pfe_test!(immediate_1, immediate, 1);
pfe_test!(immediate_2, immediate, 2);
pfe_test!(immediate_10, immediate, 10);
pfe_test!(immediate_100, immediate, 0);
pfe_test!(suspend_1, suspend, 1);
pfe_test!(suspend_2, suspend, 2);
pfe_test!(suspend_10, suspend, 10);
pfe_test!(suspend_100, suspend, 0);

teseladb::perf_test_c!(parallel_for_each, cor_empty, |s| {
    // SAFETY: the fixture outlives the coroutine.
    let s = unsafe { &mut *s };
    loop_::parallel_for_each(s.empty_range.iter().copied(), |_: i32| -> Future<()> {
        std::process::abort()
    })
    .await?;
    Ok(())
});

/// The coroutine forms: await `parallel_for_each(...)` (or
/// `teseladb::engine::coroutine::parallel_for_each` when `$coro` is true), then return `n`.
macro_rules! cor_pfe_test {
    ($name:ident, $f:ident, $n:expr, $coro:expr) => {
        teseladb::perf_test_cn!(parallel_for_each, $name, |s| {
            // SAFETY: the fixture outlives the coroutine.
            let s = unsafe { &mut *s };
            let n: usize = if $n == 0 { s.range.len() } else { $n };
            let this: *mut parallel_for_each = s;
            // The range is iterated in place (no copy per run).
            // SAFETY: the fixture's range outlives the loop.
            let items = unsafe { std::slice::from_raw_parts(s.range.as_ptr(), n) }.iter().copied();
            // SAFETY: as above.
            let func = move |v: i32| $f(v, unsafe { &mut (*this).value });
            if $coro {
                CoroutineParallelForEach::new(items, func).await?;
            } else {
                loop_::parallel_for_each(items, func).await?;
            }
            do_not_optimize(&s.value);
            Ok(n)
        });
    };
}

cor_pfe_test!(cor_immediate_1, immediate, 1, false);
cor_pfe_test!(cor_immediate_2, immediate, 2, false);
cor_pfe_test!(cor_immediate_10, immediate, 10, false);
cor_pfe_test!(cor_immediate_100, immediate, 0, false);
cor_pfe_test!(cor_suspend_1, suspend, 1, false);
cor_pfe_test!(cor_suspend_2, suspend, 2, false);
cor_pfe_test!(cor_suspend_10, suspend, 10, false);
cor_pfe_test!(cor_suspend_100, suspend, 0, false);

teseladb::perf_test_c!(parallel_for_each, cor_pfe_empty, |s| {
    // SAFETY: the fixture outlives the coroutine.
    let s = unsafe { &mut *s };
    CoroutineParallelForEach::new(s.empty_range.iter().copied(), |_: i32| -> Future<()> {
        std::process::abort()
    })
    .await?;
    Ok(())
});

cor_pfe_test!(cor_pfe_immediate_1, immediate, 1, true);
cor_pfe_test!(cor_pfe_immediate_2, immediate, 2, true);
cor_pfe_test!(cor_pfe_immediate_10, immediate, 10, true);
cor_pfe_test!(cor_pfe_immediate_100, immediate, 0, true);
cor_pfe_test!(cor_pfe_suspend_1, suspend, 1, true);
cor_pfe_test!(cor_pfe_suspend_2, suspend, 2, true);
cor_pfe_test!(cor_pfe_suspend_10, suspend, 10, true);
cor_pfe_test!(cor_pfe_suspend_100, suspend, 0, true);

pub(crate) fn main() {
    empty::register();
    then_value::register();
    await_value::register();
    await_value_as_future::register();
    then_exception::register();
    await_exception::register();
    await_exception_as_future::register();
    immediate_1::register();
    immediate_2::register();
    immediate_10::register();
    immediate_100::register();
    suspend_1::register();
    suspend_2::register();
    suspend_10::register();
    suspend_100::register();
    cor_empty::register();
    cor_immediate_1::register();
    cor_immediate_2::register();
    cor_immediate_10::register();
    cor_immediate_100::register();
    cor_suspend_1::register();
    cor_suspend_2::register();
    cor_suspend_10::register();
    cor_suspend_100::register();
    cor_pfe_empty::register();
    cor_pfe_immediate_1::register();
    cor_pfe_immediate_2::register();
    cor_pfe_immediate_10::register();
    cor_pfe_immediate_100::register();
    cor_pfe_suspend_1::register();
    cor_pfe_suspend_2::register();
    cor_pfe_suspend_10::register();
    cor_pfe_suspend_100::register();
    perf_tests::main();
}
