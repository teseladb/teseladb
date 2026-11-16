//! Tests of the coroutine awaiters beyond the common paths: `parallel_for_each` over more
//! pending futures than its inline slots, with failures, dropped unawaited, and interrupted
//! by a panic while launching; `all` with ready and pending futures; the readiness checks of
//! `as_future`, `maybe_yield` and `switch_to`; and the coroutine exception helpers.
#![cfg(target_os = "linux")]

use std::cell::Cell;
use std::future::IntoFuture;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{
    ExceptionPtr, Future, RuntimeError, make_exception_future, make_ready_future,
};
use teseladb::engine::core::preempt::need_preempt;
use teseladb::engine::core::reactor::later;
use teseladb::engine::core::scheduling::current_scheduling_group;
use teseladb::engine::coroutine::all::{All, is_future_v};
use teseladb::engine::coroutine::as_future::{as_future, as_future_without_preemption_check};
use teseladb::engine::coroutine::exception::coroutine::{
    Exception, return_exception, return_exception_ptr,
};
use teseladb::engine::coroutine::maybe_yield::maybe_yield;
use teseladb::engine::coroutine::parallel_for_each::ParallelForEach;
use teseladb::engine::coroutine::switch_to::SwitchTo;
use teseladb::engine_test_case;

engine_test_case!(parallel_for_each_keeps_many_pending_futures, {
    make_coroutine(async move {
        // Eight pending futures: five inline slots and three on the heap.
        let done = Rc::new(Cell::new(0));
        let d = done.clone();
        let p = ParallelForEach::new(0..8, move |_i: i32| {
            let d = d.clone();
            later().then(move |()| d.set(d.get() + 1))
        });
        assert!(!p.await_ready());
        assert!(p.waiting_task().is_none());
        p.await?;
        assert_eq!(done.get(), 8);

        // Ready failures only: the first error is reported without waiting.
        let p = ParallelForEach::new(0..3, |i: i32| -> Future<()> {
            if i == 1 {
                return make_exception_future(ExceptionPtr::new(RuntimeError::new(
                    "ready failure",
                )));
            }
            make_ready_future(())
        });
        assert!(!p.await_ready(), "a failure is not ready");
        let e = p.await.unwrap_err();
        assert!(e.is::<RuntimeError>());

        // A pending failure among pending successes.
        let p = ParallelForEach::new(0..7, |i: i32| -> Future<()> {
            later().then(move |()| -> Future<()> {
                if i == 6 {
                    return make_exception_future(ExceptionPtr::new(RuntimeError::new("late")));
                }
                make_ready_future(())
            })
        });
        assert!(p.await.is_err());

        // Nothing pending and nothing failed: ready at once.
        let p = ParallelForEach::new(0..3, |_i: i32| make_ready_future(()));
        assert!(p.await_ready());
        p.await?;
        Ok(())
    })
});

engine_test_case!(parallel_for_each_dropped_or_interrupted_releases_its_futures, {
    make_coroutine(async move {
        // Dropped without being awaited, with futures in the slots and on the heap.
        let ran = Rc::new(Cell::new(0));
        let r = ran.clone();
        let p = ParallelForEach::new(0..7, move |_i: i32| {
            let r = r.clone();
            later().then(move |()| r.set(r.get() + 1))
        });
        drop(p);

        // A panic while launching drops the futures launched so far.
        let launched = catch_unwind(AssertUnwindSafe(|| {
            let p = ParallelForEach::new(0..4, |i: i32| -> Future<()> {
                if i == 3 {
                    panic!("launch failure");
                }
                later()
            });
            drop(p);
        }));
        assert!(launched.is_err());

        // The dropped futures' continuations still run.
        later().await?;
        later().await?;
        assert_eq!(ran.get(), 7);
        Ok(())
    })
});

engine_test_case!(all_and_readiness_checks, {
    make_coroutine(async move {
        const { assert!(is_future_v::<Future<i32>>()) };

        let ready = All::new((make_ready_future(1), make_ready_future(())));
        assert!(ready.await_ready());
        let (one, ()) = ready.await?;
        assert_eq!(one, 1);

        let pending = All::new((
            make_ready_future(2),
            later().then(|()| Ok::<i32, ExceptionPtr>(3)),
            later(),
        ));
        assert!(!pending.await_ready());
        let (two, three, ()) = pending.await?;
        assert_eq!((two, three), (2, 3));

        let a = as_future(make_ready_future(5));
        // A ready future is returned at once unless preemption is requested, which a build
        // with `--cfg engine_debug` always reports (as as_future_awaiter::await_ready()).
        assert_eq!(a.await_ready(), !need_preempt());
        assert_eq!(a.await.get()?, 5);
        let a = as_future_without_preemption_check(later());
        assert!(!a.await_ready());
        assert!(a.await.get().is_ok());

        let y = maybe_yield().into_future();
        let _ = y.await_ready();
        y.await;

        let here = SwitchTo::new(current_scheduling_group());
        assert!(here.await_ready());
        let prev = here.await;
        assert!(prev == current_scheduling_group());
        Ok(())
    })
});

#[allow(deprecated)]
#[test]
fn exception_helpers_wrap_the_error() {
    use teseladb::engine::coroutine::exception::coroutine::{make_exception, make_exception_from};

    let e = make_exception(ExceptionPtr::new(RuntimeError::new("a")));
    let p: ExceptionPtr = e.into();
    assert!(p.is::<RuntimeError>());
    let e = make_exception_from(RuntimeError::new("b"));
    assert_eq!(e.eptr.downcast_ref::<RuntimeError>().unwrap().0, "b");
    let e = return_exception(RuntimeError::new("c"));
    assert!(e.eptr.is::<RuntimeError>());
    let e = Exception::new(ExceptionPtr::new(RuntimeError::new("d")));
    assert!(ExceptionPtr::from(e).is::<RuntimeError>());
}

engine_test_case!(awaited_exceptions_fail_the_coroutine, {
    make_coroutine(async move {
        let r = return_exception_ptr(ExceptionPtr::new(RuntimeError::new("x"))).await;
        let e = r.unwrap_err();
        assert_eq!(e.downcast_ref::<RuntimeError>().unwrap().0, "x");
        let failing = make_coroutine(async move {
            return_exception(RuntimeError::new("from the body")).await?;
            Ok(1)
        });
        let e = failing.await.unwrap_err();
        assert!(e.is::<RuntimeError>());
        Ok(())
    })
});
