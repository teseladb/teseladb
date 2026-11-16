//! Tests of `Distributed` (sharded services): starting and stopping on every shard,
//! `invoke_on`, `map_reduce` and its functor's lifetime, asynchronous services, bounded
//! concurrency of cross-shard requests, and services that depend on other sharded ones.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use teseladb::engine::core::distributed::Distributed;
use teseladb::engine::core::future::{ExceptionPtr, Future, RuntimeError, make_ready_future};
use teseladb::engine::core::loop_::parallel_for_each;
use teseladb::engine::core::reactor::yield_;
use teseladb::engine::core::semaphore::{Semaphore, SemaphoreTimedOut};
use teseladb::engine::core::shard_id::{ShardId, this_shard_id};
use teseladb::engine::core::sharded::{AsyncShardedService, Sharded, ShardedService};
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::smp::{
    Smp, SmpServiceGroup, SmpServiceGroupConfig, SmpSubmitToOptions, SmpTimeoutClock,
    create_smp_service_group, destroy_smp_service_group,
};
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::thread::async_;
use teseladb::engine::util::closeable::DeferredStop;
use teseladb::engine::util::defer::defer;
use teseladb::engine_test_case;

thread_local! {
    /// Whether this shard's [`AsyncService`] was dropped.
    static DELETED: Cell<bool> = const { Cell::new(false) };
}

/// An asynchronous sharded service: it keeps itself alive while its `run` is in flight.
struct AsyncService {
    asvc: AsyncShardedService,
}

impl Drop for AsyncService {
    fn drop(&mut self) {
        DELETED.with(|d| d.set(true));
    }
}

impl AsyncService {
    fn run(&mut self) {
        // SAFETY: `run` is only called through `invoke_on_all` of the container
        // that started this instance, on its shard.
        let r = unsafe { self.asvc.shared_from_this(self) };
        let this: *const Self = self;
        // Wait a while and check.
        let _ =
            sleep(Duration::from_millis(100 + 100 * u64::from(this_shard_id()))).then(move |()| {
                let _ref = &r;
                // SAFETY: `r` keeps the service alive until this continuation runs.
                unsafe { (*this).check() };
            });
    }

    fn check(&self) {
        assert!(!DELETED.with(Cell::get));
    }
}

impl ShardedService for AsyncService {
    fn stop(&mut self) -> Future<()> {
        make_ready_future(())
    }

    fn freed(&mut self) -> Option<Future<()>> {
        Some(self.asvc.freed())
    }
}

struct X;

impl X {
    fn echo(&self, arg: Sstring) -> Sstring {
        arg
    }

    fn cpu_id_squared(&self) -> i32 {
        let id = this_shard_id() as i32;
        id * id
    }
}

impl ShardedService for X {
    fn stop(&mut self) -> Future<()> {
        make_ready_future(())
    }
}

/// The handle to the container that `do_with_distributed` hands to its function: the container
/// lives in a box, in place, until the function's future and the `stop()` after it resolve.
struct Dist<T: ShardedService>(*mut Distributed<T>);

impl<T: ShardedService> Clone for Dist<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ShardedService> Copy for Dist<T> {}

impl<T: ShardedService> Dist<T> {
    /// The pinned container, for one call.
    fn get(self) -> Pin<&'static mut Distributed<T>> {
        // SAFETY: only `do_with_distributed` makes a `Dist`, and it keeps the box alive and
        // unmoved until everything its function chains, and the stop, have resolved; each
        // use is a single call, so no two of these references are used at once.
        unsafe { Pin::new_unchecked(&mut *self.0) }
    }
}

fn do_with_distributed<T: ShardedService, Func>(func: Func) -> Future<()>
where
    Func: FnOnce(Dist<T>) -> Future<()>,
{
    let x = Dist(Box::into_raw(Box::new(Distributed::<T>::new())));
    func(x).finally(move || x.get().stop()).finally(move || {
        // SAFETY: the box was leaked above and nothing uses it after the stop.
        drop(unsafe { Box::from_raw(x.0) });
    })
}

fn wrong_message(msg: &Sstring) -> Result<(), ExceptionPtr> {
    if *msg != "hello" {
        return Err(ExceptionPtr::new(RuntimeError("wrong message".to_string())));
    }
    Ok(())
}

engine_test_case!(test_that_each_core_gets_the_arguments, {
    do_with_distributed::<X, _>(|x| {
        x.get().start(|| X).then(move |()| {
            x.get().map_reduce(
                |msg: Sstring| wrong_message(&msg),
                |x: &mut X| make_ready_future(x.echo(Sstring::from("hello"))),
            )
        })
    })
});

engine_test_case!(test_functor_version, {
    do_with_distributed::<X, _>(|x| {
        x.get().start(|| X).then(move |()| {
            x.get().map_reduce(
                |msg: Sstring| wrong_message(&msg),
                |x: &mut X| make_ready_future(x.echo("hello".into())),
            )
        })
    })
});

struct Y {
    s: Sstring,
}

impl ShardedService for Y {
    fn stop(&mut self) -> Future<()> {
        make_ready_future(())
    }
}

engine_test_case!(test_constructor_argument_is_passed_to_each_core, {
    do_with_distributed::<Y, _>(|y| {
        let hello = Sstring::from("hello");
        y.get().start(move || Y { s: hello.clone() }).then(move |()| {
            y.get().invoke_on_all(|y: &mut Y| -> Result<(), ExceptionPtr> {
                if y.s != "hello" {
                    return Err(ExceptionPtr::new(RuntimeError(format!(
                        "expected message mismatch, is \"{}\"",
                        y.s
                    ))));
                }
                Ok(())
            })
        })
    })
});

fn expected_sum_of_squares() -> i64 {
    let n = i64::from(Smp::count()) - 1;
    (n * (n + 1) * (2 * n + 1)) / 6
}

engine_test_case!(test_map_reduce, {
    do_with_distributed::<X, _>(|x| {
        x.get().start(|| X).then(move |()| {
            x.get()
                .map_reduce0(
                    |x: &mut X| make_ready_future(x.cpu_id_squared()),
                    0i32,
                    |a: i32, b: i32| a + b,
                )
                .then(|result: i32| -> Result<(), ExceptionPtr> {
                    if i64::from(result) != expected_sum_of_squares() {
                        return Err(ExceptionPtr::new(RuntimeError(
                            "map_reduce failed".to_string(),
                        )));
                    }
                    Ok(())
                })
        })
    })
});

/// The `map` functor of the lifetime tests: each copy checks it is still alive when the
/// asynchronous part of the call runs. Each copy has its own
/// flag, set by its destructor; the continuation keeps a handle on the flag of the copy that
/// started it. The functor crosses shards, so the flag is atomic.
struct MapFn {
    destroyed: Arc<AtomicBool>,
}

impl Clone for MapFn {
    /// The copy starts alive.
    fn clone(&self) -> Self {
        MapFn { destroyed: Arc::new(AtomicBool::new(self.destroyed.load(Ordering::Relaxed))) }
    }
}

impl Drop for MapFn {
    /// Marks this copy as destroyed.
    fn drop(&mut self) {
        self.destroyed.store(true, Ordering::Relaxed);
    }
}

impl MapFn {
    fn new() -> Self {
        MapFn { destroyed: Arc::new(AtomicBool::new(false)) }
    }

    fn call(&self, x: &X) -> Future<i32> {
        let destroyed = self.destroyed.clone();
        let v = x.cpu_id_squared();
        yield_().then(move |()| {
            assert!(!destroyed.load(Ordering::Relaxed));
            make_ready_future(v)
        })
    }
}

engine_test_case!(test_map_reduce_lifetime, {
    do_with_distributed::<X, _>(|x| {
        x.get().start(|| X).then(move |()| {
            let result = Rc::new(Cell::new(0i64));
            let map = MapFn::new();
            let r2 = result.clone();
            let reduce_destroyed = Rc::new(Cell::new(false));
            let reduce = move |v: i32| {
                let r2 = r2.clone();
                let rd = reduce_destroyed.clone();
                yield_().then(move |()| {
                    assert!(!rd.get());
                    r2.set(r2.get() + i64::from(v));
                })
            };
            x.get().map_reduce(reduce, move |x: &mut X| map.call(x)).then(move |()| {
                assert_eq!(result.get(), expected_sum_of_squares());
            })
        })
    })
});

engine_test_case!(test_map_reduce0_lifetime, {
    do_with_distributed::<X, _>(|x| {
        x.get().start(|| X).then(move |()| {
            let map = MapFn::new();
            let reduce_destroyed = Rc::new(Cell::new(false));
            let reduce = move |res: i64, v: i32| {
                assert!(!reduce_destroyed.get());
                res + i64::from(v)
            };
            x.get().map_reduce0(move |x: &mut X| map.call(x), 0i64, reduce).then(|result: i64| {
                assert_eq!(result, expected_sum_of_squares());
            })
        })
    })
});

engine_test_case!(test_map_lifetime, {
    do_with_distributed::<X, _>(|x| {
        x.get().start(|| X).then(move |()| {
            let map = MapFn::new();
            x.get().map(move |x: &mut X| map.call(x)).then(|result: Vec<i32>| {
                assert_eq!(result.len(), Smp::count() as usize);
                for (i, r) in result.iter().enumerate() {
                    assert_eq!(*r as usize, i * i);
                }
            })
        })
    })
});

engine_test_case!(test_async, {
    do_with_distributed::<AsyncService, _>(|x| {
        x.get()
            .start(|| AsyncService { asvc: AsyncShardedService::new() })
            .then(move |()| x.get().invoke_on_all(|s: &mut AsyncService| s.run()))
    })
    .then(|()| sleep(Duration::from_millis(100 * (u64::from(Smp::count()) + 1))))
});

struct MyService {
    counter: i32,
}

impl MyService {
    fn up(&mut self) {
        self.counter += 1;
    }
}

impl ShardedService for MyService {
    fn stop(&mut self) -> Future<()> {
        make_ready_future(())
    }
}

engine_test_case!(test_invoke_on_others, {
    async_(|| -> Result<(), ExceptionPtr> {
        for c in 0..Smp::count() {
            Smp::submit_to(c, move || {
                thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
                    let mut s = pin!(Sharded::<MyService>::new());
                    s.as_mut().start(|| MyService { counter: 0 }).get()?;
                    s.as_mut().invoke_on_others(|s: &mut MyService| s.up()).get()?;
                    // SAFETY: no other reference to the local service is live, and no call on
                    // the container runs on this shard meanwhile.
                    if unsafe { s.as_mut().local() }.counter != 0 {
                        return Err(ExceptionPtr::new(RuntimeError("local modified".to_string())));
                    }
                    s.as_mut()
                        .invoke_on_all(move |remote: &mut MyService| -> Result<(), ExceptionPtr> {
                            if this_shard_id() != c && remote.counter != 1 {
                                return Err(ExceptionPtr::new(RuntimeError(
                                    "remote not modified".to_string(),
                                )));
                            }
                            Ok(())
                        })
                        .get()?;
                    s.as_mut().stop().get()?;
                    Ok(())
                })
            })
            .get()?;
        }
        Ok(())
    })
});

engine_test_case!(test_smp_invoke_on_others, {
    async_(|| -> Result<(), ExceptionPtr> {
        // Each shard appends to its own vector of a shared, unsynchronized vector of vectors.
        let calls: Arc<Vec<Mutex<Vec<u32>>>> = Arc::new(
            (0..Smp::count())
                .map(|_| Mutex::new(Vec::with_capacity(Smp::count() as usize)))
                .collect(),
        );

        let c = calls.clone();
        Smp::invoke_on_all(move || {
            let c = c.clone();
            let from = this_shard_id();
            Smp::invoke_on_others(move || {
                c[this_shard_id() as usize].lock().expect("calls").push(from);
            })
        })
        .get()?;

        for i in 0..Smp::count() {
            let v = calls[i as usize].lock().expect("calls");
            assert_eq!(v.len(), Smp::count() as usize - 1);
            for f in 0..Smp::count() {
                let r = v.iter().position(|&x| x == f);
                assert_eq!(r.is_none(), i == f);
            }
        }
        Ok(())
    })
});

struct RemoteWorker {
    current: u32,
    max_concurrent_observed: u32,
    expected_max: u32,
    sem: Semaphore,
}

impl RemoteWorker {
    fn new(expected_max: u32) -> Self {
        RemoteWorker {
            current: 0,
            max_concurrent_observed: 0,
            expected_max,
            sem: Semaphore::new(0),
        }
    }

    fn do_work(&mut self) -> Future<()> {
        self.current += 1;
        self.max_concurrent_observed = self.current.max(self.max_concurrent_observed);
        if self.max_concurrent_observed >= self.expected_max && self.sem.current() == 0 {
            self.sem.signal(Semaphore::max_counter());
        }
        let this: *mut Self = self;
        self.sem.wait(1).then(move |()| {
            // Sleep a bit to check if the concurrency goes over the max
            sleep(Duration::from_millis(100)).then(move |()| {
                // SAFETY: the worker outlives the bunch of requests waited for in the test.
                let me = unsafe { &mut *this };
                me.max_concurrent_observed = me.current.max(me.max_concurrent_observed);
                me.current -= 1;
            })
        })
    }

    fn do_remote_work(&mut self, t: ShardId, ssg: SmpServiceGroup) -> Future<()> {
        let this = self as *mut Self as usize;
        // SAFETY: as above; the remote shard only touches the worker in this request.
        Smp::submit_to_with(t, ssg, move || unsafe { (*(this as *mut Self)).do_work() })
    }
}

engine_test_case!(test_smp_service_groups, {
    async_(|| -> Result<(), ExceptionPtr> {
        let ssgc1 =
            SmpServiceGroupConfig { max_nonlocal_requests: 1, ..SmpServiceGroupConfig::default() };
        let ssg1 = create_smp_service_group(ssgc1).get()?;
        let ssgc2 = SmpServiceGroupConfig {
            max_nonlocal_requests: 1000,
            ..SmpServiceGroupConfig::default()
        };
        let ssg2 = create_smp_service_group(ssgc2).get()?;
        let other_shard: ShardId = Smp::count() - 1;
        let mut rm1 = RemoteWorker::new(1);
        let mut rm2 = RemoteWorker::new(1000);
        let (p1, p2): (*mut RemoteWorker, *mut RemoteWorker) = (&mut rm1, &mut rm2);
        // SAFETY: both workers outlive the bunches, waited for below.
        let bunch1 = parallel_for_each(0..20, move |_ignore: i32| unsafe {
            (*p1).do_remote_work(other_shard, ssg1)
        });
        // SAFETY: both workers outlive the bunches, waited for below.
        let bunch2 = parallel_for_each(0..2000, move |_ignore: i32| unsafe {
            (*p2).do_remote_work(other_shard, ssg2)
        });
        bunch1.get()?;
        bunch2.get()?;
        if Smp::count() > 1 {
            assert!(rm1.max_concurrent_observed == 1);
            assert!(rm2.max_concurrent_observed == 1000);
        }
        destroy_smp_service_group(ssg1).get()?;
        destroy_smp_service_group(ssg2).get()?;
        Ok(())
    })
});

engine_test_case!(test_smp_service_groups_re_construction, {
    // During development of the feature, we saw a bug where the vector
    // holding the groups did not expand correctly. This test triggers the
    // bug.
    async_(|| -> Result<(), ExceptionPtr> {
        let ssg1 = create_smp_service_group(SmpServiceGroupConfig::default()).get()?;
        let ssg2 = create_smp_service_group(SmpServiceGroupConfig::default()).get()?;
        destroy_smp_service_group(ssg1).get()?;
        let ssg3 = create_smp_service_group(SmpServiceGroupConfig::default()).get()?;
        destroy_smp_service_group(ssg2).get()?;
        destroy_smp_service_group(ssg3).get()?;
        Ok(())
    })
});

engine_test_case!(test_smp_timeout, {
    async_(|| -> Result<(), ExceptionPtr> {
        let ssgc1 =
            SmpServiceGroupConfig { max_nonlocal_requests: 1, ..SmpServiceGroupConfig::default() };
        let ssg1 = create_smp_service_group(ssgc1).get()?;

        let _guard = defer(move || {
            let _ = destroy_smp_service_group(ssg1).get();
        });

        let other_shard: ShardId = Smp::count() - 1;

        // Ugly but beats using sleeps.
        let mut_ = Arc::new(Mutex::new(()));
        let lk = mut_.lock().expect("lock");
        let m2 = mut_.clone();

        // Submitted to the remote shard.
        let fut1 = Smp::submit_to_with(other_shard, ssg1, move || {
            println!("Running request no. 1");
            let _lk = m2.lock().expect("lock");
            println!("Request no. 1 done");
        });
        // Consume the only unit from the semaphore.
        let fut2 = Smp::submit_to_with(other_shard, ssg1, || {
            println!("Running request no. 2 - done");
        });

        let fut_timedout = Smp::submit_to_with(
            other_shard,
            SmpSubmitToOptions::new(ssg1, SmpTimeoutClock::now() + Duration::from_millis(10)),
            || {
                println!("Running timed-out request - done");
            },
        );

        {
            let _notify = defer(move || drop(lk));

            match fut_timedout.get() {
                Ok(()) => {
                    return Err(ExceptionPtr::new(RuntimeError(
                        "smp::submit_to() didn't timeout as expected".to_string(),
                    )));
                }
                Err(e) => match e.downcast_ref::<SemaphoreTimedOut>() {
                    Some(te) => println!("Expected timeout received: {}", te.what()),
                    None => {
                        return Err(ExceptionPtr::with_nested(
                            RuntimeError(
                                "smp::submit_to() failed with unexpected exception".to_string(),
                            ),
                            e,
                        ));
                    }
                },
            }
        }

        fut1.get()?;
        fut2.get()?;
        Ok(())
    })
});

struct Dependency {
    val: u32,
}

impl ShardedService for Dependency {}

struct SomeService {
    ok: bool,
}

impl ShardedService for SomeService {}

impl SomeService {
    fn new(
        non_shard_dependent: u32,
        shard_dependent: u32,
        dep: &Dependency,
        shard_dependent_2: u32,
    ) -> Self {
        let ok = non_shard_dependent == 43
            && shard_dependent == this_shard_id() * 3
            && dep.val == this_shard_id() * 7
            && shard_dependent_2 == dep.val.wrapping_neg();
        SomeService { ok }
    }
}

engine_test_case!(test_sharded_parameter, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut s_dep = pin!(Sharded::<Dependency>::new());
        s_dep.as_mut().start(|| Dependency { val: this_shard_id() * 7 }).get()?;
        // SAFETY: the pointer keeps the pin's guarantee: the container is not moved.
        let depp =
            unsafe { s_dep.as_mut().get_unchecked_mut() } as *mut Sharded<Dependency> as usize;
        // SAFETY: as above; the guard stops the container at the end of this frame, after the
        // service that depends on it.
        let _undo1 = DeferredStop::new(unsafe { &mut *(depp as *mut Sharded<Dependency>) });

        let mut s_service = pin!(Sharded::<SomeService>::new());
        s_service
            .as_mut()
            .start(move || {
                // SAFETY: the dependency container outlives the service (stopped after it)
                // and stays pinned; each shard reads only its own instance, and no other
                // reference to it is live during the constructor.
                let dep =
                    unsafe { Pin::new_unchecked(&mut *(depp as *mut Sharded<Dependency>)).local() };
                SomeService::new(
                    43,                     // should be copied verbatim
                    this_shard_id() * 3,    // sharded_parameter([] { return this_shard_id() * 3; })
                    dep,                    // std::ref(s_dep)
                    dep.val.wrapping_neg(), // sharded_parameter([] (dependency& d) { return -d.val; }, std::ref(s_dep))
                )
            })
            .get()?;
        // SAFETY: as above.
        let svcp: *mut Sharded<SomeService> = unsafe { s_service.as_mut().get_unchecked_mut() };
        // SAFETY: as above; the guard stops the container at the end of this frame.
        let _undo2 = DeferredStop::new(unsafe { &mut *svcp });
        // SAFETY: the container is pinned in this frame; this call happens before the guard
        // stops it.
        let all_ok = unsafe { Pin::new_unchecked(&mut *svcp) }
            .map_reduce0(
                |s: &mut SomeService| make_ready_future(s.ok),
                true,
                |a: bool, b: bool| a && b,
            )
            .get()?;
        assert!(all_ok);
        Ok(())
    })
});
