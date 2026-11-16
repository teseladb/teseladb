//! Tests of engine coroutines: awaiting futures, returning values and errors, scheduling
//! groups, `all!`, `parallel_for_each`, `maybe_yield`, abortable sleeps, generators
//! (buffered and unbuffered), coroutine closures, and wakers of non-engine futures. A
//! coroutine is `make_coroutine(async move { ... })`; awaiting a future is `f.await?` (or the
//! awaiter's own output), returning `v` is `Ok(v)` and an exception is `Err`.

#![cfg(target_os = "linux")]
#![allow(unreachable_code)]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/minstd.rs"]
mod minstd;
#[path = "common/tl_generator.rs"]
mod tl_generator;

use std::cell::Cell;
use std::fmt;
use std::io::Read;
use std::rc::Rc;
use std::time::Duration;

use minstd::{DefaultRandomEngine, UniformIntDistribution};
use teseladb::engine::core::coroutine::coroutine::{Lambda, WithoutPreemptionCheck};
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{
    BrokenPromise, ExceptionPtr, Future, InvalidArgument, LogicError, OutOfRange, Promise,
    RuntimeError, make_exception_future, make_exception_ptr, make_ready_future,
};
use teseladb::engine::core::loop_::{max_concurrent_for_each, parallel_for_each};
use teseladb::engine::core::memory;
use teseladb::engine::core::preempt::need_preempt;
use teseladb::engine::core::reactor::{
    create_scheduling_group_with_shares as create_scheduling_group, destroy_scheduling_group,
    yield_,
};
use teseladb::engine::core::scheduling::{
    SchedulingGroup, current_scheduling_group, default_scheduling_group,
};
use teseladb::engine::core::semaphore::{Semaphore, SemaphoreTimedOut};
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_;
use teseladb::engine::core::with_scheduling_group::with_scheduling_group;
use teseladb::engine::coroutine::as_future::{as_future, as_future_without_preemption_check};
use teseladb::engine::coroutine::exception::coroutine::{
    Exception, return_exception, return_exception_ptr,
};
use teseladb::engine::coroutine::generator::experimental::internal::Fifo;
use teseladb::engine::coroutine::generator::experimental::{BufferSizeT, Generator, Yielder};
use teseladb::engine::coroutine::maybe_yield::maybe_yield;
use teseladb::engine::coroutine::parallel_for_each::ParallelForEach;
use teseladb::engine::coroutine::switch_to::SwitchTo;
use teseladb::engine::testing::random::testing::local_random;
use teseladb::engine_test_case;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// A thrown integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IntException(i32);

impl fmt::Display for IntException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for IntException {}

/// The value of a thrown integer, if `e` is one.
fn int_of(e: &ExceptionPtr) -> Option<i32> {
    e.downcast_ref::<IntException>().map(|v| v.0)
}

fn old_fashioned_continuations() -> Future<i32> {
    yield_().then(|()| make_ready_future(42))
}

fn simple_coroutine() -> Future<i32> {
    make_coroutine(async move {
        yield_().await?;
        Ok(53)
    })
}

fn ready_coroutine() -> Future<i32> {
    make_coroutine(async move { Ok(64) })
}

fn tuple_coroutine() -> Future<(i32, f64)> {
    make_coroutine(async move { Ok((1, 2.)) })
}

fn failing_coroutine() -> Future<i32> {
    make_coroutine(async move {
        yield_().await?;
        Err(make_exception_ptr(IntException(42)))
    })
}

#[inline(never)]
fn throw_exception(x: i32) -> Result<i32, ExceptionPtr> {
    Err(make_exception_ptr(IntException(x)))
}

fn failing_coroutine2() -> Future<i32> {
    make_coroutine(async move {
        yield_().await?;
        throw_exception(17)
    })
}

/// The value of `f`, or the thrown integer it failed with as its value.
fn recover_int(f: Future<i32>) -> Future<i32> {
    make_coroutine(async move {
        assert!(f.failed());
        let e = f.get_exception();
        match int_of(&e) {
            Some(v) => Ok(v),
            None => Err(e),
        }
    })
}

engine_test_case!(test_simple_coroutines, {
    make_coroutine(async move {
        assert_eq!(old_fashioned_continuations().await?, 42);
        assert_eq!(simple_coroutine().await?, 53);
        assert_eq!(ready_coroutine().get()?, 64);
        assert!(tuple_coroutine().await? == (1, 2.));
        assert!(failing_coroutine().await.err().and_then(|e| int_of(&e)) == Some(42));
        assert_eq!(failing_coroutine().then_wrapped(recover_int).await?, 42);
        assert!(failing_coroutine2().await.err().and_then(|e| int_of(&e)) == Some(17));
        assert_eq!(failing_coroutine2().then_wrapped(recover_int).await?, 17);
        Ok(())
    })
});

engine_test_case!(test_abandond_coroutine, {
    make_coroutine(async move {
        let mut f: Option<Future<i32>>;
        {
            let mut p1 = Promise::<()>::new();
            let mut p2 = Promise::<()>::new();
            let mut p3 = Promise::<()>::new();
            let (p2p, p3p): (*mut Promise<()>, *mut Promise<()>) = (&mut p2, &mut p3);
            f = Some(p1.get_future().then(move |()| {
                make_coroutine(async move {
                    // SAFETY: the continuation runs (and takes p3's future) before the
                    // promises leave their scope below.
                    unsafe { (*p2p).set_value(()) };
                    // SAFETY: the continuation takes p3's future before the promises leave their scope below.
                    let fut3 = unsafe { (*p3p).get_future() };
                    assert!(matches!(fut3.await, Err(ref e) if e.is::<BrokenPromise>()));
                    Ok(1)
                })
            }));
            p1.set_value(());
            p2.get_future().await?;
        }
        assert_eq!(f.take().expect("set above").await?, 1);
        Ok(())
    })
});

engine_test_case!(test_scheduling_group, {
    make_coroutine(async move {
        let other_sg = create_scheduling_group(Sstring::from("the other group"), 10.0).await?;
        let body = async {
            let mut p1 = Promise::<()>::new();
            let mut p2 = Promise::<()>::new();

            let mut p1b = Promise::<()>::new();
            let mut p2b = Promise::<()>::new();
            let f1 = p1b.get_future();
            let f2 = p2b.get_future();

            assert!(current_scheduling_group() == default_scheduling_group());
            let other_sg_cap = other_sg;
            let (lf1, lf2) = (p1.get_future(), p2.get_future());
            let f_ret = with_scheduling_group(other_sg, move || {
                let (f1, f2, mut p1, mut p2) = (lf1, lf2, p1b, p2b);
                make_coroutine(async move {
                    // Make a copy in the coroutine before the closure is destroyed.
                    let other_sg = other_sg_cap;
                    assert!(current_scheduling_group() == other_sg);
                    // The scheduling group compares equal to itself.
                    #[allow(clippy::eq_op)]
                    let same = other_sg == other_sg;
                    assert!(same);
                    p1.set_value(());
                    f1.await?;
                    assert!(current_scheduling_group() == other_sg);
                    p2.set_value(());
                    f2.await?;
                    assert!(current_scheduling_group() == other_sg);
                    Ok(42)
                })
            });

            f1.await?;
            assert!(current_scheduling_group() == default_scheduling_group());
            p1.set_value(());
            f2.await?;
            assert!(current_scheduling_group() == default_scheduling_group());
            p2.set_value(());
            assert_eq!(f_ret.await?, 42);
            assert!(current_scheduling_group() == default_scheduling_group());
            Ok::<(), ExceptionPtr>(())
        };
        let ex = body.await.err();
        destroy_scheduling_group(other_sg).await?;
        match ex {
            Some(ex) => Err(ex),
            None => Ok(()),
        }
    })
});

fn switch_to_with_context(sg: SchedulingGroup) -> Future<SchedulingGroup> {
    make_coroutine(async move {
        let new_sg = SwitchTo::new(sg).await;
        assert!(current_scheduling_group() == sg);
        Ok(new_sg)
    })
}

engine_test_case!(test_switch_to, {
    make_coroutine(async move {
        let other_sg0 = create_scheduling_group(Sstring::from("other group 0"), 10.0).await?;
        let other_sg1 = create_scheduling_group(Sstring::from("other group 1"), 10.0).await?;
        let other_sg2 = create_scheduling_group(Sstring::from("other group 2"), 10.0).await?;
        let body = async {
            let base_sg = current_scheduling_group();

            let prev_sg = SwitchTo::new(other_sg0).await;
            assert!(current_scheduling_group() == other_sg0);
            assert!(prev_sg == base_sg);

            let same_sg = SwitchTo::new(other_sg0).await;
            assert!(current_scheduling_group() == other_sg0);
            assert!(same_sg == other_sg0);

            let nested_sg = SwitchTo::new(other_sg1).await;
            assert!(current_scheduling_group() == other_sg1);
            assert!(nested_sg == other_sg0);

            switch_to_with_context(other_sg2).await?;

            SwitchTo::new(base_sg).await;
            assert!(current_scheduling_group() == base_sg);
            Ok::<(), ExceptionPtr>(())
        };
        let ex = body.await.err();

        destroy_scheduling_group(other_sg1).await?;
        destroy_scheduling_group(other_sg0).await?;
        match ex {
            Some(ex) => Err(ex),
            None => Ok(()),
        }
    })
});

fn check_thread_inherits_sg_from_coroutine_frame(expected_sg: SchedulingGroup) -> Future<()> {
    async_(move || {
        assert!(current_scheduling_group() == expected_sg);
    })
}

fn check_coroutine_inherits_sg_from_another_one(expected_sg: SchedulingGroup) -> Future<()> {
    make_coroutine(async move {
        yield_().await?;
        assert!(current_scheduling_group() == expected_sg);
        Ok(())
    })
}

fn switch_to_sg_and_perform_inheriting_checks(
    base_sg: SchedulingGroup,
    new_sg: SchedulingGroup,
) -> Future<()> {
    make_coroutine(async move {
        assert!(current_scheduling_group() == base_sg);
        SwitchTo::new(new_sg).await;
        assert!(current_scheduling_group() == new_sg);

        check_thread_inherits_sg_from_coroutine_frame(new_sg).await?;
        check_coroutine_inherits_sg_from_another_one(new_sg).await?;

        // don't restore previous sg on purpose, expecting it will be restored once coroutine goes out of scope
        Ok(())
    })
}

engine_test_case!(test_switch_to_sg_restoration_and_inheriting, {
    make_coroutine(async move {
        let new_sg = create_scheduling_group(Sstring::from("other group 0"), 10.0).await?;
        let body = async {
            let base_sg = current_scheduling_group();

            switch_to_sg_and_perform_inheriting_checks(base_sg, new_sg).await?;
            // engine automatically restores base_sg once it goes out of coroutine frame
            assert!(current_scheduling_group() == base_sg);

            async_(move || -> Result<(), ExceptionPtr> {
                switch_to_sg_and_perform_inheriting_checks(base_sg, new_sg).get()?;
                assert!(current_scheduling_group() == base_sg);
                Ok(())
            })
            .await?;

            switch_to_sg_and_perform_inheriting_checks(base_sg, new_sg)
                .finally(move || {
                    assert!(current_scheduling_group() == base_sg);
                })
                .await?;
            Ok::<(), ExceptionPtr>(())
        };
        let ex = body.await.err();

        destroy_scheduling_group(new_sg).await?;
        match ex {
            Some(ex) => Err(ex),
            None => Ok(()),
        }
    })
});

engine_test_case!(test_preemption, {
    make_coroutine(async move {
        let x = Rc::new(Cell::new(false));
        let mut preempted: u32 = 0;
        let xc = x.clone();
        let f = yield_().then(move |()| {
            xc.set(true);
        });

        // try to preempt 1000 times. 1 should be enough if not for
        // task queue shaffling in debug mode which may cause co-routine
        // continuation to run first.
        while preempted < 1000 && !x.get() {
            preempted += need_preempt() as u32;
            make_ready_future(()).await?;
        }

        let save_x = x.get();
        // wait for yield() to complete
        f.await?;
        assert!(save_x);
        Ok(())
    })
});

engine_test_case!(test_no_preemption, {
    make_coroutine(async move {
        let x = Rc::new(Cell::new(false));
        let mut preempted: u32 = 0;
        let xc = x.clone();
        let f = yield_().then(move |()| {
            xc.set(true);
        });

        // preemption should not happen, we explicitly asked for continuing if possible
        while preempted < 1000 && !x.get() {
            preempted += need_preempt() as u32;
            WithoutPreemptionCheck(make_ready_future(())).await?;
        }

        let save_x = x.get();
        // wait for yield() to complete
        f.await?;
        assert!(!save_x);
        Ok(())
    })
});

engine_test_case!(test_all_simple, {
    make_coroutine(async move {
        let (a, b) =
            teseladb::all!(|| make_ready_future::<i32>(1), || make_ready_future::<i32>(2)).await?;
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        Ok(())
    })
});

/// Rearranges `v` into the next lexicographic
/// permutation; returns false (and sorts `v`) after the last one.
fn next_permutation<T: Ord>(v: &mut [T]) -> bool {
    if v.len() < 2 {
        return false;
    }
    let mut i = v.len() - 1;
    while i > 0 && v[i - 1] >= v[i] {
        i -= 1;
    }
    if i == 0 {
        v.reverse();
        return false;
    }
    let mut j = v.len() - 1;
    while v[j] <= v[i - 1] {
        j -= 1;
    }
    v.swap(i - 1, j);
    v[i..].reverse();
    true
}

engine_test_case!(test_all_permutations, {
    make_coroutine(async move {
        let mut delays: Vec<Duration> = vec![ms(0), ms(0), ms(2), ms(2), ms(4), ms(6)];
        loop {
            let make_delayed_future_returning_nr = |nr: usize| {
                let delay = delays[nr];
                move || {
                    if delay == ms(0) {
                        make_ready_future::<i32>(nr as i32)
                    } else {
                        sleep(delay).then(move |()| make_ready_future::<i32>(nr as i32))
                    }
                }
            };
            let (a, b, c, d, e, f) = teseladb::all!(
                make_delayed_future_returning_nr(0),
                make_delayed_future_returning_nr(1),
                make_delayed_future_returning_nr(2),
                make_delayed_future_returning_nr(3),
                make_delayed_future_returning_nr(4),
                make_delayed_future_returning_nr(5)
            )
            .await?;
            assert_eq!(a, 0);
            assert_eq!(b, 1);
            assert_eq!(c, 2);
            assert_eq!(d, 3);
            assert_eq!(e, 4);
            assert_eq!(f, 5);
            if !next_permutation(&mut delays) {
                break;
            }
        }
        Ok(())
    })
});

engine_test_case!(test_all_ready_exceptions, {
    make_coroutine(async move {
        let r = teseladb::all!(
            || -> Future<()> { make_exception_future(make_exception_ptr(IntException(1))) },
            || -> Future<()> { make_exception_future(make_exception_ptr(IntException(2))) }
        )
        .await;
        match r {
            Ok(_) => {}
            Err(e) => match int_of(&e) {
                Some(e) => assert!(e == 1 || e == 2),
                None => return Err(e),
            },
        }
        Ok(())
    })
});

engine_test_case!(test_all_nonready_exceptions, {
    make_coroutine(async move {
        let r = teseladb::all!(
            || -> Future<()> {
                make_coroutine(async move {
                    sleep(ms(1)).await?;
                    Err(make_exception_ptr(IntException(1)))
                })
            },
            || -> Future<()> {
                make_coroutine(async move {
                    sleep(ms(1)).await?;
                    Err(make_exception_ptr(IntException(2)))
                })
            }
        )
        .await;
        match r {
            Ok(_) => {}
            Err(e) => match int_of(&e) {
                Some(e) => assert!(e == 1 || e == 2),
                None => return Err(e),
            },
        }
        Ok(())
    })
});

engine_test_case!(test_all_heterogeneous_types, {
    make_coroutine(async move {
        // The value-less future contributes `()` to the tuple.
        let (a, (), b) = teseladb::all!(
            || -> Future<i32> {
                make_coroutine(async move {
                    sleep(ms(1)).await?;
                    Ok(1)
                })
            },
            || -> Future<()> {
                make_coroutine(async move {
                    sleep(ms(1)).await?;
                    Ok(())
                })
            },
            || -> Future<i64> {
                make_coroutine(async move {
                    sleep(ms(1)).await?;
                    Ok(2i64)
                })
            }
        )
        .await?;
        assert_eq!(a, 1);
        assert_eq!(b, 2i64);
        Ok(())
    })
});

engine_test_case!(test_all_noncopyable_types, {
    make_coroutine(async move {
        let (a,) = teseladb::all!(|| -> Future<Box<i32>> {
            make_coroutine(async move { Ok(Box::new(6)) })
        })
        .await?;
        assert_eq!(*a, 6);
        Ok(())
    })
});

engine_test_case!(test_all_throw_in_input_func, {
    make_coroutine(async move {
        let nr_completed = Rc::new(Cell::new(0));
        let mut exception_seen = false;
        let (n1, n3) = (nr_completed.clone(), nr_completed.clone());
        let r = teseladb::all!(
            move || -> Future<i32> {
                make_coroutine(async move {
                    sleep(ms(1)).await?;
                    n1.set(n1.get() + 1);
                    Ok(7)
                })
            },
            || -> Future<i32> {
                // A failure of the function itself (not of its future); futurize_invoke
                // turns it into a failed future.
                make_exception_future(make_exception_ptr(IntException(9)))
            },
            move || -> Future<i32> {
                make_coroutine(async move {
                    sleep(ms(1)).await?;
                    n3.set(n3.get() + 1);
                    Ok(7)
                })
            }
        )
        .await;
        if let Err(e) = r {
            match int_of(&e) {
                Some(n) => {
                    assert_eq!(n, 9);
                    exception_seen = true;
                }
                None => return Err(e),
            }
        }
        assert_eq!(nr_completed.get(), 2);
        assert!(exception_seen);
        Ok(())
    })
});

/// A shared counter of live values, decremented on drop.
struct CounterRef {
    counter: Rc<Cell<i32>>,
}

impl CounterRef {
    fn new(cnt: &Rc<Cell<i32>>) -> Self {
        cnt.set(cnt.get() + 1);
        CounterRef { counter: cnt.clone() }
    }
}

impl Drop for CounterRef {
    fn drop(&mut self) {
        self.counter.set(self.counter.get() - 1);
    }
}

fn check_coroutine_throws<Ex: 'static, T: 'static, F>(fun: F) -> Future<()>
where
    F: Fn(Rc<Cell<i32>>) -> Future<T> + 'static,
{
    make_coroutine(async move {
        // The counter keeps track of the number of alive "counter_ref" objects.
        // If it is not zero then it means that some values were not dropped
        // while quitting the coroutine.
        let counter = Rc::new(Cell::new(0));
        assert!(fun(counter.clone()).await.err().is_some_and(|e| e.is::<Ex>()));
        assert_eq!(counter.get(), 0);
        let c = counter.clone();
        fun(counter.clone())
            .then_wrapped(move |f: Future<T>| {
                assert!(f.failed());
                assert!(f.get_exception().is::<Ex>());
                assert_eq!(c.get(), 0);
            })
            .await?;
        Ok(())
    })
}

engine_test_case!(test_coroutine_exception, {
    make_coroutine(async move {
        check_coroutine_throws::<RuntimeError, i32, _>(|counter| {
            make_coroutine(async move {
                let _ref = CounterRef::new(&counter);
                Err(Exception::new(make_exception_ptr(RuntimeError::new("threw"))).into())
            })
        })
        .await?;
        check_coroutine_throws::<RuntimeError, i32, _>(|counter| {
            make_coroutine(async move {
                let _ref = CounterRef::new(&counter);
                Exception::new(make_exception_ptr(RuntimeError::new("threw"))).await?;
                Ok(42)
            })
        })
        .await?;
        check_coroutine_throws::<LogicError, (), _>(|counter| {
            make_coroutine(async move {
                let _ref = CounterRef::new(&counter);
                return_exception(LogicError::new("threw")).await?;
                Ok(())
            })
        })
        .await?;
        check_coroutine_throws::<IntException, (), _>(|counter| {
            make_coroutine(async move {
                let _ref = CounterRef::new(&counter);
                return_exception(IntException(42)).await?;
                Ok(())
            })
        })
        .await?;
        Ok(())
    })
});

engine_test_case!(test_coroutine_return_exception_ptr, {
    make_coroutine(async move {
        check_coroutine_throws::<RuntimeError, (), _>(|_counter| {
            make_coroutine(async move {
                return_exception(RuntimeError::new("threw")).await?;
                Ok(())
            })
        })
        .await?;
        check_coroutine_throws::<RuntimeError, (), _>(|_counter| {
            make_coroutine(async move {
                let ex = make_exception_ptr(RuntimeError::new("threw"));
                return_exception_ptr(ex).await?;
                Ok(())
            })
        })
        .await?;
        check_coroutine_throws::<RuntimeError, (), _>(|_counter| {
            make_coroutine(async move {
                let ex = make_exception_ptr(RuntimeError::new("threw"));
                return_exception_ptr(ex.clone()).await?;
                Ok(())
            })
        })
        .await?;
        check_coroutine_throws::<IntException, (), _>(|_counter| {
            make_coroutine(async move {
                return_exception_ptr(make_exception_ptr(IntException(3))).await?;
                Ok(())
            })
        })
        .await?;
        Ok(())
    })
});

engine_test_case!(test_maybe_yield, {
    make_coroutine(async move {
        let var = Rc::new(Cell::new(0));
        let done = Rc::new(Cell::new(false));
        let (v, d) = (var.clone(), done.clone());
        let spinner = move || -> Future<()> {
            make_coroutine(async move {
                // increment a variable continuously, but yield so an observer can see it.
                while !d.get() {
                    v.set(v.get() + 1);
                    maybe_yield().await;
                }
                Ok(())
            })
        };
        let spinner_fut = spinner();
        let mut snapshot = var.get();
        for _nr_changes in 0..10 {
            // Try to observe the value changing in time, yield to
            // allow the spinner to advance it.
            while snapshot == var.get() {
                maybe_yield().await;
            }
            snapshot = var.get();
        }
        done.set(true);
        spinner_fut.await?;
        // The test will hang if it doesn't work.
        Ok(())
    })
});

fn simple_generator(max: i32) -> tl_generator::Generator<i32> {
    let mut i = 0;
    tl_generator::Generator::new(move || {
        if i < max {
            let v = i;
            i += 1;
            return Ok(Some(v)); // co_yield i;
        }
        Ok(None)
    })
}

engine_test_case!(generator, {
    make_coroutine(async move {
        // test ability of teseladb::parallel_for_each to deal with move-only views
        let accum = Rc::new(Cell::new(0));
        let a = accum.clone();
        parallel_for_each(simple_generator(10), move |i: i32| {
            a.set(a.get() + i);
            make_ready_future(())
        })
        .await?;
        assert_eq!(accum.get(), 45);

        // test ability of teseladb::max_concurrent_for_each to deal with move-only views
        accum.set(0);
        let a = accum.clone();
        max_concurrent_for_each(simple_generator(10), 10, move |i: i32| {
            a.set(a.get() + i);
            make_ready_future(())
        })
        .await?;
        assert_eq!(accum.get(), 45);
        Ok(())
    })
});

engine_test_case!(test_parallel_for_each_empty, {
    make_coroutine(async move {
        let values: Vec<i32> = Vec::new();
        let count = Rc::new(Cell::new(0));
        let c = count.clone();
        ParallelForEach::new(values, move |_x: i32| {
            c.set(c.get() + 1);
        })
        .await?;
        assert_eq!(count.get(), 0); // the test will hang if it doesn't work.
        Ok(())
    })
});

engine_test_case!(test_parallel_for_each_exception, {
    make_coroutine(async move {
        let values: [i32; 5] = [10, 2, 1, 4, 8];
        let count = Rc::new(Cell::new(0usize));
        // A full-range unsigned draw from the local random engine.
        let dist = UniformIntDistribution::new(0, u64::from(u32::MAX));
        let mut throw_at =
            dist.sample_fn(&mut || u64::from(local_random())) as usize % values.len();
        println!("Will throw at value #{}/{}", throw_at, values.len());
        let c = count.clone();
        let f0 = ParallelForEach::new(values, move |_x: i32| -> Result<(), ExceptionPtr> {
            let n = c.get();
            c.set(n + 1);
            if n == throw_at {
                println!("throw");
                return Err(make_exception_ptr(RuntimeError::new("test")));
            }
            Ok(())
        });
        // An exception thrown by the functor must be propagated
        assert!(matches!(f0.await, Err(ref e) if e.is::<RuntimeError>()));
        // Functor must be called on all values, even if there's an exception
        assert_eq!(count.get(), values.len());

        count.set(0);
        throw_at = dist.sample_fn(&mut || u64::from(local_random())) as usize % values.len();
        println!("Will throw at value #{}/{}", throw_at, values.len());
        let c = count.clone();
        let f1 = ParallelForEach::new(values, move |x: i32| -> Future<()> {
            let c = c.clone();
            make_coroutine(async move {
                sleep(ms(x as u64)).await?;
                let n = c.get();
                c.set(n + 1);
                if n == throw_at {
                    return Err(make_exception_ptr(RuntimeError::new("test")));
                }
                Ok(())
            })
        });
        assert!(matches!(f1.await, Err(ref e) if e.is::<RuntimeError>()));
        assert_eq!(count.get(), values.len());
        Ok(())
    })
});

engine_test_case!(test_parallel_for_each, {
    make_coroutine(async move {
        let values: Vec<i32> = vec![3, 1, 4];
        let sum_of_squares = Rc::new(Cell::new(0));

        let expected = values.iter().fold(0, |sum, &x| sum + x * x);

        // Test all-ready futures
        let s = sum_of_squares.clone();
        ParallelForEach::new(values.clone(), move |x: i32| {
            s.set(s.get() + x * x);
        })
        .await?;
        assert_eq!(sum_of_squares.get(), expected);

        let squaring = |s: Rc<Cell<i32>>| {
            move |x: i32| -> Future<()> {
                let s = s.clone();
                make_coroutine(async move {
                    if x > 1 {
                        sleep(ms(x as u64)).await?;
                    }
                    s.set(s.get() + x * x);
                    Ok(())
                })
            }
        };

        // Test non-ready futures
        sum_of_squares.set(0);
        ParallelForEach::new(values.clone(), squaring(sum_of_squares.clone())).await?;
        assert_eq!(sum_of_squares.get(), expected);

        // Test legacy subrange
        sum_of_squares.set(0);
        ParallelForEach::new(
            values[..values.len() - 1].iter().copied(),
            squaring(sum_of_squares.clone()),
        )
        .await?;
        assert_eq!(sum_of_squares.get(), 10);

        // Also run over a sub-range given by its bounds.
        sum_of_squares.set(0);
        ParallelForEach::new(
            values[0..values.len() - 1].to_vec(),
            squaring(sum_of_squares.clone()),
        )
        .await?;
        assert_eq!(sum_of_squares.get(), 10);
        Ok(())
    })
});

engine_test_case!(test_void_as_future, {
    make_coroutine(async move {
        let mut f: Future<()> = as_future(make_ready_future(())).await;
        assert!(f.available());

        f = as_future(make_exception_future::<()>(make_exception_ptr(RuntimeError::new(
            "exception",
        ))))
        .await;
        assert!(matches!(f.get(), Err(ref e) if e.is::<RuntimeError>()));

        let mut sem = Semaphore::new(0);
        let semp: *mut Semaphore = &mut sem;
        // SAFETY: `sem` outlives the sleep, waited for through sem.wait() below.
        let _ = sleep(ms(1)).then(move |()| unsafe { (*semp).signal(1) });
        f = as_future(sem.wait(1)).await;
        assert!(f.available());

        // SAFETY: `sem` lives in this coroutine frame and the wait is awaited right here.
        f = as_future(unsafe { sem.wait_for(ms(1), 1) }).await;
        assert!(matches!(f.get(), Err(ref e) if e.is::<SemaphoreTimedOut>()));
        Ok(())
    })
});

engine_test_case!(test_void_as_future_without_preemption_check, {
    make_coroutine(async move {
        let mut f: Future<()> = as_future_without_preemption_check(make_ready_future(())).await;
        assert!(f.available());

        f = as_future_without_preemption_check(make_exception_future::<()>(make_exception_ptr(
            RuntimeError::new("exception"),
        )))
        .await;
        assert!(matches!(f.get(), Err(ref e) if e.is::<RuntimeError>()));

        let mut sem = Semaphore::new(0);
        let semp: *mut Semaphore = &mut sem;
        // SAFETY: as in test_void_as_future.
        let _ = sleep(ms(1)).then(move |()| unsafe { (*semp).signal(1) });
        f = as_future_without_preemption_check(sem.wait(1)).await;
        assert!(f.available());

        // SAFETY: as in test_void_as_future.
        f = as_future_without_preemption_check(unsafe { sem.wait_for(ms(1), 1) }).await;
        assert!(matches!(f.get(), Err(ref e) if e.is::<SemaphoreTimedOut>()));
        Ok(())
    })
});

fn gen_exception() -> Future<i32> {
    make_coroutine(async move {
        sleep(ms(1)).await?;
        Err(Exception::new(make_exception_ptr(RuntimeError::new("exception"))).into())
    })
}

engine_test_case!(test_non_void_as_future, {
    make_coroutine(async move {
        let mut f: Future<i32> = as_future(make_ready_future::<i32>(42)).await;
        assert_eq!(f.get()?, 42);

        f = as_future(make_exception_future::<i32>(make_exception_ptr(RuntimeError::new(
            "exception",
        ))))
        .await;
        assert!(matches!(f.get(), Err(ref e) if e.is::<RuntimeError>()));

        let mut p = Promise::<i32>::new();
        let pp: *mut Promise<i32> = &mut p;
        // SAFETY: `p` outlives the sleep, waited for through its future below.
        let _ = sleep(ms(1)).then(move |()| unsafe { (*pp).set_value(314) });
        f = as_future(p.get_future()).await;
        assert_eq!(f.get()?, 314);

        f = as_future(gen_exception()).await;
        assert!(matches!(f.get(), Err(ref e) if e.is::<RuntimeError>()));
        Ok(())
    })
});

engine_test_case!(test_non_void_as_future_without_preemption_check, {
    make_coroutine(async move {
        let mut f: Future<i32> =
            as_future_without_preemption_check(make_ready_future::<i32>(42)).await;
        assert_eq!(f.get()?, 42);

        f = as_future_without_preemption_check(make_exception_future::<i32>(make_exception_ptr(
            RuntimeError::new("exception"),
        )))
        .await;
        assert!(matches!(f.get(), Err(ref e) if e.is::<RuntimeError>()));

        let mut p = Promise::<i32>::new();
        let pp: *mut Promise<i32> = &mut p;
        // SAFETY: as in test_non_void_as_future.
        let _ = sleep(ms(1)).then(move |()| unsafe { (*pp).set_value(314) });
        f = as_future_without_preemption_check(p.get_future()).await;
        assert_eq!(f.get()?, 314);

        f = as_future_without_preemption_check(gen_exception()).await;
        assert!(matches!(f.get(), Err(ref e) if e.is::<RuntimeError>()));
        Ok(())
    })
});

engine_test_case!(test_as_future_preemption, {
    make_coroutine(async move {
        let stop = Rc::new(Cell::new(false));

        let s = stop.clone();
        let get_ready_future = move || -> Future<()> {
            if s.get() {
                make_exception_future(make_exception_ptr(RuntimeError::new("exception")))
            } else {
                make_ready_future(())
            }
        };

        let wait_for_stop = move || -> Future<bool> {
            make_coroutine(async move {
                loop {
                    let f = as_future(get_ready_future()).await;
                    if f.failed() {
                        return Err(Exception::new(f.get_exception()).into());
                    }
                }
            })
        };

        let f0 = wait_for_stop();
        let f0p: *const Future<bool> = &f0;
        let s = stop.clone();
        let set_stop = move || -> Future<()> {
            make_coroutine(async move {
                loop {
                    s.set(true);
                    // SAFETY: `f0` outlives set_stop(), awaited right below.
                    if unsafe { (*f0p).available() } {
                        return Ok(());
                    }
                    maybe_yield().await;
                }
            })
        };

        set_stop().await?;
        assert!(matches!(f0.get(), Err(ref e) if e.is::<RuntimeError>()));
        Ok(())
    })
});

/// The two value containers of a generator: a circular buffer (buffered) and a single
/// optional slot (unbuffered).
trait Container {
    type C<T: 'static>: Fifo<T> + 'static;
    fn make<T: 'static, F>(size: BufferSizeT, body: F) -> Generator<T, Self::C<T>>
    where
        F: AsyncFnOnce(&Yielder<T, Self::C<T>>) -> Result<(), ExceptionPtr> + 'static;
}

/// The buffered container: a circular buffer.
struct BufferedContainer;

impl Container for BufferedContainer {
    type C<T: 'static> = std::collections::VecDeque<T>;
    fn make<T: 'static, F>(size: BufferSizeT, body: F) -> Generator<T, Self::C<T>>
    where
        F: AsyncFnOnce(&Yielder<T, Self::C<T>>) -> Result<(), ExceptionPtr> + 'static,
    {
        Generator::buffered(size, body)
    }
}

/// The unbuffered container: one optional slot.
struct OptionalContainer;

impl Container for OptionalContainer {
    type C<T: 'static> = Option<T>;
    fn make<T: 'static, F>(_size: BufferSizeT, body: F) -> Generator<T, Self::C<T>>
    where
        F: AsyncFnOnce(&Yielder<T, Self::C<T>>) -> Result<(), ExceptionPtr> + 'static,
    {
        Generator::new(body)
    }
}

fn fibonacci_sequence<K: Container>(size: BufferSizeT, count: u32) -> Generator<i32, K::C<i32>> {
    K::make(size, async move |y: &Yielder<i32, K::C<i32>>| {
        let (mut a, mut b): (i32, i32) = (0, 1);
        for i in 0..count {
            if i32::MAX - a < b {
                return Err(make_exception_ptr(OutOfRange::new(format!(
                    "fibonacci[{}] is greater than the largest value of int",
                    i
                ))));
            }
            // Yield `a` and advance the Fibonacci pair.
            let next_b = a + b;
            let v = std::mem::replace(&mut a, std::mem::replace(&mut b, next_b));
            y.yield_value(v).await;
        }
        Ok(())
    })
}

fn test_async_generator_drained<K: Container + 'static>() -> Future<()> {
    make_coroutine(async move {
        let expected_fibs = [0, 1, 1, 2, 3, 5, 8, 13, 21, 34, 55];
        let mut fib = fibonacci_sequence::<K>(BufferSizeT(2), expected_fibs.len() as u32);
        for expected_fib in expected_fibs {
            let actual_fib = fib.next().await?;
            assert!(actual_fib.is_some());
            assert_eq!(actual_fib, Some(expected_fib));
        }
        let sentinel = fib.next().await?;
        assert!(sentinel.is_none());
        Ok(())
    })
}

engine_test_case!(test_async_generator_drained_buffered, {
    test_async_generator_drained::<BufferedContainer>()
});

engine_test_case!(test_async_generator_drained_unbuffered, {
    test_async_generator_drained::<OptionalContainer>()
});

fn test_async_generator_not_drained<K: Container + 'static>() -> Future<()> {
    make_coroutine(async move {
        let mut fib = fibonacci_sequence::<K>(BufferSizeT(2), 42);
        let actual_fib = fib.next().await?;
        assert!(actual_fib.is_some());
        assert_eq!(actual_fib, Some(0));
        Ok(())
    })
}

engine_test_case!(test_async_generator_not_drained_buffered, {
    test_async_generator_not_drained::<BufferedContainer>()
});

engine_test_case!(test_async_generator_not_drained_unbuffered, {
    test_async_generator_not_drained::<OptionalContainer>()
});

/// A value that counts the live instances: a Rust move runs no code, so a moved-from
/// state never exists; only the live value counts.
struct CounterT {
    n: i32,
    count: *mut i32,
}

impl CounterT {
    fn new(n: i32, count: *mut i32) -> Self {
        // SAFETY: `count` points to the live total of the test.
        unsafe { *count += 1 };
        CounterT { n, count }
    }
}

impl Drop for CounterT {
    fn drop(&mut self) {
        if !self.count.is_null() {
            // SAFETY: as in new().
            unsafe { *self.count -= 1 };
        }
    }
}

impl fmt::Display for CounterT {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.n)
    }
}

fn fiddle<K: Container>(
    size: BufferSizeT,
    n: i32,
    total: *mut i32,
) -> Generator<CounterT, K::C<CounterT>> {
    K::make(size, async move |y: &Yielder<CounterT, K::C<CounterT>>| {
        let mut i = 0;
        loop {
            let cur = i;
            i += 1;
            if cur == n {
                return Err(make_exception_ptr(InvalidArgument::new("Eureka from generator!")));
            }
            y.yield_value(CounterT::new(i, total)).await;
        }
    })
}

fn test_async_generator_throws_from_generator<K: Container + 'static>() -> Future<()> {
    make_coroutine(async move {
        let mut total: i32 = 0;
        let totalp: *mut i32 = &mut total;
        let count_to = move |n: u32| -> Future<()> {
            make_coroutine(async move {
                let mut count = fiddle::<K>(BufferSizeT(2), n as i32, totalp);
                for _ in 0..2 * n {
                    count.next().await?;
                }
                Ok(())
            })
        };
        count_to(42)
            .then_wrapped(move |f: Future<()>| {
                assert!(f.failed());
                assert!(f.get_exception().is::<InvalidArgument>());
                // SAFETY: `total` outlives the coroutine, awaited right here.
                assert_eq!(unsafe { *totalp }, 0);
            })
            .await?;
        Ok(())
    })
}

engine_test_case!(test_async_generator_throws_from_generator_buffered, {
    test_async_generator_throws_from_generator::<BufferedContainer>()
});

engine_test_case!(test_async_generator_throws_from_generator_unbuffered, {
    test_async_generator_throws_from_generator::<OptionalContainer>()
});

fn test_async_generator_throws_from_consumer<K: Container + 'static>() -> Future<()> {
    make_coroutine(async move {
        let mut total: i32 = 0;
        let totalp: *mut i32 = &mut total;
        let count_to = move |n: u32| -> Future<()> {
            make_coroutine(async move {
                let mut count = fiddle::<K>(BufferSizeT(2), n as i32, totalp);
                for i in 0..n {
                    if i == n / 2 {
                        return Err(make_exception_ptr(InvalidArgument::new(
                            "Eureka from consumer!",
                        )));
                    }
                    count.next().await?;
                }
                Ok(())
            })
        };
        count_to(42)
            .then_wrapped(move |f: Future<()>| {
                assert!(f.failed());
                assert!(f.get_exception().is::<InvalidArgument>());
                // SAFETY: as in test_async_generator_throws_from_generator.
                assert_eq!(unsafe { *totalp }, 0);
            })
            .await?;
        Ok(())
    })
}

engine_test_case!(test_async_generator_throws_from_consumer_buffered, {
    test_async_generator_throws_from_consumer::<BufferedContainer>()
});

engine_test_case!(test_async_generator_throws_from_consumer_unbuffered, {
    test_async_generator_throws_from_consumer::<OptionalContainer>()
});

unsafe extern "C" {
    fn malloc(size: usize) -> *mut std::ffi::c_void;
    fn free(ptr: *mut std::ffi::c_void);
}

/// A nondeterministic random seed, read from `/dev/urandom`.
fn random_device() -> u32 {
    let mut b = [0u8; 4];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)) {
        Ok(()) => u32::from_ne_bytes(b),
        Err(e) => panic!("random_device: {}", e),
    }
}

/// A uniform draw in [0, 1) made from two draws of the engine (53 bits of precision).
fn uniform_real_0_1(g: &mut DefaultRandomEngine) -> f64 {
    let r = (DefaultRandomEngine::MAX - DefaultRandomEngine::MIN) as f64 + 1.0;
    let mut sum = 0.0;
    let mut tmp = 1.0;
    for _ in 0..2 {
        sum += (g.next() - DefaultRandomEngine::MIN) as f64 * tmp;
        tmp *= r;
    }
    let ret = sum / tmp;
    if ret >= 1.0 { 1.0 - f64::EPSILON / 2.0 } else { ret }
}

engine_test_case!(test_lambda_coroutine_in_continuation, {
    make_coroutine(async move {
        let mut rand_eng = DefaultRandomEngine::new(u64::from(random_device()));
        let n = uniform_real_0_1(&mut rand_eng);
        let sin1 = n.sin(); // avoid optimizer tricks
        let boo = [0u8; 1025]; // bias coroutine size towards 1024 size class
        let func = move |(): ()| -> Future<f64> {
            make_coroutine(async move {
                // Expect coroutine capture to be freed after await without a coroutine closure
                yield_().await?;
                // Try to overwrite recently-release coroutine frame by allocating in similar size-class
                let mut garbage: Vec<*mut u8> = Vec::new();
                for sz in 1024usize..2048 {
                    for _ctr in 0..100 {
                        // SAFETY: malloc has no preconditions; the block is zeroed and freed below.
                        let p = unsafe { malloc(sz) } as *mut u8;
                        // SAFETY: `p` is the `sz`-byte block malloc just returned, owned by this loop until it is freed below.
                        unsafe { std::ptr::write_bytes(p, 0, sz) };
                        garbage.push(p);
                    }
                }
                for p in garbage {
                    // SAFETY: every pointer is a live malloc block.
                    unsafe { free(p as *mut std::ffi::c_void) };
                }
                let _ = &boo;
                Ok(n.sin())
            })
        };
        // `then` takes a `'static` continuation, and a coroutine closure borrows the function
        // object for the statement: the continuation reaches it
        // through a pointer instead.
        let funcp: *const _ = &func;
        // SAFETY: `func` lives in this coroutine frame, unmoved, until the future awaited in
        // this statement (whose continuation is the only user of `funcp`) resolves.
        let sin2 = yield_()
            .then(move |()| unsafe { Lambda::new(&*funcp) }.call::<(), Future<f64>>(()))
            .await?;
        assert_eq!(sin1, sin2);
        Ok(())
    })
});

// Futures that are not engine futures get a
// `std::task::Waker` from the coroutine (see `core::coroutine`). A wake queues the coroutine's
// own task next to the runs its promises hand out, and a waker may outlive the coroutine.

/// Pending on its first poll, after storing a clone of the waker in `slot`; ready on the next.
struct WakeLater {
    slot: Rc<std::cell::RefCell<Option<std::task::Waker>>>,
    polled: bool,
}

impl std::future::Future for WakeLater {
    type Output = ();
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.polled {
            return std::task::Poll::Ready(());
        }
        self.polled = true;
        *self.slot.borrow_mut() = Some(cx.waker().clone());
        std::task::Poll::Pending
    }
}

engine_test_case!(test_coroutine_std_waker_resumes, {
    let slot = Rc::new(std::cell::RefCell::new(None::<std::task::Waker>));
    let waker = slot.clone();
    // Runs after the coroutine below has suspended on WakeLater.
    let woken = yield_().then(move |()| {
        let w = waker.borrow_mut().take();
        assert!(w.is_some());
        if let Some(w) = w {
            w.wake();
        }
    });
    let done = Rc::new(Cell::new(false));
    let d = done.clone();
    make_coroutine(async move {
        WakeLater { slot, polled: false }.await;
        d.set(true);
        woken.await?;
        Ok(())
    })
    .then(move |()| assert!(done.get()))
});

engine_test_case!(test_coroutine_std_waker_with_pending_hand_out, {
    make_coroutine(async move {
        // The awaiter of an unready engine future registers the coroutine with its promise
        // (a hand-out), and a wake is queued in the same poll: the coroutine is queued twice
        // and both runs are accounted for, whichever comes first.
        for wake_first in [false, true] {
            let mut aw = Box::pin(std::future::IntoFuture::into_future(yield_()));
            let mut polls = 0;
            std::future::poll_fn(|cx| {
                polls += 1;
                if wake_first && polls == 1 {
                    cx.waker().wake_by_ref();
                }
                let r = std::future::Future::poll(aw.as_mut(), cx);
                if !wake_first && polls == 1 {
                    cx.waker().wake_by_ref();
                }
                r
            })
            .await?;
            assert!(polls >= 2);
        }
        Ok(())
    })
});

engine_test_case!(test_coroutine_std_waker_outlives_coroutine, {
    let keep = Rc::new(std::cell::RefCell::new(Vec::<std::task::Waker>::new()));
    let k1 = keep.clone();
    let k2 = keep.clone();
    // One coroutine completes after suspending, the other in its first poll; both leave a
    // waker clone behind, which keeps their memory until it is dropped.
    let suspended = make_coroutine(async move {
        std::future::poll_fn(|cx| {
            k1.borrow_mut().push(cx.waker().clone());
            std::task::Poll::Ready(())
        })
        .await;
        yield_().await?;
        Ok(())
    });
    let immediate = make_coroutine(async move {
        std::future::poll_fn(|cx| {
            k2.borrow_mut().push(cx.waker().clone());
            std::task::Poll::Ready(())
        })
        .await;
        Ok(())
    });
    assert!(immediate.available());
    suspended.then(move |()| {
        let wakers = std::mem::take(&mut *keep.borrow_mut());
        assert_eq!(wakers.len(), 2);
        for w in wakers {
            // The coroutine is done: waking it does nothing; dropping the last clone frees it.
            w.wake_by_ref();
            drop(w);
        }
        drop(immediate);
    })
});

thread_local! {
    static SUSPEND_DROPS: Cell<u64> = const { Cell::new(0) };
}

/// A value whose drops are counted in `SUSPEND_DROPS`.
struct DropCounted(u64);

impl Drop for DropCounted {
    fn drop(&mut self) {
        SUSPEND_DROPS.with(|d| d.set(d.get() + 1));
    }
}

/// Awaits `n` promises, each fulfilled by a task that runs after the coroutine suspended on
/// it, so that every await takes the suspend path. Returns the sum of the values received.
async fn suspend_on_promises(n: u64) -> Result<u64, ExceptionPtr> {
    let mut sum = 0;
    for i in 0..n {
        let mut p = Promise::<DropCounted>::new();
        let f = p.get_future();
        let setter = yield_().then(move |()| p.set_value(DropCounted(i)));
        let v = f.await?;
        assert_eq!(v.0, i);
        sum += v.0;
        drop(v);
        setter.await?;
    }
    Ok(sum)
}

engine_test_case!(test_suspended_awaiter_gets_the_value_once, {
    make_coroutine(async move {
        let before = SUSPEND_DROPS.with(Cell::get);
        let sum = suspend_on_promises(100).await?;
        assert_eq!(sum, (0..100).sum::<u64>());
        // Each value is delivered and dropped exactly once.
        assert_eq!(SUSPEND_DROPS.with(Cell::get) - before, 100);
        Ok(())
    })
});

engine_test_case!(test_suspended_awaiter_gets_a_broken_promise, {
    make_coroutine(async move {
        let mut p = Promise::<DropCounted>::new();
        let f = p.get_future();
        let breaker = yield_().then(move |()| drop(p));
        assert!(matches!(f.await, Err(ref e) if e.is::<BrokenPromise>()));
        breaker.await?;
        Ok(())
    })
});

engine_test_case!(test_suspended_awaiter_leaks_nothing, {
    make_coroutine(async move {
        // Warm up the pools so that the measures do not count their refills.
        suspend_on_promises(64).await?;
        let s0 = memory::stats();
        suspend_on_promises(100).await?;
        let s1 = memory::stats();
        suspend_on_promises(10_000).await?;
        let s2 = memory::stats();
        let live_100 = s1.live_objects() as i64 - s0.live_objects() as i64;
        let live_10000 = s2.live_objects() as i64 - s1.live_objects() as i64;
        // Every round allocates and frees its promise node and continuation: nothing stays
        // live, whatever the number of rounds.
        assert!(live_100.abs() <= 2, "live objects after 100 rounds: {live_100}");
        assert!(live_10000.abs() <= 2, "live objects after 10000 rounds: {live_10000}");
        assert!(s2.mallocs() - s1.mallocs() >= 10_000);
        Ok(())
    })
});
