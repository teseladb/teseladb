//! Tests of `Sharded`: peering services that reach their container, `invoke_on` during stop,
//! `map_reduce`, failing constructors, sharded and per-shard arguments, and a coordinator
//! that syncs a map across shards. A peering service keeps the pointer to its container
//! that `start()` hands it through `ShardedService::set_container`; `container()` reads it.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::pin::{Pin, pin};

use teseladb::engine::core::future::{ExceptionPtr, Future, RuntimeError, make_ready_future};
use teseladb::engine::core::map_reduce::Adder;
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::sharded::{Sharded, ShardedService};
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine_test_case;

struct InvokeOnDuringStop {
    container: *mut Sharded<InvokeOnDuringStop>,
    flag: bool,
}

impl InvokeOnDuringStop {
    fn new() -> Self {
        InvokeOnDuringStop { container: std::ptr::null_mut(), flag: false }
    }

    // A mutable reference from a shared handle.
    #[allow(clippy::mut_from_ref)]
    fn container(&self) -> Pin<&mut Sharded<InvokeOnDuringStop>> {
        // SAFETY: the container outlives its services and stays pinned while they run.
        unsafe { Pin::new_unchecked(&mut *self.container) }
    }
}

impl ShardedService for InvokeOnDuringStop {
    fn stop(&mut self) -> Future<()> {
        self.container().invoke_on(0, |instance: &mut InvokeOnDuringStop| {
            instance.flag = true;
        })
    }

    fn set_container(&mut self, container: *mut Sharded<Self>) {
        self.container = container;
    }
}

impl Drop for InvokeOnDuringStop {
    fn drop(&mut self) {
        if this_shard_id() == 0 {
            assert!(self.flag);
        }
    }
}

engine_test_case!(invoke_on_during_stop_test, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut s = pin!(Sharded::<InvokeOnDuringStop>::new());
        s.as_mut().start(InvokeOnDuringStop::new).get()?;
        s.as_mut().stop().get()?;
        Ok(())
    })
});

struct PeeringCounter {
    container: *mut Sharded<PeeringCounter>,
}

impl ShardedService for PeeringCounter {
    fn set_container(&mut self, container: *mut Sharded<Self>) {
        self.container = container;
    }
}

impl PeeringCounter {
    fn new() -> Self {
        PeeringCounter { container: std::ptr::null_mut() }
    }

    // A mutable reference from a shared handle.
    #[allow(clippy::mut_from_ref)]
    fn container(&self) -> Pin<&mut Sharded<PeeringCounter>> {
        // SAFETY: the container outlives its services and stays pinned while they run.
        unsafe { Pin::new_unchecked(&mut *self.container) }
    }

    fn count(&self) -> Future<i32> {
        self.container()
            .map_reduce(Adder::<i32>::default(), |_pc: &mut PeeringCounter| make_ready_future(1))
    }

    fn count_from(&self, base: i32) -> Future<i32> {
        self.container().map_reduce0(
            |_pc: &mut PeeringCounter| make_ready_future(1),
            base,
            |a: i32, b: i32| a + b,
        )
    }

    fn count_from_const(&self, base: i32) -> Future<i32> {
        self.container().map_reduce0(
            |pc: &mut PeeringCounter| pc.get_1_c(),
            base,
            |a: i32, b: i32| a + b,
        )
    }

    fn count_from_mutate(&mut self, base: i32) -> Future<i32> {
        self.container().map_reduce0(
            |pc: &mut PeeringCounter| pc.get_1_m(),
            base,
            |a: i32, b: i32| a + b,
        )
    }

    fn count_const(&self) -> Future<i32> {
        self.container().map_reduce(Adder::<i32>::default(), |pc: &mut PeeringCounter| pc.get_1_c())
    }

    fn count_mutate(&mut self) -> Future<i32> {
        self.container().map_reduce(Adder::<i32>::default(), |pc: &mut PeeringCounter| pc.get_1_m())
    }

    fn get_1_c(&self) -> Future<i32> {
        make_ready_future::<i32>(1)
    }

    fn get_1_m(&mut self) -> Future<i32> {
        make_ready_future::<i32>(1)
    }
}

engine_test_case!(test_const_map_reduces, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut c = pin!(Sharded::<PeeringCounter>::new());
        c.as_mut().start(PeeringCounter::new).get()?;

        // SAFETY: on this shard, with no other reference to the local
        // service live; each reference lives for its statement.
        assert_eq!(unsafe { c.as_mut().local() }.count().get()?, Smp::count() as i32);
        // SAFETY: on this shard, with no other reference to the local service live; the reference lives for its statement.
        assert_eq!(unsafe { c.as_mut().local() }.count_from(1).get()?, Smp::count() as i32 + 1);

        c.as_mut().stop().get()?;
        Ok(())
    })
});

engine_test_case!(test_member_map_reduces, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut c = pin!(Sharded::<PeeringCounter>::new());
        c.as_mut().start(PeeringCounter::new).get()?;

        // SAFETY: on this shard, with no other reference to the local
        // service live; each reference lives for its statement.
        assert_eq!(unsafe { c.as_mut().local() }.count_const().get()?, Smp::count() as i32);
        // SAFETY: on this shard, with no other reference to the local service live; the reference lives for its statement.
        assert_eq!(unsafe { c.as_mut().local() }.count_mutate().get()?, Smp::count() as i32);
        assert_eq!(
            // SAFETY: on this shard, with no other reference to the local service live; the reference lives for its statement.
            unsafe { c.as_mut().local() }.count_from_const(1).get()?,
            Smp::count() as i32 + 1
        );
        assert_eq!(
            // SAFETY: on this shard, with no other reference to the local service live; the reference lives for its statement.
            unsafe { c.as_mut().local() }.count_from_mutate(1).get()?,
            Smp::count() as i32 + 1
        );
        c.as_mut().stop().get()?;
        Ok(())
    })
});

struct MyData {
    x: i32,
}

impl ShardedService for MyData {
    fn stop(&mut self) -> Future<()> {
        make_ready_future(())
    }
}

engine_test_case!(invoke_map_returns_non_future_value, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut s = pin!(Sharded::<MyData>::new());
        s.as_mut().start(|| MyData { x: 1 }).get()?;
        s.as_mut()
            .map(|m: &mut MyData| make_ready_future(m.x))
            .then(|results: Vec<i32>| {
                for x in &results {
                    assert!(*x == 1);
                }
            })
            .get()?;
        s.as_mut().stop().get()?;
        Ok(())
    })
});

engine_test_case!(invoke_map_returns_future_value, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut s = pin!(Sharded::<MyData>::new());
        s.as_mut().start(|| MyData { x: 1 }).get()?;
        s.as_mut()
            .map(|m: &mut MyData| make_ready_future::<i32>(m.x))
            .then(|results: Vec<i32>| {
                for x in &results {
                    assert!(*x == 1);
                }
            })
            .get()?;
        s.as_mut().stop().get()?;
        Ok(())
    })
});

engine_test_case!(invoke_map_returns_future_value_from_thread, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut s = pin!(Sharded::<MyData>::new());
        s.as_mut().start(|| MyData { x: 1 }).get()?;
        s.as_mut()
            .map(|m: &mut MyData| {
                let mp: *mut MyData = m;
                // SAFETY: the service outlives the map, waited for below.
                thread_async(move || -> Result<i32, ExceptionPtr> { Ok(unsafe { (*mp).x }) })
            })
            .then(|results: Vec<i32>| {
                for x in &results {
                    assert!(*x == 1);
                }
            })
            .get()?;
        s.as_mut().stop().get()?;
        Ok(())
    })
});

/// A service whose constructor always fails.
struct FailToStart;

impl ShardedService for FailToStart {}

engine_test_case!(failed_sharded_start_doesnt_hang, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut s = pin!(Sharded::<FailToStart>::new());
        s.as_mut()
            .start(|| -> Result<FailToStart, ExceptionPtr> {
                Err(ExceptionPtr::new(RuntimeError("0".to_string())))
            })
            .then_wrapped(|mut fut: Future<()>| fut.ignore_ready_future())
            .get()?;
        Ok(())
    })
});

struct Argument {
    x: i32,
}

impl Argument {
    fn new() -> Self {
        Argument { x: this_shard_id() as i32 }
    }

    fn get(&self) -> i32 {
        self.x
    }
}

impl ShardedService for Argument {}

struct Service;

impl ShardedService for Service {}

impl Service {
    fn fn_local(&mut self, arg: &mut Argument) {
        assert_eq!(arg.get(), this_shard_id() as i32);
    }

    fn fn_sharded(&mut self, arg: Pin<&mut Sharded<Argument>>) {
        // SAFETY: on this shard, with no other reference to the local argument live.
        assert_eq!(unsafe { arg.local() }.get(), this_shard_id() as i32);
    }

    fn fn_sharded_param(&mut self, arg: i32) {
        assert_eq!(arg, this_shard_id() as i32);
    }
}

engine_test_case!(invoke_on_all_sharded_arg, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut srv = pin!(Sharded::<Service>::new());
        srv.as_mut().start(|| Service).get()?;
        let mut arg = pin!(Sharded::<Argument>::new());
        arg.as_mut().start(Argument::new).get()?;
        // SAFETY: the pointer keeps the pin's guarantee: the container is not moved.
        let argp = unsafe { arg.as_mut().get_unchecked_mut() } as *mut Sharded<Argument> as usize;

        // A sharded argument: each shard receives `arg.local()`.
        srv.as_mut()
            .invoke_on_all(move |s: &mut Service| {
                // SAFETY: `arg` is pinned in this frame and outlives them; each
                // closure runs on its shard and reaches only that shard's instance, with no other
                // reference to it live.
                s.fn_local(unsafe {
                    Pin::new_unchecked(&mut *(argp as *mut Sharded<Argument>)).local()
                })
            })
            .get()?;
        srv.as_mut()
            .invoke_on_all(move |s: &mut Service| {
                // SAFETY: `arg` is pinned in this frame and outlives the call; each closure runs on its shard and reaches only that shard's instance, with no other reference to it live.
                s.fn_sharded(unsafe { Pin::new_unchecked(&mut *(argp as *mut Sharded<Argument>)) })
            })
            .get()?;
        // A per-shard argument, computed on each shard from `arg.local()`.
        srv.as_mut()
            .invoke_on_all(move |s: &mut Service| {
                // SAFETY: `arg` is pinned in this frame and outlives the call; each closure runs on its shard and reaches only that shard's instance, with no other reference to it live.
                let p = unsafe {
                    Pin::new_unchecked(&mut *(argp as *mut Sharded<Argument>)).local().get()
                };
                s.fn_sharded_param(p)
            })
            .get()?;

        srv.as_mut().stop().get()?;
        arg.as_mut().stop().get()?;
        Ok(())
    })
});

struct Checker;

impl ShardedService for Checker {}

impl Checker {
    fn fn_(&mut self, _a: i32) -> Future<()> {
        make_ready_future(())
    }
}

engine_test_case!(invoke_on_modifiers, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut srv = pin!(Sharded::<Checker>::new());
        srv.as_mut().start(|| Checker).get()?;
        let a = 42;
        srv.as_mut().invoke_on_all(move |s: &mut Checker| s.fn_(a)).get()?;
        srv.as_mut().invoke_on_all(move |s: &mut Checker| s.fn_(a)).get()?;
        srv.as_mut().invoke_on_others(move |s: &mut Checker| s.fn_(a)).get()?;
        srv.as_mut().invoke_on_others(move |s: &mut Checker| s.fn_(a)).get()?;
        srv.as_mut().stop().get()?;
        Ok(())
    })
});

struct CoordinatorSyncedShardMap {
    container: *mut Sharded<CoordinatorSyncedShardMap>,
    unsigned_per_shard: Vec<u32>,
    coordinator_id: u32,
}

impl ShardedService for CoordinatorSyncedShardMap {
    fn set_container(&mut self, container: *mut Sharded<Self>) {
        self.container = container;
    }
}

impl CoordinatorSyncedShardMap {
    fn new(coordinator_id: u32) -> Self {
        CoordinatorSyncedShardMap {
            container: std::ptr::null_mut(),
            unsigned_per_shard: vec![0; Smp::count() as usize],
            coordinator_id,
        }
    }

    fn sync(&mut self, value: u32) -> Future<()> {
        let shard_id = this_shard_id();
        // SAFETY: the container outlives its services and stays pinned while they run.
        unsafe { Pin::new_unchecked(&mut *self.container) }.invoke_on(
            self.coordinator_id,
            move |s: &mut CoordinatorSyncedShardMap| {
                s.unsigned_per_shard[shard_id as usize] = value;
            },
        )
    }

    fn get_synced(&self, shard_id: u32) -> u32 {
        assert!(this_shard_id() == self.coordinator_id);
        self.unsigned_per_shard[shard_id as usize]
    }
}

engine_test_case!(invoke_on_range_contiguous, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut s = pin!(Sharded::<CoordinatorSyncedShardMap>::new());
        let coordinator_id = this_shard_id();
        s.as_mut().start(move || CoordinatorSyncedShardMap::new(coordinator_id)).get()?;

        let mid = Smp::count() / 2;
        let half1 = 0..mid;
        let half1_id: u32 = 1;
        let half2 = mid..Smp::count();
        let half2_id: u32 = 2;

        let f1 = s
            .as_mut()
            .invoke_on_range(half1, move |s: &mut CoordinatorSyncedShardMap| s.sync(half1_id));
        let f2 = s
            .as_mut()
            .invoke_on_range(half2, move |s: &mut CoordinatorSyncedShardMap| s.sync(half2_id));
        f1.get()?;
        f2.get()?;

        let f3 = s.as_mut().invoke_on(coordinator_id, move |s: &mut CoordinatorSyncedShardMap| {
            for i in 0..mid {
                assert_eq!(half1_id, s.get_synced(i));
            }
            for i in mid..Smp::count() {
                assert_eq!(half2_id, s.get_synced(i));
            }
        });
        f3.get()?;

        s.as_mut().stop().get()?;
        Ok(())
    })
});

engine_test_case!(invoke_on_range_fragmented, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut s = pin!(Sharded::<CoordinatorSyncedShardMap>::new());
        let coordinator_id = this_shard_id();
        s.as_mut().start(move || CoordinatorSyncedShardMap::new(coordinator_id)).get()?;

        // The even shards.
        let even: Vec<u32> = (0..Smp::count()).filter(|i| i % 2 == 0).collect();
        let even_id: u32 = 1;
        let odd: Vec<u32> = (1..Smp::count()).filter(|i| i % 2 == 1).collect();
        let odd_id: u32 = 2;

        let f1 = s
            .as_mut()
            .invoke_on_range(even, move |s: &mut CoordinatorSyncedShardMap| s.sync(even_id));
        let f2 = s
            .as_mut()
            .invoke_on_range(odd, move |s: &mut CoordinatorSyncedShardMap| s.sync(odd_id));
        f1.get()?;
        f2.get()?;

        let f3 = s.as_mut().invoke_on(coordinator_id, move |s: &mut CoordinatorSyncedShardMap| {
            for i in (0..Smp::count()).step_by(2) {
                assert_eq!(even_id, s.get_synced(i));
            }
            for i in (1..Smp::count()).step_by(2) {
                assert_eq!(odd_id, s.get_synced(i));
            }
        });
        f3.get()?;

        s.as_mut().stop().get()?;
        Ok(())
    })
});
