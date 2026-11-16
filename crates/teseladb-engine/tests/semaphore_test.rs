//! Tests of `Semaphore` and its helpers: waits, signals, timeouts, breaking, abortable
//! waits, `with_semaphore`, units (`get_units`, `try_get_units`, splitting, returning and
//! reassigning them), named semaphores and custom exception factories, and `SharedMutex`
//! readers and writers.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/expected_exception.rs"]
mod expected_exception;

use std::cell::Cell;
use std::fmt;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use expected_exception::ExpectedException;
use teseladb::engine::core::abort_source::{AbortRequestedException, AbortSource};
use teseladb::engine::core::do_with::do_with;
use teseladb::engine::core::future::{
    BrokenPromise, ExceptionPtr, Future, InvalidArgument, RuntimeError, make_exception_ptr,
    make_ready_future,
};
use teseladb::engine::core::loop_::parallel_for_each;
use teseladb::engine::core::map_reduce::map_reduce_initial;
use teseladb::engine::core::semaphore::{
    BasicSemaphore, BrokenNamedSemaphore, BrokenSemaphore, NamedSemaphore,
    NamedSemaphoreExceptionFactory, NamedSemaphoreTimedOut, Semaphore, SemaphoreAborted,
    SemaphoreExceptionFactory, SemaphoreTimedOut, SemaphoreUnits, get_units, get_units_abortable,
    get_units_for, try_get_units, with_semaphore,
};
use teseladb::engine::core::shared_mutex::{SharedMutex, with_lock, with_shared};
use teseladb::engine::core::shared_ptr::make_lw_shared;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::timer::{Clock, SteadyClockType, Timer};
use teseladb::engine_test_case;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

const ONE_MIN: Duration = Duration::from_secs(60);

fn new_as() -> Pin<Box<AbortSource>> {
    Box::pin(AbortSource::new())
}

engine_test_case!(test_semaphore_consume, {
    let mut sem = Semaphore::new(0);
    sem.consume(1);
    assert_eq!(sem.current(), 0usize);
    assert_eq!(sem.waiters(), 0usize);

    assert!(!sem.try_wait(0));
    let fut = sem.wait(1);
    assert!(!fut.available());
    assert_eq!(sem.waiters(), 1usize);
    sem.signal(2);
    assert_eq!(sem.waiters(), 0usize);
    fut.finally(move || drop(sem))
});

engine_test_case!(test_semaphore_1, {
    do_with((Semaphore::new(0), Rc::new(Cell::new(0))), |x| {
        let c = x.1.clone();
        let _ = x.0.wait(1).then(move |()| c.set(c.get() + 1));
        x.0.signal(1);
        let c = x.1.clone();
        sleep(ms(10)).then(move |()| assert_eq!(c.get(), 1))
    })
});

engine_test_case!(test_semaphore_2, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sem = Some(Semaphore::new(0));
        let x = Rc::new(Cell::new(0));
        let xc = x.clone();
        let fut = sem.as_mut().expect("sem").wait(1).then(move |()| xc.set(xc.get() + 1));
        sleep(ms(10)).get()?;
        assert_eq!(x.get(), 0);
        sem = None;
        assert!(sem.is_none());
        assert!(matches!(fut.get(), Err(ref e) if e.is::<BrokenPromise>()));
        Ok(())
    })
});

engine_test_case!(test_semaphore_timeout_1, {
    do_with((Semaphore::new(0), Rc::new(Cell::new(0))), |x| {
        let semp: *mut Semaphore = &mut x.0;
        let c = x.1.clone();
        // SAFETY: the semaphore does not move nor is dropped while the timed wait is pending (it lives in this frame until the wait has resolved).
        let _ = unsafe { x.0.wait_for(ms(100), 1) }.then(move |()| c.set(c.get() + 1));
        // SAFETY: do_with keeps the semaphore alive past the 200ms sleep.
        let _ = sleep(ms(3)).then(move |()| unsafe { (*semp).signal(1) });
        let c = x.1.clone();
        sleep(ms(200)).then(move |()| assert_eq!(c.get(), 1))
    })
});

engine_test_case!(test_semaphore_timeout_2, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sem = Semaphore::new(0);
        let semp: *mut Semaphore = &mut sem;
        let x = Rc::new(Cell::new(0));
        let xc = x.clone();
        // SAFETY: the semaphore does not move nor is dropped while the timed wait is pending (it lives in this frame until the wait has resolved).
        let fut1 = unsafe { sem.wait_for(ms(3), 1) }.then(move |()| xc.set(xc.get() + 1));
        let signaled = Rc::new(Cell::new(false));
        let s = signaled.clone();
        let fut2 = sleep(ms(100)).then(move |()| {
            s.set(true);
            // SAFETY: `sem` outlives fut2, waited for below.
            unsafe { (*semp).signal(1) };
        });
        sleep(ms(200)).get()?;
        fut2.get()?;
        assert!(signaled.get());
        assert!(matches!(fut1.get(), Err(ref e) if e.is::<SemaphoreTimedOut>()));
        assert_eq!(x.get(), 0);
        Ok(())
    })
});

engine_test_case!(test_semaphore_mix_1, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sem = Semaphore::new(0);
        let semp: *mut Semaphore = &mut sem;
        let x = Rc::new(Cell::new(0));
        let xc = x.clone();
        // SAFETY: the semaphore does not move nor is dropped while the timed wait is pending (it lives in this frame until the wait has resolved).
        let fut1 = unsafe { sem.wait_for(ms(30), 1) }.then(move |()| xc.set(xc.get() + 1));
        let xc = x.clone();
        let fut2 = sem.wait(1).then(move |()| xc.set(xc.get() + 10));
        // SAFETY: `sem` outlives fut3, waited for below.
        let fut3 = sleep(ms(100)).then(move |()| unsafe { (*semp).signal(1) });
        sleep(ms(200)).get()?;
        fut3.get()?;
        fut2.get()?;
        assert!(matches!(fut1.get(), Err(ref e) if e.is::<SemaphoreTimedOut>()));
        assert_eq!(x.get(), 10);
        Ok(())
    })
});

/// The error a semaphore is broken with in the tests.
#[derive(Debug, Clone)]
struct Oops;

impl fmt::Display for Oops {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("oops")
    }
}

impl std::error::Error for Oops {}

engine_test_case!(test_broken_semaphore, {
    let sem = make_lw_shared(std::cell::UnsafeCell::new(Semaphore::new(0)));
    // SAFETY: the semaphore is used from this shard only, one call at a time.
    let s = |p: &teseladb::engine::core::shared_ptr::LwSharedPtr<
        std::cell::UnsafeCell<Semaphore>,
    >| unsafe { &mut *std::cell::UnsafeCell::get(&**p) };
    let keep = sem.clone();
    let check_result = move |f: Future<()>| -> Future<()> {
        let _keep = &keep;
        match f.get() {
            Ok(()) => panic!("expecting exception"),
            Err(e) if e.is::<Oops>() => {
                // ok
                make_ready_future(())
            }
            Err(_) => panic!("wrong exception seen"),
        }
    };
    let ret = s(&sem).wait(1).then_wrapped(check_result.clone());
    s(&sem).broken_with_exception(Oops);
    s(&sem).wait(1).then_wrapped(check_result).then(move |()| ret)
});

/// An exception factory with only a custom timeout error; broken semaphores use the default error.
#[derive(Debug, Clone, Copy, Default)]
struct TestSemaphoreExceptionFactory;

impl SemaphoreExceptionFactory for TestSemaphoreExceptionFactory {
    fn timeout(&self) -> ExceptionPtr {
        make_exception_ptr(SemaphoreTimedOut)
    }
}

engine_test_case!(test_default_broken_semaphore, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sem = BasicSemaphore::<TestSemaphoreExceptionFactory>::new(0);
        let fut = sem.wait(1);
        assert!(!fut.available());
        sem.broken();
        assert!(matches!(fut.get(), Err(ref e) if e.is::<BrokenSemaphore>()));
        assert!(matches!(sem.wait(1).get(), Err(ref e) if e.is::<BrokenSemaphore>()));
        Ok(())
    })
});

engine_test_case!(test_non_default_broken_semaphore, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sem = BasicSemaphore::<TestSemaphoreExceptionFactory>::new(0);
        let fut = sem.wait(1);
        assert!(!fut.available());
        sem.broken_with_exception(ExpectedException::new());
        assert!(matches!(fut.get(), Err(ref e) if e.is::<ExpectedException>()));
        assert!(matches!(sem.wait(1).get(), Err(ref e) if e.is::<ExpectedException>()));
        Ok(())
    })
});

engine_test_case!(test_shared_mutex_exclusive, {
    do_with((SharedMutex::new(), Rc::new(Cell::new(0u32))), |x| {
        let smp: *mut SharedMutex = &mut x.0;
        let counter = x.1.clone();
        parallel_for_each(0..10, move |_idx: i32| {
            let counter = counter.clone();
            // SAFETY: do_with keeps the mutex alive and in place until the loop resolves.
            unsafe {
                with_lock(&mut *smp, move || {
                    assert_eq!(counter.get(), 0u32);
                    counter.set(counter.get() + 1);
                    sleep(ms(10)).then(move |()| {
                        counter.set(counter.get() - 1);
                        assert_eq!(counter.get(), 0u32);
                    })
                })
            }
        })
    })
});

fn running_in_parallel(smp: *mut SharedMutex, counter: Rc<Cell<u32>>) -> Future<bool> {
    // SAFETY: the caller's do_with keeps the mutex alive and in place until map_reduce
    // resolves.
    unsafe {
        with_shared(&mut *smp, move || {
            counter.set(counter.get() + 1);
            sleep(ms(10)).then(move |()| {
                let was_parallel = counter.get() != 0;
                counter.set(counter.get() - 1);
                make_ready_future(was_parallel)
            })
        })
    }
}

fn running_alone(smp: *mut SharedMutex, counter: Rc<Cell<u32>>) -> Future<bool> {
    // SAFETY: as above.
    unsafe {
        with_lock(&mut *smp, move || {
            assert_eq!(counter.get(), 0u32);
            counter.set(counter.get() + 1);
            sleep(ms(10)).then(move |()| {
                counter.set(counter.get() - 1);
                assert_eq!(counter.get(), 0u32);
                make_ready_future(true)
            })
        })
    }
}

engine_test_case!(test_shared_mutex_shared, {
    do_with((SharedMutex::new(), Rc::new(Cell::new(0u32))), |x| {
        let smp: *mut SharedMutex = &mut x.0;
        let counter = x.1.clone();
        let c2 = counter.clone();
        map_reduce_initial(
            0..100,
            move |_instance: i32| running_in_parallel(smp, counter.clone()),
            false,
            |a: bool, b: bool| a | b,
        )
        .then(move |result: bool| {
            assert!(result);
            assert_eq!(c2.get(), 0u32);
        })
    })
});

engine_test_case!(test_shared_mutex_mixed, {
    do_with((SharedMutex::new(), Rc::new(Cell::new(0u32))), |x| {
        let smp: *mut SharedMutex = &mut x.0;
        let counter = x.1.clone();
        let c2 = counter.clone();
        let run = move |instance: i32| {
            if instance % 9 == 0 {
                running_alone(smp, counter.clone())
            } else {
                running_in_parallel(smp, counter.clone())
            }
        };
        map_reduce_initial(0..100, run, false, |a: bool, b: bool| a | b).then(
            move |result: bool| {
                assert!(result);
                assert_eq!(c2.get(), 0u32);
            },
        )
    })
});

engine_test_case!(test_with_semaphore, {
    do_with((Semaphore::new(1), Rc::new(Cell::new(0))), |x| {
        let semp: *mut Semaphore = &mut x.0;
        let counter = x.1.clone();
        let c1 = counter.clone();
        // SAFETY: do_with keeps the semaphore alive and in place until this chain resolves.
        unsafe { with_semaphore(&mut *semp, 1, move || c1.set(c1.get() + 1)) }.then(move |()| {
            let c2 = counter.clone();
            // SAFETY: do_with keeps the semaphore alive and in place until this chain resolves.
            unsafe {
                with_semaphore(&mut *semp, 1, move || -> Result<(), ExceptionPtr> {
                    c2.set(c2.get() + 1);
                    // fail with 123
                    Err(ExceptionPtr::new(RuntimeError("123".to_string())))
                })
            }
            .then_wrapped(move |mut fut: Future<()>| {
                assert_eq!(counter.get(), 2);
                assert!(fut.failed());
                fut.ignore_ready_future();
            })
        })
    })
});

engine_test_case!(test_semaphore_units_valid_splitting, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sm = Semaphore::new(2);
        // SAFETY: the semaphore stays alive and in place in this frame until the units are gone and the future has resolved, and nothing borrows it when the units return to it.
        let mut units = unsafe { get_units_for(&mut sm, 2, ONE_MIN) }.get()?;
        {
            assert_eq!(units.count(), 2);
            assert_eq!(sm.available_units(), 0);
            let _split = units.split(1)?;
            assert_eq!(sm.available_units(), 0);
        }
        assert_eq!(sm.available_units(), 1);
        drop(units);
        Ok(())
    })
});

engine_test_case!(test_semaphore_units_invalid_splitting, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sm = Semaphore::new(2);
        // SAFETY: the semaphore stays alive and in place in this frame until the units are gone and the future has resolved, and nothing borrows it when the units return to it.
        let mut units = unsafe { get_units_for(&mut sm, 2, ONE_MIN) }.get()?;
        assert_eq!(sm.available_units(), 0);
        assert!(matches!(units.split(10), Err(ref e) if e.is::<InvalidArgument>()));
        assert_eq!(sm.available_units(), 0);
        Ok(())
    })
});

engine_test_case!(test_semaphore_units_return_when_destroyed, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sm = Semaphore::new(3);
        {
            // SAFETY: the semaphore stays alive and in place in this frame until the units are gone and the future has resolved, and nothing borrows it when the units return to it.
            let mut units = unsafe { get_units_for(&mut sm, 3, ONE_MIN) }.get()?;
            assert_eq!(units.count(), 3);
            assert_eq!(sm.available_units(), 0);
            assert_eq!(units.return_units(1)?, 2);
            assert_eq!(units.count(), 2);
            assert_eq!(sm.available_units(), 1);
        }
        assert_eq!(sm.available_units(), 3);
        Ok(())
    })
});

engine_test_case!(test_semaphore_units_return_all, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sm = Semaphore::new(3);
        // SAFETY: the semaphore stays alive and in place in this frame until the units are gone and the future has resolved, and nothing borrows it when the units return to it.
        let mut units = unsafe { get_units_for(&mut sm, 2, ONE_MIN) }.get()?;
        assert_eq!(sm.available_units(), 1);
        assert!(matches!(units.return_units(10), Err(ref e) if e.is::<InvalidArgument>()));
        assert_eq!(sm.available_units(), 1);
        units.return_all();
        assert_eq!(units.count(), 0);
        assert_eq!(sm.available_units(), 3);
        Ok(())
    })
});

engine_test_case!(test_semaphore_try_get_units, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        const INITIAL_UNITS: usize = 1;
        let mut sm = Semaphore::new(INITIAL_UNITS);

        // SAFETY: the semaphore stays alive and in place in this frame until the units are gone and the future has resolved, and nothing borrows it when the units return to it.
        let mut opt_units = unsafe { try_get_units(&mut sm, 1) };
        assert!(opt_units.is_some());

        // SAFETY: the semaphore stays alive and in place in this frame until the units are gone and the future has resolved, and nothing borrows it when the units return to it.
        let opt_units2 = unsafe { try_get_units(&mut sm, 1) };
        assert!(opt_units2.is_none());

        opt_units = None;
        assert!(opt_units.is_none());
        assert_eq!(sm.available_units(), INITIAL_UNITS as isize);

        // SAFETY: the semaphore stays alive and in place in this frame until the units are gone and the future has resolved, and nothing borrows it when the units return to it.
        opt_units = unsafe { try_get_units(&mut sm, 1) };
        assert!(opt_units.is_some());

        opt_units.as_mut().expect("units").return_all();
        assert_eq!(opt_units.as_ref().expect("units").count(), 0);
        assert_eq!(sm.available_units(), INITIAL_UNITS as isize);
        Ok(())
    })
});

engine_test_case!(test_semaphore_units_abort, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sm = Semaphore::new(3);
        // SAFETY: the semaphore stays alive and in place in this frame until the units are gone and the future has resolved, and nothing borrows it when the units return to it.
        let units = unsafe { get_units_for(&mut sm, 3, ONE_MIN) }.get()?;
        assert_eq!(units.count(), 3);
        let mut as_ = new_as();
        // SAFETY: the pointer is only used through Pin::new_unchecked; the source never moves.
        let asp: *mut AbortSource = unsafe { as_.as_mut().get_unchecked_mut() };
        // SAFETY: the semaphore stays alive and in place in this frame until the units are gone and the future has resolved, and nothing borrows it when the units return to it.
        let f = unsafe { get_units_abortable(&mut sm, 1, as_.as_mut()) };
        assert!(!f.available());
        // SAFETY: the abort source is pinned in its box and outlives the sleep.
        let _ =
            sleep(ms(1)).then(move |()| unsafe { Pin::new_unchecked(&mut *asp) }.request_abort());
        assert!(matches!(f.get(), Err(ref e) if e.is::<SemaphoreAborted>()));
        drop(units);
        Ok(())
    })
});

engine_test_case!(test_semaphore_units_bool_operator, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sem = Semaphore::new(2);
        let mut u0 = SemaphoreUnits::default();
        assert!(!u0.is_some());

        // SAFETY: the semaphore stays alive and in place in this frame until the units are gone and the future has resolved, and nothing borrows it when the units return to it.
        u0 = unsafe { get_units(&mut sem, 2) }.get()?;
        assert!(u0.is_some());

        u0.return_units(1)?;
        assert!(u0.is_some());

        let n = u0.release();
        assert!(!u0.is_some());
        sem.signal(n);

        // SAFETY: the semaphore stays alive and in place in this frame until the units are gone and the future has resolved, and nothing borrows it when the units return to it.
        u0 = unsafe { get_units(&mut sem, 2) }.get()?;
        assert!(u0.is_some());

        let mut u1 = std::mem::take(&mut u0);
        assert!(u1.is_some());
        assert!(!u0.is_some());

        u1.return_all();
        assert!(!u1.is_some());

        // SAFETY: the semaphore stays alive and in place in this frame until the units are gone and the future has resolved, and nothing borrows it when the units return to it.
        u0 = unsafe { get_units(&mut sem, 2) }.get()?;
        assert!(u0.is_some());

        u1 = u0.split(1)?;
        assert!(u1.is_some());
        assert!(u0.is_some());

        u0.adopt(std::mem::take(&mut u1));
        assert!(!u1.is_some());
        assert!(u0.is_some());
        Ok(())
    })
});

fn named_factory() -> NamedSemaphoreExceptionFactory {
    NamedSemaphoreExceptionFactory { name: "name_of_the_semaphore".into() }
}

engine_test_case!(test_named_semaphore_error, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let sem = make_lw_shared(std::cell::UnsafeCell::new(NamedSemaphore::with_factory(
            0,
            named_factory(),
        )));
        // SAFETY: the semaphore is used from this thread only, one call at a time.
        let s = |p: &teseladb::engine::core::shared_ptr::LwSharedPtr<
            std::cell::UnsafeCell<NamedSemaphore>,
        >| unsafe { &mut *std::cell::UnsafeCell::get(&**p) };
        let keep = sem.clone();
        let check_result = move |f: Future<()>| -> Future<()> {
            let _keep = &keep;
            match f.get() {
                Ok(()) => panic!("Expecting an exception"),
                Err(e) => match e.downcast_ref::<BrokenNamedSemaphore>() {
                    Some(ex) => assert!(ex.what().contains("name_of_the_semaphore")),
                    None => panic!(
                        "Expected an instance of broken_named_semaphore with proper semaphore name"
                    ),
                },
            }
            make_ready_future(())
        };
        let ret = s(&sem).wait(1).then_wrapped(check_result.clone());
        s(&sem).broken();
        s(&sem).wait(1).then_wrapped(check_result).then(move |()| ret).get()?;
        Ok(())
    })
});

engine_test_case!(test_named_semaphore_timeout, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sem = NamedSemaphore::with_factory(0, named_factory());

        let f = // SAFETY: `sem` lives in this frame, in place, until the wait has resolved.
        unsafe { sem.wait_until(<SteadyClockType as Clock>::add(<SteadyClockType as Clock>::now(), ms(1)), 1) };
        match f.get() {
            Ok(()) => panic!("Expecting an exception"),
            Err(e) => match e.downcast_ref::<NamedSemaphoreTimedOut>() {
                Some(ex) => assert!(ex.what().contains("name_of_the_semaphore")),
                None => panic!(
                    "Expected an instance of named_semaphore_timed_out with proper semaphore name"
                ),
            },
        }
        Ok(())
    })
});

engine_test_case!(test_semaphore_abort_after_wait, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sem = Semaphore::new(0);
        let mut as_ = new_as();
        let x = Rc::new(Cell::new(0));
        let xc = x.clone();
        // SAFETY: `sem` lives in this frame, in place, until the wait has resolved.
        let fut1 =
            unsafe { sem.wait_abortable(as_.as_mut(), 1) }.then(move |()| xc.set(xc.get() + 1));
        as_.as_mut().request_abort();
        sem.signal(1);
        assert!(matches!(fut1.get(), Err(ref e) if e.is::<SemaphoreAborted>()));
        assert_eq!(x.get(), 0);
        Ok(())
    })
});

engine_test_case!(test_semaphore_abort_with_exception_after_wait, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sem = Semaphore::new(0);
        let mut as_ = new_as();
        let x = Rc::new(Cell::new(0));
        let xc = x.clone();
        // SAFETY: `sem` lives in this frame, in place, until the wait has resolved.
        let fut1 =
            unsafe { sem.wait_abortable(as_.as_mut(), 1) }.then(move |()| xc.set(xc.get() + 1));
        as_.as_mut().request_abort_ex_from(ExpectedException::new());
        sem.signal(1);
        assert!(matches!(fut1.get(), Err(ref e) if e.is::<ExpectedException>()));
        assert_eq!(x.get(), 0);
        Ok(())
    })
});

engine_test_case!(test_semaphore_abort_before_wait, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sem = Semaphore::new(0);
        let mut as_ = new_as();
        let x = Rc::new(Cell::new(0));
        as_.as_mut().request_abort();
        let xc = x.clone();
        // SAFETY: `sem` lives in this frame, in place, until the wait has resolved.
        let fut1 =
            unsafe { sem.wait_abortable(as_.as_mut(), 1) }.then(move |()| xc.set(xc.get() + 1));
        sem.signal(1);
        assert!(matches!(fut1.get(), Err(ref e) if e.is::<SemaphoreAborted>()));
        assert_eq!(x.get(), 0);
        Ok(())
    })
});

engine_test_case!(test_reassigned_units_are_returned, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sem0 = Semaphore::new(1);
        let mut sem1 = Semaphore::new(1);
        // SAFETY: the semaphore stays alive and in place in this frame until the units are gone and the future has resolved, and nothing borrows it when the units return to it.
        // The assignment drops the value it replaces, which is what this checks.
        #[allow(unused_assignments)]
        let mut units = unsafe { get_units(&mut sem0, 1) }.get()?;
        let wait = sem0.wait(1);
        assert!(!wait.available());
        // SAFETY: the semaphore stays alive and in place in this frame until the units are gone and the future has resolved, and nothing borrows it when the units return to it.
        units = unsafe { get_units(&mut sem1, 1) }.get()?;
        let mut t =
            std::pin::pin!(Timer::<SteadyClockType>::with_callback(|| std::process::abort()));
        t.as_mut().arm_in(Duration::from_secs(1));
        // will hang if units are not returned when reassigned
        wait.get()?;
        t.as_mut().cancel();
        drop(units);
        Ok(())
    })
});

engine_test_case!(test_get_units_after_move, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sem = Box::new(Semaphore::new(0));
        // SAFETY: the semaphore stays alive and in place in this frame until the units are gone and the future has resolved, and nothing borrows it when the units return to it.
        let f = unsafe { get_units(&mut sem, 1) };
        assert!(!f.available());
        sem.signal(1);
        f.get()?;
        Ok(())
    })
});

engine_test_case!(test_get_units_abort_after_move, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sem = Box::new(Semaphore::new(0));
        let mut as_ = new_as();
        // SAFETY: the semaphore stays alive and in place in this frame until the units are gone and the future has resolved, and nothing borrows it when the units return to it.
        let f = unsafe { get_units_abortable(&mut sem, 1, as_.as_mut()) };
        assert!(!f.available());
        as_.as_mut().request_abort();
        assert!(matches!(f.get(), Err(ref e) if e.is::<AbortRequestedException>()));
        Ok(())
    })
});

engine_test_case!(test_immediate_abort_after_move, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sem = Box::new(Semaphore::new(0));
        let mut as_ = new_as();
        as_.as_mut().request_abort();
        // SAFETY: the semaphore stays alive and in place in this frame until the units are gone and the future has resolved, and nothing borrows it when the units return to it.
        assert!(matches!(unsafe { get_units_abortable(&mut sem, 1, as_.as_mut()) }
            .get(), Err(ref e) if e.is::<AbortRequestedException>()));
        Ok(())
    })
});

engine_test_case!(test_get_units_after_move_assign, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        // The assignment drops the value it replaces, which is what this checks.
        #[allow(unused_assignments)]
        let mut sem = Semaphore::new(0);
        sem = Semaphore::new(0);
        // SAFETY: the semaphore stays alive and in place in this frame until the units are gone and the future has resolved, and nothing borrows it when the units return to it.
        let f = unsafe { get_units(&mut sem, 1) };
        assert!(!f.available());
        sem.signal(1);
        f.get()?;
        Ok(())
    })
});

engine_test_case!(test_get_units_abort_after_move_assign, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        // The assignment drops the value it replaces, which is what this checks.
        #[allow(unused_assignments)]
        let mut sem = Semaphore::new(0);
        sem = Semaphore::new(0);
        let mut as_ = new_as();
        // SAFETY: the semaphore stays alive and in place in this frame until the units are gone and the future has resolved, and nothing borrows it when the units return to it.
        let f = unsafe { get_units_abortable(&mut sem, 1, as_.as_mut()) };
        assert!(!f.available());
        as_.as_mut().request_abort();
        assert!(matches!(f.get(), Err(ref e) if e.is::<AbortRequestedException>()));
        Ok(())
    })
});

engine_test_case!(test_immediate_abort_after_move_assign, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        // The assignment drops the value it replaces, which is what this checks.
        #[allow(unused_assignments)]
        let mut sem = Semaphore::new(0);
        sem = Semaphore::new(0);
        let mut as_ = new_as();
        as_.as_mut().request_abort();
        // SAFETY: the semaphore stays alive and in place in this frame until the units are gone and the future has resolved, and nothing borrows it when the units return to it.
        assert!(matches!(unsafe { get_units_abortable(&mut sem, 1, as_.as_mut()) }
            .get(), Err(ref e) if e.is::<AbortRequestedException>()));
        Ok(())
    })
});
