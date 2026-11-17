//! Tests of `ConditionVariable`: signal and broadcast wake waiters in FIFO order, predicate
//! waits, timeouts, breaking the variable, coroutine waits, and errors from predicates.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::time::Duration;

use teseladb::engine::core::condition_variable::{
    BrokenConditionVariable, ConditionVariable, ConditionVariableTimedOut,
};
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future, RuntimeError};
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::timed_out_error::TimedOutError;
use teseladb::engine::core::timer::{Clock, SteadyClockType, Timer};
use teseladb::engine::core::with_timeout::with_timeout;
use teseladb::engine_test_case;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn deadline(d: Duration) -> <SteadyClockType as Clock>::TimePoint {
    <SteadyClockType as Clock>::add(<SteadyClockType as Clock>::now(), d)
}

fn available_count(waiters: &[Future<()>]) -> usize {
    waiters.iter().filter(|f| f.available()).count()
}

/// Asserts that `r` failed with a condition-variable timeout, and with nothing else.
fn expect_cv_timed_out(r: Result<(), ExceptionPtr>) {
    match r {
        Ok(()) => panic!("should not reach"),
        // The specific type first: a condition variable's timeout is a timed_out_error too.
        Err(e) if e.is::<ConditionVariableTimedOut>() => {
            // ok
        }
        Err(e) if e.is::<TimedOutError>() => panic!("should not reach"),
        Err(_) => panic!("should not reach"),
    }
}

engine_test_case!(test_condition_variable_timeout_is_timed_out_error, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let cv = pin!(ConditionVariable::new());
        let r = cv.as_ref().wait_for(ms(1)).get();
        assert!(matches!(r, Err(ref e) if e.is::<TimedOutError>()), "{r:?}");
        Ok(())
    })
});

engine_test_case!(test_condition_variable_signal_consume, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let cv = pin!(ConditionVariable::new());

        cv.signal();
        let f = cv.as_ref().wait();

        assert!(f.available());
        f.get()?;

        let f2 = cv.as_ref().wait();

        assert!(!f2.available());

        cv.signal();

        with_timeout::<SteadyClockType, ()>(deadline(ms(10)), f2).get()?;

        let waiters: Vec<Future<()>> =
            vec![cv.as_ref().wait(), cv.as_ref().wait(), cv.as_ref().wait()];

        assert_eq!(available_count(&waiters), 0usize);

        cv.signal();

        assert_eq!(available_count(&waiters), 1usize);
        // FIFO
        assert!(waiters[0].available());

        cv.broadcast();

        assert_eq!(available_count(&waiters), 3usize);
        for f in waiters {
            f.get()?;
        }
        Ok(())
    })
});

engine_test_case!(test_condition_variable_pred, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let cv = pin!(ConditionVariable::new());
        let ready = Rc::new(Cell::new(false));

        let r = ready.clone();
        // SAFETY: `cv` is pinned in this frame and outlives the wait, which is resolved right here.
        expect_cv_timed_out(unsafe { cv.as_ref().wait_for_pred(ms(100), move || r.get()) }.get());
        // should not affect outcome.
        cv.signal();

        let r = ready.clone();
        // SAFETY: `cv` is pinned in this frame and outlives the wait, which is resolved right here.
        expect_cv_timed_out(unsafe { cv.as_ref().wait_for_pred(ms(100), move || r.get()) }.get());
        Ok(())
    })
});

engine_test_case!(test_condition_variable_signal_break, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let cv = pin!(ConditionVariable::new());

        let waiters: Vec<Future<()>> =
            vec![cv.as_ref().wait(), cv.as_ref().wait(), cv.as_ref().wait()];

        assert_eq!(available_count(&waiters), 0usize);

        cv.broken();

        for f in waiters {
            match f.get() {
                Err(e) if e.is::<BrokenConditionVariable>() => {
                    // ok
                    continue;
                }
                _ => panic!("should not reach"),
            }
        }

        let f = cv.as_ref().wait();
        match f.get() {
            Ok(()) => panic!("should not reach"),
            Err(e) if e.is::<BrokenConditionVariable>() => {
                // ok
            }
            Err(_) => panic!("should not reach"),
        }
        Ok(())
    })
});

engine_test_case!(test_condition_variable_timeout, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let cv = pin!(ConditionVariable::new());

        let f = cv.as_ref().wait_for(ms(100));
        assert!(!f.available());

        sleep(ms(200)).get()?;

        assert!(f.available());

        match f.get() {
            Ok(()) => panic!("should not reach"),
            Err(e) if e.is::<ConditionVariableTimedOut>() => {
                // ok
            }
            Err(_) => panic!("should not reach"),
        }
        Ok(())
    })
});

engine_test_case!(test_condition_variable_pred_wait, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let cv = pin!(ConditionVariable::new());
        let cvp: *const ConditionVariable = &*cv;

        let ready = Rc::new(Cell::new(false));

        let mut t = pin!(Timer::<SteadyClockType>::new());
        let r = ready.clone();
        t.as_mut().set_callback(move || {
            r.set(true);
            // SAFETY: `cv` outlives the timer, cancelled or dropped first.
            unsafe { (*cvp).signal() };
        });
        t.as_mut().arm_in(ms(100));

        let r = ready.clone();
        // SAFETY: `cv` is pinned in this frame and outlives the wait, which is resolved right here.
        unsafe { cv.as_ref().wait_pred(move || r.get()) }.get()?;

        ready.set(false);

        let r = ready.clone();
        // SAFETY: `cv` is pinned in this frame and outlives the wait, which is resolved right here.
        expect_cv_timed_out(unsafe { cv.as_ref().wait_for_pred(ms(10), move || r.get()) }.get());

        ready.set(true);
        cv.signal();

        let r = ready.clone();
        // SAFETY: `cv` is pinned in this frame and outlives the wait, which is resolved right here.
        unsafe { cv.as_ref().wait_for_pred(ms(10), move || r.get()) }.get()?;

        for _ in 0..2 {
            ready.set(false);
            // SAFETY: `cvp` points at `cv`, pinned in this frame; the timer is declared after it, so it is dropped (and its callback can no longer run) before `cv`.
            t.as_mut().set_callback(move || unsafe { (*cvp).broadcast() });
            t.as_mut().arm_periodic(ms(10));

            let r = ready.clone();
            // SAFETY: `cv` is pinned in this frame and outlives the wait, which is resolved right here.
            expect_cv_timed_out(
                unsafe { cv.as_ref().wait_for_pred(ms(300), move || r.get()) }.get(),
            );
            t.as_mut().cancel();
            cv.signal();
        }

        ready.set(true);
        cv.signal();

        let r = ready.clone();
        // SAFETY: `cv` is pinned in this frame and outlives the wait, which is resolved right here.
        unsafe { cv.as_ref().wait_pred(move || r.get()) }.get()?;
        // signal state should remain on
        cv.as_ref().wait().get()?;
        Ok(())
    })
});

engine_test_case!(test_condition_variable_has_waiter, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let cv = pin!(ConditionVariable::new());

        assert!(!cv.has_waiters());

        let f = cv.as_ref().wait();
        assert!(cv.has_waiters());

        cv.signal();
        f.get()?;

        assert!(!cv.has_waiters());
        Ok(())
    })
});

engine_test_case!(test_condition_variable_signal_consume_coroutine, {
    make_coroutine(async move {
        let cv = pin!(ConditionVariable::new());
        let cvp: *const ConditionVariable = &*cv;

        cv.signal();

        with_timeout::<SteadyClockType, ()>(
            deadline(ms(10)),
            // SAFETY: `cv` is pinned in this coroutine and outlives each
            // awaited future.
            make_coroutine(async move { unsafe { Pin::new_unchecked(&*cvp) }.when().await }),
        )
        .await?;

        match with_timeout::<SteadyClockType, ()>(
            deadline(ms(10)),
            // SAFETY: `cv` is pinned in this coroutine and outlives the awaited future.
            make_coroutine(async move { unsafe { Pin::new_unchecked(&*cvp) }.when().await }),
        )
        .await
        {
            Ok(()) => panic!("should not reach"),
            Err(e) if e.is::<ConditionVariableTimedOut>() => panic!("should not reach"),
            Err(e) if e.is::<TimedOutError>() => {
                // ok
            }
            Err(_) => panic!("should not reach"),
        }

        expect_cv_timed_out(
            with_timeout::<SteadyClockType, ()>(
                deadline(Duration::from_secs(10)),
                make_coroutine(async move {
                    // SAFETY: `cv` is pinned in this coroutine and outlives the awaited future.
                    unsafe { Pin::new_unchecked(&*cvp) }.when_for(ms(100)).await
                }),
            )
            .await,
        );
        Ok(())
    })
});

engine_test_case!(test_condition_variable_pred_when, {
    make_coroutine(async move {
        let cv = pin!(ConditionVariable::new());
        let cvp: *const ConditionVariable = &*cv;

        let ready = Rc::new(Cell::new(false));

        let mut t = pin!(Timer::<SteadyClockType>::new());
        let r = ready.clone();
        t.as_mut().set_callback(move || {
            r.set(true);
            // SAFETY: `cv` outlives the timer.
            unsafe { (*cvp).signal() };
        });
        t.as_mut().arm_in(ms(100));

        let r = ready.clone();
        cv.as_ref().when_pred(move || r.get()).await?;

        ready.set(false);

        let r = ready.clone();
        expect_cv_timed_out(cv.as_ref().when_for_pred(ms(10), move || r.get()).await);

        ready.set(true);
        cv.signal();

        let r = ready.clone();
        cv.as_ref().when_for_pred(ms(10), move || r.get()).await?;

        for _ in 0..2 {
            ready.set(false);
            // SAFETY: `cvp` points at `cv`, pinned in this coroutine; the timer is declared after it, so it is dropped (and its callback can no longer run) before `cv`.
            t.as_mut().set_callback(move || unsafe { (*cvp).broadcast() });
            t.as_mut().arm_periodic(ms(10));

            let r = ready.clone();
            expect_cv_timed_out(cv.as_ref().when_for_pred(ms(300), move || r.get()).await);
            t.as_mut().cancel();
            cv.signal();
        }

        ready.set(true);
        cv.signal();

        let r = ready.clone();
        cv.as_ref().when_pred(move || r.get()).await?;
        // signal state should remain on
        cv.as_ref().when().await?;
        Ok(())
    })
});

engine_test_case!(test_condition_variable_when_signal, {
    make_coroutine(async move {
        let cv = pin!(ConditionVariable::new());
        let cvp: *const ConditionVariable = &*cv;

        let ready = Rc::new(Cell::new(false));

        let mut t = pin!(Timer::<SteadyClockType>::new());
        let r = ready.clone();
        t.as_mut().set_callback(move || {
            // SAFETY: `cv` outlives the timer.
            unsafe { (*cvp).signal() };
            r.set(true);
        });
        t.as_mut().arm_in(ms(100));

        cv.as_ref().when().await?;
        // ensure we did not resume before timer ran fully
        assert!(ready.get());
        Ok(())
    })
});

engine_test_case!(test_condition_variable_when_timeout, {
    make_coroutine(async move {
        let cv = pin!(ConditionVariable::new());
        let cvp: *const ConditionVariable = &*cv;

        let ready = Rc::new(Cell::new(false));

        // create "background" fiber
        let r = ready.clone();
        let f = make_coroutine(async move {
            // SAFETY: `cv` outlives the fiber, awaited below.
            match unsafe { Pin::new_unchecked(&*cvp) }.when_for_pred(ms(100), move || r.get()).await
            {
                Ok(()) => {}
                Err(_) => panic!("should not reach"),
            }
            Ok(())
        });

        // ensure we wake up waiter before timeuot
        ready.set(true);
        cv.signal();

        // now busy-spin until the timer should be expired
        while cv.has_waiters() {}

        // he should not have run yet...
        assert!(!f.available());

        // now, if the code is broken, the timer will run once we switch out,
        // and cause the wait to time out, even though it did not. -> assert
        f.await?;
        Ok(())
    })
});

fn throwing_predicate() -> impl FnMut() -> Result<bool, ExceptionPtr> + 'static {
    let mut i = 1;
    move || {
        if i == 0 {
            return Err(ExceptionPtr::new(RuntimeError("Predicate error".to_string())));
        }
        i -= 1;
        Ok(false)
    }
}

// Check that exception from a predicate is propogated to the waiter
engine_test_case!(test_condition_variable_wait_predicate_throws, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let cv = pin!(ConditionVariable::new());

        // SAFETY: `cv` is pinned in this frame and outlives the wait, which is resolved right here.
        let f = unsafe { cv.as_ref().wait_pred(throwing_predicate()) };

        cv.signal();
        assert!(matches!(f.get(), Err(ref e) if e.is::<RuntimeError>()));
        Ok(())
    })
});

// Check that exception from a predicate is propogated to the waiter
engine_test_case!(test_condition_variable_when_predicate_throws, {
    make_coroutine(async move {
        let cv = pin!(ConditionVariable::new());
        let cvp: *const ConditionVariable = &*cv;
        // SAFETY: `cv` outlives the sleep, which fires while the coroutine waits.
        let _ = sleep(ms(100)).then(move |()| unsafe { (*cvp).signal() });
        let r = cv.as_ref().when_pred(throwing_predicate()).await;
        assert!(matches!(r, Err(ref e) if e.is::<RuntimeError>()));
        Ok(())
    })
});
