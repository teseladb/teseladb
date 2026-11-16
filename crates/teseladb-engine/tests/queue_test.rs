//! Tests of `Queue`: pops that wait for pushes, pushes that wait for room, and aborting a
//! queue with waiters on either side.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

use teseladb::engine::core::future::{ExceptionPtr, Future, RuntimeError, make_ready_future};
use teseladb::engine::core::loop_::{StopIteration, repeat};
use teseladb::engine::core::queue::Queue;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::thread::async_;
use teseladb::engine::core::timer::Timer;
use teseladb::engine::util::log::Logger;
use teseladb::engine_test_case;

static TESTLOG: LazyLock<std::pin::Pin<Box<Logger>>> =
    LazyLock::new(|| Logger::new("testlog").expect("logger registered once"));

engine_test_case!(test_queue_pop_eventually, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        // The locals are boxed so that the callbacks can refer to them by address.
        struct Locals {
            q: Queue<i32>,
            pushed: Cell<i32>,
            pusher_done: Cell<bool>,
            popped: Cell<i32>,
            popper_done: Cell<bool>,
            stop: Cell<i32>,
            stop_timer: Timer,
        }
        let l: *mut Locals = Box::into_raw(Box::new(Locals {
            q: Queue::new(100),
            pushed: Cell::new(0),
            pusher_done: Cell::new(false),
            popped: Cell::new(0),
            popper_done: Cell::new(false),
            stop: Cell::new(0),
            stop_timer: Timer::new(),
        }));
        let test_duration = Duration::from_millis(1);
        let watchdog_duration = Duration::from_secs(10);

        // SAFETY: `l` is freed at the end of this function,
        // after the timer is cancelled and both loops are done; the locals stay in their box,
        // so the timer and the queue do not move while armed or waited on.
        unsafe {
            std::pin::Pin::new_unchecked(&mut (*l).stop_timer).set_callback(move || {
                let l = &mut *l;
                teseladb::log_debug!(TESTLOG, "stop_timer: pushed={} pusher_done={} popped={} popper_done={} full={} empty={} stop={}",
                    l.pushed.get(),
                    l.pusher_done.get(),
                    l.popped.get(),
                    l.popper_done.get(),
                    l.q.full(),
                    l.q.empty(),
                    l.stop.get());
                let s = l.stop.get();
                l.stop.set(s + 1);
                if s == 0 {
                    // First callback is for stopping the test.
                    // Second one is a watchdog. Consider the test hung
                    // if this callback isn't canceled within 10 seconds.
                    // That should be long enough to work in absurdly slow test environments.
                    std::pin::Pin::new_unchecked(&mut l.stop_timer).arm_in(watchdog_duration);
                } else {
                    teseladb::log_error!(TESTLOG, "test_queue_pop_eventually is hung: pushed={} pusher_done={} popped={} popper_done={} full={} empty={} stop={}",
                        l.pushed.get(),
                        l.pusher_done.get(),
                        l.popped.get(),
                        l.popper_done.get(),
                        l.q.full(),
                        l.q.empty(),
                        l.stop.get());
                    std::process::abort();
                }
            });
            std::pin::Pin::new_unchecked(&mut (*l).stop_timer).arm_in(test_duration);
        }

        let start = SystemTime::now();
        let pusher = repeat(move || {
            // SAFETY: as above.
            let lr = unsafe { &mut *l };
            let data = lr.pushed.get();
            teseladb::log_trace!(
                TESTLOG,
                "pusher: full={} empty={} stop={}",
                lr.q.full(),
                lr.q.empty(),
                lr.stop.get()
            );
            // SAFETY: as above.
            unsafe { lr.q.push_eventually(data) }.then(move |()| {
                // SAFETY: as above.
                let lr = unsafe { &mut *l };
                lr.pushed.set(lr.pushed.get() + 1);
                if lr.stop.get() != 0 && !lr.q.empty() {
                    teseladb::log_debug!(TESTLOG, "pusher done");
                    lr.pusher_done.set(true);
                    return make_ready_future(StopIteration::YES);
                }
                make_ready_future(StopIteration::NO)
            })
        });

        let popper = repeat(move || -> Future<StopIteration> {
            // SAFETY: as above.
            let lr = unsafe { &mut *l };
            teseladb::log_trace!(
                TESTLOG,
                "popper: full={} empty={} stop={}",
                lr.q.full(),
                lr.q.empty(),
                lr.stop.get()
            );
            if lr.q.empty() {
                if lr.pusher_done.get() {
                    teseladb::log_debug!(TESTLOG, "popper done");
                    lr.popper_done.set(true);
                    return make_ready_future(StopIteration::YES);
                } else if lr.stop.get() != 0 {
                    teseladb::log_debug!(
                        TESTLOG,
                        "popper: full={} empty={} pusher_done={} stop={}",
                        lr.q.full(),
                        lr.q.empty(),
                        lr.pusher_done.get(),
                        lr.stop.get()
                    );
                }
            }
            // SAFETY: as above.
            unsafe { lr.q.pop_eventually() }.then(move |_: i32| {
                // SAFETY: as above.
                let lr = unsafe { &mut *l };
                lr.popped.set(lr.popped.get() + 1);
                make_ready_future(StopIteration::NO)
            })
        });

        pusher.get()?;
        popper.get()?;
        let elapsed = SystemTime::now().duration_since(start).unwrap_or_default();
        let elapsed_us = elapsed.as_micros() as i64;
        // SAFETY: both loops are done; only this function uses `l` now.
        let lr = unsafe { Box::from_raw(l) };
        let mut lr = lr;
        // SAFETY: the timer is still in its box, which is freed at the end of this function.
        unsafe { std::pin::Pin::new_unchecked(&mut lr.stop_timer) }.cancel();

        assert!(lr.q.empty());
        assert!(lr.pushed.get() != 0);
        assert!(lr.pusher_done.get());
        assert!(lr.popped.get() == lr.pushed.get());
        assert!(lr.popper_done.get());

        teseladb::log_info!(
            TESTLOG,
            "Pushed and popped {} elemements in {}us, {:.3} elements/us",
            lr.pushed.get(),
            elapsed_us,
            f64::from(lr.pushed.get()) / elapsed_us as f64
        );
        Ok(())
    })
});

#[cfg(feature = "alloc_failure_injection")]
engine_test_case!(test_queue_push_eventually_exception, {
    use std::rc::Rc;
    use teseladb::engine::util::alloc_failure_injector::memory::{
        BadAlloc, with_allocation_failures,
    };
    use teseladb::engine::util::noncopyable_function::NoncopyableFunction;
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let i = Cell::new(0);
        let mut q = Queue::<i32>::new(42);
        let intercepted = Rc::new(Cell::new(0));
        let counter = intercepted.clone();
        with_allocation_failures(NoncopyableFunction::<fn()>::new(move || {
            let v = i.get();
            i.set(v + 1);
            let counter = counter.clone();
            // SAFETY: `q` lives in this closure, on this thread, and is waited on right here.
            assert!(
                unsafe { q.push_eventually(v) }
                    .handle_exception_type(move |_: &BadAlloc| {
                        counter.set(counter.get() + 1);
                        Ok(())
                    })
                    .get()
                    .is_ok()
            );
        }));
        assert!(intercepted.get() != 0);
        Ok(())
    })
});

fn boom() -> ExceptionPtr {
    ExceptionPtr::new(RuntimeError("boom".to_string()))
}

engine_test_case!(test_queue_pop_after_abort, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut q = Queue::<i32>::new(1);
        let mut exception = false;
        let timer = std::rc::Rc::new(Cell::new(false));
        q.abort(boom());
        let qp: *mut Queue<i32> = &mut q;
        let t = timer.clone();
        let done = sleep(Duration::from_millis(1)).then(move |()| {
            t.set(true);
            // SAFETY: `q` outlives `done`, which is waited for below.
            unsafe { (*qp).abort(boom()) };
        });
        // SAFETY: `q` stays in place in this thread until get() returns, right here; it is
        // reached through `qp`, as the continuation above reaches it.
        if unsafe { (*qp).pop_eventually() }.get().is_err() {
            exception = !timer.get();
        }
        assert!(exception);
        done.get()?;
        Ok(())
    })
});

engine_test_case!(test_queue_push_abort, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut q = Queue::<i32>::new(1);
        let mut exception = false;
        let timer = std::rc::Rc::new(Cell::new(false));
        q.abort(boom());
        let qp: *mut Queue<i32> = &mut q;
        let t = timer.clone();
        let done = sleep(Duration::from_millis(1)).then(move |()| {
            t.set(true);
            // SAFETY: `q` outlives `done`, which is waited for below.
            unsafe { (*qp).abort(boom()) };
        });
        // SAFETY: `q` stays in place in this thread until get() returns, right here; it is
        // reached through `qp`, as the continuation above reaches it.
        if unsafe { (*qp).push_eventually(1) }.get().is_err() {
            exception = !timer.get();
        }
        assert!(exception);
        done.get()?;
        Ok(())
    })
});
