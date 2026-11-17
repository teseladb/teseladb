//! Coroutine awaiters dropped while their coroutine is suspended on them (a hand-written
//! race inside a coroutine lets the loser go): engine futures, `all!`, `ParallelForEach` and
//! a generator's `next()`; the generator keeps working afterwards. Also generator buffering
//! edge cases, `swap`, the task trace through suspended coroutines, `WithoutPreemptionCheck`
//! accessors, and the panic of an engine future awaited outside an engine coroutine.

#![cfg(target_os = "linux")]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::{Future as StdFuture, IntoFuture};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::Duration;

use teseladb::engine::core::coroutine::coroutine::WithoutPreemptionCheck;
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{
    ExceptionPtr, Future, Promise, RuntimeError, make_exception_ptr, make_ready_future,
};
use teseladb::engine::core::reactor::later;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::coroutine::generator::experimental::{BufferSizeT, Generator, Yielder};
use teseladb::engine::coroutine::parallel_for_each::ParallelForEach;
use teseladb::engine::util::backtrace::current_tasktrace;
use teseladb::engine_test_case;

/// Which side of a [`Race`] finished first.
enum Won<A, B> {
    First(A),
    Second(B),
}

/// Polls two futures, first `a` then `b`, and completes with the first one ready; the other
/// one is dropped right then, inside the coroutine's run, while it may be suspended.
struct Race<A, B> {
    a: Option<Pin<Box<A>>>,
    b: Option<Pin<Box<B>>>,
}

fn race<A: StdFuture, B: StdFuture>(a: A, b: B) -> Race<A, B> {
    Race { a: Some(Box::pin(a)), b: Some(Box::pin(b)) }
}

impl<A: StdFuture, B: StdFuture> StdFuture for Race<A, B> {
    type Output = Won<A::Output, B::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(a) = self.a.as_mut()
            && let Poll::Ready(v) = a.as_mut().poll(cx)
        {
            self.a = None;
            self.b = None;
            return Poll::Ready(Won::First(v));
        }
        if let Some(b) = self.b.as_mut()
            && let Poll::Ready(v) = b.as_mut().poll(cx)
        {
            self.b = None;
            self.a = None;
            return Poll::Ready(Won::Second(v));
        }
        Poll::Pending
    }
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

// An engine future whose promise is never fulfilled loses a race to a sleep: its awaiter,
// suspended and linked to the promise, is dropped and gives the coroutine's task back. The
// promise is fulfilled later with nothing waiting; the coroutine completes and is freed.
engine_test_case!(engine_future_awaiter_dropped_while_suspended, {
    make_coroutine(async move {
        let mut p = Promise::<i32>::new();
        let f = p.get_future();
        let r = race(f.into_future(), sleep(ms(5)).into_future()).await;
        assert!(matches!(r, Won::Second(Ok(()))));
        p.set_value(7);
        // The same with the promise broken after the race.
        let mut p = Promise::<i32>::new();
        let f = p.get_future();
        let r = race(f.into_future(), later().into_future()).await;
        assert!(matches!(r, Won::Second(Ok(()))));
        drop(p);
        // An awaiter dropped without ever being polled.
        let mut p = Promise::<i32>::new();
        let unpolled = p.get_future().into_future();
        drop(unpolled);
        p.set_value(1);
        // The winner is the first: the second one is dropped suspended.
        let r = race(later().into_future(), sleep(ms(50)).into_future()).await;
        assert!(matches!(r, Won::First(Ok(()))));
        Ok(())
    })
});

// `all!` dropped while it waits on its second future: the first one has resolved, the task
// is registered on the second, which is withdrawn.
engine_test_case!(all_dropped_while_suspended, {
    make_coroutine(async move {
        let mut p = Promise::<i32>::new();
        let f = Rc::new(RefCell::new(Some(p.get_future())));
        let f2 = f.clone();
        let all = teseladb::all!(|| make_ready_future::<i32>(1), move || f2
            .borrow_mut()
            .take()
            .expect("future"));
        let r = race(all, sleep(ms(5)).into_future()).await;
        assert!(matches!(r, Won::Second(Ok(()))));
        p.set_value(2);
        assert!(f.borrow().is_none());
        // The same, resolved later while polled again in between: still the right values.
        let mut p = Promise::<i32>::new();
        let fut = p.get_future();
        let mut slot = Some(fut);
        let all =
            teseladb::all!(move || slot.take().expect("future"), || make_ready_future::<i32>(3));
        let r = race(all, async move {
            later().await?;
            p.set_value(4);
            sleep(ms(1000)).await
        })
        .await;
        match r {
            Won::First(Ok((a, b))) => assert_eq!((a, b), (4, 3)),
            _ => panic!("all! did not win"),
        }
        Ok(())
    })
});

// A `ParallelForEach` dropped while it waits for its last pending future; the bodies whose
// futures were pending still finish, nobody waiting for them.
engine_test_case!(parallel_for_each_dropped_while_suspended, {
    make_coroutine(async move {
        let done = Rc::new(Cell::new(0));
        let d = done.clone();
        let pfe = ParallelForEach::new(0..7u64, move |i: u64| {
            let d = d.clone();
            sleep(ms(if i == 6 { 30 } else { 1 + i })).then(move |()| {
                d.set(d.get() + 1);
                make_ready_future(())
            })
        });
        let r = race(pfe, sleep(ms(15)).into_future()).await;
        assert!(matches!(r, Won::Second(Ok(()))));
        assert!(done.get() < 7);
        sleep(ms(40)).await?;
        assert_eq!(done.get(), 7);
        // Polled again while waiting (the race wakes it for the other side), then it wins
        // with the error of one of the bodies.
        let pfe = ParallelForEach::new(0..6u64, move |i: u64| {
            sleep(ms(2 * i)).then(move |()| {
                if i == 3 {
                    return teseladb::engine::core::future::make_exception_future(
                        make_exception_ptr(RuntimeError::new("body 3")),
                    );
                }
                make_ready_future(())
            })
        });
        let ticker = async move {
            for _ in 0..100 {
                sleep(ms(1)).await?;
            }
            Ok::<(), ExceptionPtr>(())
        };
        match race(pfe, make_coroutine(ticker).into_future()).await {
            Won::First(Err(e)) => assert!(e.to_string().contains("body 3"), "{e}"),
            _ => panic!("parallel_for_each did not fail first"),
        }
        Ok(())
    })
});

/// An unbuffered generator of `0..n`, sleeping `pause` before each value.
fn slow_numbers(n: i32, pause: u64) -> Generator<i32> {
    Generator::new(async move |y: &Yielder<i32, Option<i32>>| {
        for i in 0..n {
            sleep(ms(pause)).await?;
            y.yield_value(i).await;
        }
        Ok(())
    })
}

// The consumer's next() loses a race to a sleep shorter than the body's pause: the awaiter is
// dropped suspended and withdraws its wait; the body goes on and its value waits for the next
// next(), so the whole sequence is still delivered in order.
engine_test_case!(generator_next_dropped_while_suspended, {
    make_coroutine(async move {
        let mut g = slow_numbers(4, 20);
        let mut got = Vec::new();
        match race(g.next(), sleep(ms(2)).into_future()).await {
            Won::Second(r) => r?,
            Won::First(_) => panic!("the value came before the sleep"),
        }
        // A next() dropped without being polled withdraws its wait too.
        drop(g.next());
        while let Some(v) = g.next().await? {
            got.push(v);
        }
        assert_eq!(got, vec![0, 1, 2, 3]);
        // Dropping a generator whose body sleeps destroys the body while it is suspended on
        // the sleep: next() resumes the body, which goes to sleep, and loses to later().
        let mut g = slow_numbers(10, 5);
        assert_eq!(g.next().await?, Some(0));
        match race(g.next(), later().into_future()).await {
            Won::Second(r) => r?,
            Won::First(_) => panic!("the value came before later()"),
        }
        drop(g);
        Ok(())
    })
});

// A buffered generator whose body fills its buffer without waiting, and fails right after its
// first value while the consumer is not waiting: the consumer gets every value, then the
// error. A buffer of one emptied after the body finished, and a buffer emptied while the body
// sleeps (no free-space wait to wake). And `swap` exchanges two generators.
engine_test_case!(generator_buffer_edges_and_swap, {
    make_coroutine(async move {
        let mut g = Generator::<i32, VecDeque<i32>>::buffered(
            BufferSizeT(4),
            async move |y: &Yielder<i32, VecDeque<i32>>| {
                y.yield_value(1).await;
                y.yield_value(2).await;
                y.yield_value(3).await;
                Err(make_exception_ptr(RuntimeError::new("after 3")))
            },
        );
        assert_eq!(g.next().await?, Some(1));
        assert_eq!(g.next().await?, Some(2));
        assert_eq!(g.next().await?, Some(3));
        let e = g.next().await.expect_err("the body's error");
        assert!(e.to_string().contains("after 3"), "{e}");

        let mut one = Generator::<i32, VecDeque<i32>>::buffered(
            BufferSizeT(1),
            async move |y: &Yielder<i32, VecDeque<i32>>| {
                y.yield_value(10).await;
                Ok(())
            },
        );
        assert_eq!(one.next().await?, Some(10));
        assert_eq!(one.next().await?, None);

        let mut sleepy = Generator::<i32, VecDeque<i32>>::buffered(
            BufferSizeT(1),
            async move |y: &Yielder<i32, VecDeque<i32>>| {
                y.yield_value(20).await;
                sleep(ms(5)).await?;
                y.yield_value(21).await;
                Ok(())
            },
        );
        assert_eq!(sleepy.next().await?, Some(20));
        assert_eq!(sleepy.next().await?, Some(21));
        assert_eq!(sleepy.next().await?, None);

        let mut a = slow_numbers(2, 0);
        let mut b = Generator::new(async move |y: &Yielder<i32, Option<i32>>| {
            y.yield_value(100).await;
            Ok(())
        });
        a.swap(&mut b);
        assert_eq!(a.next().await?, Some(100));
        assert_eq!(a.next().await?, None);
        assert_eq!(b.next().await?, Some(0));
        assert_eq!(b.next().await?, Some(1));
        assert_eq!(b.next().await?, None);
        Ok(())
    })
});

// The task trace taken in a continuation that a coroutine awaits, itself awaited by another
// coroutine, lists the waiting coroutines.
engine_test_case!(tasktrace_through_suspended_coroutines, {
    make_coroutine(async move {
        let inner = make_coroutine(async move {
            let trace =
                later().then(|()| make_ready_future(current_tasktrace().to_string())).await?;
            Ok(trace)
        });
        let trace = inner.await?;
        assert!(trace.matches("--------").count() >= 2, "{trace}");
        assert!(trace.contains("CoroutinePromise"), "{trace}");
        Ok(())
    })
});

// WithoutPreemptionCheck derefs to the future it wraps, and awaits it.
engine_test_case!(without_preemption_check_accessors, {
    make_coroutine(async move {
        let mut w = WithoutPreemptionCheck::new(make_ready_future(5));
        assert!(w.available());
        assert!(!(*w).failed());
        let inner: &mut Future<i32> = &mut w;
        assert!(inner.available());
        assert_eq!(w.await?, 5);
        Ok(())
    })
});

// An engine future awaited by an executor that is not an engine coroutine panics on the first
// poll that would suspend.
engine_test_case!(await_outside_engine_coroutine_panics, {
    let mut p = Promise::<()>::new();
    let mut aw = Box::pin(p.get_future().into_future());
    let mut cx = Context::from_waker(std::task::Waker::noop());
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = aw.as_mut().poll(&mut cx);
    }));
    let msg = r.expect_err("the poll panics");
    let msg = msg
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| msg.downcast_ref::<String>().cloned());
    assert!(msg.unwrap_or_default().contains("outside a engine coroutine"));
    drop(aw);
    p.set_value(());
    make_ready_future(())
});

// `is_future_v` tells futures apart at compile time.
#[test]
fn is_future_v_of_a_future() {
    const IS: bool = teseladb::engine::coroutine::all::is_future_v::<Future<i32>>();
    assert!(IS);
}
