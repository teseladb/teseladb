//! Tests of `RwLock` and `SharedMutex`: exclusive and shared locking, `with_lock` and
//! `with_shared` (also when the function, its move or the lock fails), abortable and
//! timed waits, and typed results.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::fmt;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use teseladb::engine::core::abort_source::AbortSource;
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::do_with::{self, Lockable, do_with};
use teseladb::engine::core::future::{ExceptionPtr, Future, RuntimeError, make_exception_future};
use teseladb::engine::core::loop_::parallel_for_each;
use teseladb::engine::core::rwlock::Rwlock;
use teseladb::engine::core::semaphore::SemaphoreAborted;
use teseladb::engine::core::shared_mutex::{
    SharedMutex, get_shared_lock, get_unique_lock, with_lock, with_shared,
};
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::coroutine::parallel_for_each::ParallelForEach as CoroutineParallelForEach;
use teseladb::engine_test_case;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn injected() -> ExceptionPtr {
    ExceptionPtr::new(RuntimeError("injected".to_string()))
}

engine_test_case!(test_rwlock, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut l = Rwlock::new();

        l.for_write().lock().get()?;
        assert!(!l.try_write_lock());
        assert!(!l.try_read_lock());
        l.for_write().unlock();

        l.for_read().lock().get()?;
        assert!(!l.try_write_lock());
        assert!(l.try_read_lock());
        l.for_read().lock().get()?;
        l.for_read().unlock();
        l.for_read().unlock();
        l.for_read().unlock();

        assert!(l.try_write_lock());
        l.for_write().unlock();
        Ok(())
    })
});

engine_test_case!(test_with_lock_mutable, {
    do_with(Rwlock::new(), |l: &mut Rwlock| {
        let p = Box::new(42);
        // SAFETY: the lock lives, in place, in this frame (or the do_with frame) until the returned future and the lock it gives are gone.
        unsafe {
            do_with::with_lock(l.for_read(), move || {
                let _p = p;
            })
        }
    })
});

engine_test_case!(test_rwlock_exclusive, {
    do_with((Rwlock::new(), Rc::new(Cell::new(0u32))), |x| {
        let lp: *mut Rwlock = &mut x.0;
        let counter = x.1.clone();
        parallel_for_each(0..10, move |_idx: i32| {
            let counter = counter.clone();
            // SAFETY: do_with keeps the lock alive until the loop resolves.
            unsafe {
                do_with::with_lock((*lp).for_write(), move || {
                    assert_eq!(counter.get(), 0u32);
                    counter.set(counter.get() + 1);
                    sleep(ms(1)).then(move |()| {
                        counter.set(counter.get() - 1);
                        assert_eq!(counter.get(), 0u32);
                    })
                })
            }
        })
    })
});

engine_test_case!(test_rwlock_shared, {
    do_with((Rwlock::new(), Rc::new(Cell::new(0u32)), Rc::new(Cell::new(0u32))), |x| {
        let lp: *mut Rwlock = &mut x.0;
        let (counter, max) = (x.1.clone(), x.2.clone());
        let (c2, m2) = (counter.clone(), max.clone());
        parallel_for_each(0..10, move |_idx: i32| {
            let (counter, max) = (counter.clone(), max.clone());
            // SAFETY: as above.
            unsafe {
                do_with::with_lock((*lp).for_read(), move || {
                    counter.set(counter.get() + 1);
                    max.set(max.get().max(counter.get()));
                    sleep(ms(1)).then(move |()| counter.set(counter.get() - 1))
                })
            }
        })
        .finally(move || {
            assert_eq!(c2.get(), 0u32);
            assert_ne!(m2.get(), 0u32);
        })
    })
});

engine_test_case!(test_rwlock_failed_func, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut l = Rwlock::new();

        // verify that the rwlock is unlocked when func fails
        // SAFETY: the lock lives, in place, in this frame (or the do_with frame) until the returned future and the lock it gives are gone.
        let mut fut: Future<()> = unsafe {
            do_with::with_lock(l.for_read(), || -> Result<(), ExceptionPtr> { Err(injected()) })
        };
        assert!(matches!(fut.get(), Err(ref e) if e.is::<RuntimeError>()));

        // SAFETY: the lock lives, in place, in this frame (or the do_with frame) until the returned future and the lock it gives are gone.
        fut = unsafe {
            do_with::with_lock(l.for_write(), || -> Result<(), ExceptionPtr> { Err(injected()) })
        };
        assert!(matches!(fut.get(), Err(ref e) if e.is::<RuntimeError>()));

        assert!(l.try_write_lock());
        l.for_write().unlock();
        Ok(())
    })
});

fn new_as() -> Pin<Box<AbortSource>> {
    Box::pin(AbortSource::new())
}

/// Requests an abort of `as_` after 1 ms, in the background.
fn abort_later(as_: &mut Pin<Box<AbortSource>>) {
    // SAFETY: the source is pinned in its box, which outlives the wait that follows.
    let asp: *mut AbortSource = unsafe { as_.as_mut().get_unchecked_mut() };
    // SAFETY: `asp` points at the source, pinned in its box, which outlives the wait that follows.
    let _ = sleep(ms(1)).then(move |()| unsafe { Pin::new_unchecked(&mut *asp) }.request_abort());
}

engine_test_case!(test_rwlock_abort, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut l = Rwlock::new();

        l.write_lock().get()?;

        {
            let mut as_ = new_as();
            // SAFETY: `l` lives, in place, in this frame until the waits have resolved and the holders are gone.
            let f = unsafe { l.write_lock_abortable(as_.as_mut()) };
            assert!(!f.available());
            abort_later(&mut as_);
            assert!(matches!(f.get(), Err(ref e) if e.is::<SemaphoreAborted>()));
        }

        {
            let mut as_ = new_as();
            // SAFETY: `l` lives, in place, in this frame until the waits have resolved and the holders are gone.
            let f = unsafe { l.read_lock_abortable(as_.as_mut()) };
            assert!(!f.available());
            abort_later(&mut as_);
            assert!(matches!(f.get(), Err(ref e) if e.is::<SemaphoreAborted>()));
        }
        Ok(())
    })
});

engine_test_case!(test_rwlock_hold_abort, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut l = Rwlock::new();

        // SAFETY: `l` lives, in place, in this frame until the waits have resolved and the holders are gone.
        let _wh = unsafe { l.hold_write_lock() }.get()?;

        {
            let mut as_ = new_as();
            // SAFETY: `l` lives, in place, in this frame until the waits have resolved and the holders are gone.
            let f = unsafe { l.hold_write_lock_abortable(as_.as_mut()) };
            assert!(!f.available());
            abort_later(&mut as_);
            assert!(matches!(f.get(), Err(ref e) if e.is::<SemaphoreAborted>()));
        }

        {
            let mut as_ = new_as();
            // SAFETY: `l` lives, in place, in this frame until the waits have resolved and the holders are gone.
            let f = unsafe { l.hold_read_lock_abortable(as_.as_mut()) };
            assert!(!f.available());
            abort_later(&mut as_);
            assert!(matches!(f.get(), Err(ref e) if e.is::<SemaphoreAborted>()));
        }
        Ok(())
    })
});

engine_test_case!(test_rwlock_hold, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut l = Rwlock::new();

        // SAFETY: `l` lives, in place, in this frame until the waits have resolved and the holders are gone.
        let mut rl = unsafe { l.hold_read_lock() }.get()?;
        // SAFETY: `l` lives, in place, in this frame until the waits have resolved and the holders are gone.
        let mut opt_rl = unsafe { l.try_hold_read_lock() };
        assert!(opt_rl.is_some());
        // SAFETY: `l` lives, in place, in this frame until the waits have resolved and the holders are gone.
        assert!(unsafe { l.try_hold_write_lock() }.is_none());

        rl.return_all();
        // SAFETY: `l` lives, in place, in this frame until the waits have resolved and the holders are gone.
        assert!(unsafe { l.try_hold_write_lock() }.is_none());

        opt_rl.as_mut().expect("holder").return_all();
        // SAFETY: `l` lives, in place, in this frame until the waits have resolved and the holders are gone.
        let opt_wl = unsafe { l.try_hold_write_lock() };
        assert!(opt_wl.is_some());
        // SAFETY: `l` lives, in place, in this frame until the waits have resolved and the holders are gone.
        assert!(unsafe { l.try_hold_read_lock() }.is_none());
        // SAFETY: `l` lives, in place, in this frame until the waits have resolved and the holders are gone.
        assert!(unsafe { l.try_hold_write_lock() }.is_none());
        Ok(())
    })
});

/// A lock whose `lock()` always fails.
struct TestLock;

impl Lockable for TestLock {
    fn lock(&mut self) -> Future<()> {
        make_exception_future(injected())
    }
    fn unlock(&mut self) {
        panic!("must not be reached");
    }
}

engine_test_case!(test_failed_with_lock, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut l = TestLock;

        // if l.lock() fails neither the function nor l.unlock()
        // should be called.
        // SAFETY: the lock lives, in place, in this frame (or the do_with frame) until the returned future and the lock it gives are gone.
        let r: Future<()> = unsafe {
            do_with::with_lock(&mut l, || -> Future<()> { panic!("must not be reached") })
        };
        assert!(matches!(r.get(), Err(ref e) if e.is::<RuntimeError>()));
        Ok(())
    })
});

engine_test_case!(test_shared_mutex, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sm = SharedMutex::new();

        sm.lock().get()?;
        assert!(!sm.try_lock());
        assert!(!sm.try_lock_shared());
        sm.unlock();

        sm.lock_shared().get()?;
        assert!(!sm.try_lock());
        assert!(sm.try_lock_shared());
        sm.lock_shared().get()?;
        sm.unlock_shared();
        sm.unlock_shared();
        sm.unlock_shared();

        assert!(sm.try_lock());
        sm.unlock();
        Ok(())
    })
});

engine_test_case!(test_shared_mutex_exclusive, {
    do_with((SharedMutex::new(), Rc::new(Cell::new(0u32))), |x| {
        let smp: *mut SharedMutex = &mut x.0;
        let counter = x.1.clone();
        parallel_for_each(0..10, move |_idx: i32| {
            let counter = counter.clone();
            // SAFETY: do_with keeps the mutex alive until the loop resolves.
            unsafe {
                with_lock(&mut *smp, move || {
                    assert_eq!(counter.get(), 0u32);
                    counter.set(counter.get() + 1);
                    sleep(ms(1)).then(move |()| {
                        counter.set(counter.get() - 1);
                        assert_eq!(counter.get(), 0u32);
                    })
                })
            }
        })
    })
});

engine_test_case!(test_shared_mutex_shared, {
    do_with((SharedMutex::new(), Rc::new(Cell::new(0u32)), Rc::new(Cell::new(0u32))), |x| {
        let smp: *mut SharedMutex = &mut x.0;
        let (counter, max) = (x.1.clone(), x.2.clone());
        let (c2, m2) = (counter.clone(), max.clone());
        parallel_for_each(0..10, move |_idx: i32| {
            let (counter, max) = (counter.clone(), max.clone());
            // SAFETY: as above.
            unsafe {
                with_shared(&mut *smp, move || {
                    counter.set(counter.get() + 1);
                    max.set(max.get().max(counter.get()));
                    sleep(ms(1)).then(move |()| counter.set(counter.get() - 1))
                })
            }
        })
        .finally(move || {
            assert_eq!(c2.get(), 0u32);
            assert_ne!(m2.get(), 0u32);
        })
    })
});

engine_test_case!(test_shared_mutex_failed_func, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sm = SharedMutex::new();

        // verify that the shared_mutex is unlocked when func fails
        // SAFETY: the lock lives, in place, in this frame (or the do_with frame) until the returned future and the lock it gives are gone.
        let mut fut: Future<()> =
            unsafe { with_shared(&mut sm, || -> Result<(), ExceptionPtr> { Err(injected()) }) };
        assert!(matches!(fut.get(), Err(ref e) if e.is::<RuntimeError>()));

        // SAFETY: the lock lives, in place, in this frame (or the do_with frame) until the returned future and the lock it gives are gone.
        fut = unsafe { with_lock(&mut sm, || -> Result<(), ExceptionPtr> { Err(injected()) }) };
        assert!(matches!(fut.get(), Err(ref e) if e.is::<RuntimeError>()));

        assert!(sm.try_lock());
        sm.unlock();
        Ok(())
    })
});

// The closure is moved without running code, so the failure of moving the captured
// value is raised inside the call.
engine_test_case!(test_shared_mutex_throwing_func, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut sm = SharedMutex::new();
        struct X {
            x: i32,
        }
        let x_moved = || -> Result<(), ExceptionPtr> {
            Err(ExceptionPtr::new(RuntimeError("X moved".to_string())))
        };

        // verify that the shared_mutex is unlocked when func move fails
        let x = X { x: 0 };
        // SAFETY: the lock lives, in place, in this frame (or the do_with frame) until the returned future and the lock it gives are gone.
        let mut fut: Future<()> = unsafe {
            with_shared(&mut sm, move || {
                let _ = x.x;
                x_moved()
            })
        };
        assert!(matches!(fut.get(), Err(ref e) if e.is::<RuntimeError>()));

        let x = X { x: 0 };
        // SAFETY: the lock lives, in place, in this frame (or the do_with frame) until the returned future and the lock it gives are gone.
        fut = unsafe {
            with_lock(&mut sm, move || {
                let _ = x.x;
                x_moved()
            })
        };
        assert!(matches!(fut.get(), Err(ref e) if e.is::<RuntimeError>()));

        assert!(sm.try_lock());
        sm.unlock();
        Ok(())
    })
});

engine_test_case!(test_shared_mutex_failed_lock, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        #[cfg(feature = "alloc_failure_injection")]
        {
            use teseladb::engine::util::alloc_failure_injector::memory::{
                BadAlloc, local_failure_injector,
            };
            let mut sm = SharedMutex::new();

            // if l.lock() fails neither the function nor l.unlock()
            // should be called.
            sm.lock().get()?;
            local_failure_injector().fail_after(0);
            // SAFETY: the lock lives, in place, in this frame (or the do_with frame) until the returned future and the lock it gives are gone.
            let r: Future<()> =
                unsafe { with_shared(&mut sm, || -> Future<()> { panic!("must not be reached") }) };
            assert!(matches!(r.get(), Err(ref e) if e.is::<BadAlloc>()));

            local_failure_injector().fail_after(0);
            // SAFETY: the lock lives, in place, in this frame (or the do_with frame) until the returned future and the lock it gives are gone.
            let r: Future<()> =
                unsafe { with_lock(&mut sm, || -> Future<()> { panic!("must not be reached") }) };
            assert!(matches!(r.get(), Err(ref e) if e.is::<BadAlloc>()));
            sm.unlock();

            local_failure_injector().cancel();
        }
        Ok(())
    })
});

#[derive(Debug, Clone)]
struct ExpectedException {
    value: i32,
}

impl fmt::Display for ExpectedException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected test error")
    }
}

impl std::error::Error for ExpectedException {}

#[derive(Debug, Clone)]
struct MovedException {
    count: i32,
}

impl fmt::Display for MovedException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected test error")
    }
}

impl std::error::Error for MovedException {}

/// A value whose moves can fail: a Rust move runs no code, so the failing moves are counted
/// explicitly: each hand-off of the value calls `moved()`, which fails while `count`
/// has not reached `delay`.
struct ThrowOnMove {
    value: i32,
    delay: i32,
    count: i32,
}

impl ThrowOnMove {
    fn new(v: i32, d: i32) -> Self {
        ThrowOnMove { value: v, delay: d, count: 0 }
    }

    fn moved(self) -> Result<Self, ExceptionPtr> {
        let count = self.count + 1;
        if count >= self.delay {
            return Err(ExceptionPtr::new(MovedException { count }));
        }
        Ok(ThrowOnMove { value: self.value, delay: self.delay, count })
    }
}

fn check_typed_return_nothrow(use_lock: bool) -> Result<(), ExceptionPtr> {
    let mut sm = SharedMutex::new();
    let expected = 42;

    let run =
        |sm: &mut SharedMutex, f: Box<dyn FnOnce() -> Result<i32, ExceptionPtr>>| -> Future<i32> {
            // SAFETY: the lock lives, in place, in this frame (or the do_with frame) until the returned future and the lock it gives are gone.
            if use_lock { unsafe { with_lock(sm, f) } } else { unsafe { with_shared(sm, f) } }
        };

    let res = run(&mut sm, Box::new(move || Ok(expected))).get()?;
    assert_eq!(res, expected);

    match run(
        &mut sm,
        Box::new(move || {
            if expected == 42 {
                return Err(ExceptionPtr::new(ExpectedException { value: expected }));
            }
            Ok(expected)
        }),
    )
    .get()
    {
        Ok(_) => panic!("No exception was thrown"),
        Err(e) => match e.downcast_ref::<ExpectedException>() {
            Some(ee) => assert_eq!(ee.value, expected),
            None => panic!("Unexpected exception type: {}", e),
        },
    }
    Ok(())
}

fn check_typed_return_throwing_move(use_lock: bool) -> Result<(), ExceptionPtr> {
    let mut sm = SharedMutex::new();
    let expected_value = 42;
    let mut done = false;
    let mut move_delay = 0;
    while !done {
        let exp = ThrowOnMove::new(expected_value, move_delay);
        let f = move || -> Result<i32, ExceptionPtr> {
            let expected = exp.moved()?;
            Ok(expected.value)
        };
        let r = if use_lock {
            // SAFETY: the lock lives, in place, in this frame (or the do_with frame) until the returned future and the lock it gives are gone.
            unsafe { with_lock(&mut sm, f) }
        } else {
            // SAFETY: as above.
            unsafe { with_shared(&mut sm, f) }
        };
        match r.get() {
            Ok(res) => {
                assert_eq!(res, expected_value);
                done = true;
            }
            Err(e) if e.downcast_ref::<MovedException>().is_some() => {
                let _ = e.downcast_ref::<MovedException>().map(|m| m.count);
            }
            Err(e) => panic!("Unexpected exception type: {}", e),
        }
        move_delay += 1;
    }
    Ok(())
}

engine_test_case!(test_with_shared_typed_return_nothrow_move_func, {
    thread_async(move || check_typed_return_nothrow(false))
});

engine_test_case!(test_with_shared_typed_return_throwing_move_func, {
    thread_async(move || check_typed_return_throwing_move(false))
});

engine_test_case!(test_with_lock_typed_return_nothrow_move_func, {
    thread_async(move || check_typed_return_nothrow(true))
});

engine_test_case!(test_with_lock_typed_return_throwing_move_func, {
    thread_async(move || check_typed_return_throwing_move(true))
});

engine_test_case!(test_shared_mutex_locks, {
    make_coroutine(async move {
        let mut sm = SharedMutex::new();
        let smp: *mut SharedMutex = &mut sm;
        // SAFETY: `sm` outlives every lock taken on it here.
        {
            // SAFETY: the lock lives, in place, in this frame (or the do_with frame) until the returned future and the lock it gives are gone.
            let _ulock = unsafe { get_unique_lock(&mut *smp) }.await?;
            // SAFETY: the lock lives in place until the returned future and the lock it gives are gone; this reference lives for the statement only.
            assert!(!unsafe { &mut *smp }.try_lock());
            // SAFETY: the lock lives in place until the returned future and the lock it gives are gone; this reference lives for the statement only.
            assert!(!unsafe { &mut *smp }.try_lock_shared());
        }
        assert!(sm.try_lock());
        sm.unlock();

        {
            // SAFETY: the lock lives, in place, in this frame (or the do_with frame) until the returned future and the lock it gives are gone.
            let _slock1 = unsafe { get_shared_lock(&mut *smp) }.await?;
            // SAFETY: the lock lives in place until the returned future and the lock it gives are gone; this reference lives for the statement only.
            assert!(!unsafe { &mut *smp }.try_lock());
            // SAFETY: the lock lives in place until the returned future and the lock it gives are gone; this reference lives for the statement only.
            assert!(unsafe { &mut *smp }.try_lock_shared());
            // SAFETY: the lock lives, in place, in this frame (or the do_with frame) until the returned future and the lock it gives are gone.
            let _slock2 = unsafe { get_shared_lock(&mut *smp) }.await?;
            // This balances out the call to `try_lock_shared()` above.
            // SAFETY: the lock lives in place until the returned future and the lock it gives are gone; this reference lives for the statement only.
            unsafe { &mut *smp }.unlock_shared();
        }

        assert!(sm.try_lock());
        sm.unlock();
        Ok(())
    })
});

engine_test_case!(test_shared_mutex_exclusive_locks, {
    make_coroutine(async move {
        let mut sm = SharedMutex::new();
        let smp: *mut SharedMutex = &mut sm;
        let counter = Rc::new(Cell::new(0u32));

        CoroutineParallelForEach::new(0..10, move |_: i32| {
            let counter = counter.clone();
            // SAFETY: `sm` outlives the loop, awaited here.
            make_coroutine(async move {
                // SAFETY: the lock lives, in place, in this frame (or the do_with frame) until the returned future and the lock it gives are gone.
                let _ulock = unsafe { get_unique_lock(&mut *smp) }.await?;
                assert_eq!(counter.get(), 0u32);
                counter.set(counter.get() + 1);
                sleep(ms(1)).await?;
                counter.set(counter.get() - 1);
                assert_eq!(counter.get(), 0u32);
                Ok(())
            })
        })
        .await?;
        drop(sm);
        Ok(())
    })
});

engine_test_case!(test_shared_mutex_exception_locks, {
    make_coroutine(async move {
        let mut sm = SharedMutex::new();
        let smp: *mut SharedMutex = &mut sm;

        // Verify that the shared_mutex is unlocked when an exception is thrown.
        // SAFETY: `sm` outlives the locks.
        let r: Result<(), ExceptionPtr> = async {
            // SAFETY: the lock lives, in place, in this frame (or the do_with frame) until the returned future and the lock it gives are gone.
            let _slock = unsafe { get_shared_lock(&mut *smp) }.await?;
            Err(injected())
        }
        .await;
        match r {
            Err(e) if e.is::<RuntimeError>() => {
                assert!(sm.try_lock());
                sm.unlock();
            }
            Err(e) => panic!("Unexpected exception type: {}", e),
            Ok(()) => {}
        }

        let r: Result<(), ExceptionPtr> = async {
            // SAFETY: the lock lives, in place, in this frame (or the do_with frame) until the returned future and the lock it gives are gone.
            let _ulock = unsafe { get_unique_lock(&mut *smp) }.await?;
            Err(injected())
        }
        .await;
        match r {
            Err(e) if e.is::<RuntimeError>() => {
                assert!(sm.try_lock());
                sm.unlock();
            }
            Err(e) => panic!("Unexpected exception type: {}", e),
            Ok(()) => {}
        }

        assert!(sm.try_lock());
        sm.unlock();
        Ok(())
    })
});
