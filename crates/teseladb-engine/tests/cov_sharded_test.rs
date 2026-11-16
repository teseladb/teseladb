//! Tests of sharded services beyond the common calls: services that see their container and
//! delay their stop while referenced, constructors that fail, calls to shards without an
//! instance or outside the shard range, mappers that fail, shard-dependent parameters, the
//! stop helper on an object that never started, and foreign pointers copied from another shard.
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::pin::pin;
use std::time::Duration;

use teseladb::engine::core::future::{
    ExceptionPtr, Future, make_exception_future, make_ready_future,
};
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::sharded::internal::{
    EitherShardedOrLocal, sharded_parallel_for_each, stop_sharded_instance,
    unwrap_sharded_parameter, unwrap_sharded_ref,
};
use teseladb::engine::core::sharded::{
    AsyncShardedService, ForeignPtr, NoShardedInstanceException, PeeringShardedService, Sharded,
    ShardedParameter, ShardedService,
};
use teseladb::engine::core::shared_ptr::SharedPtr;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::util::closeable::Stoppable;
use teseladb::engine_thread_test_case;

/// A service that knows its container and reports its destruction.
struct Peer {
    peer: PeeringShardedService<Peer>,
    tracker: AsyncShardedService,
    shard: u32,
}

impl ShardedService for Peer {
    fn freed(&mut self) -> Option<Future<()>> {
        Some(self.tracker.freed())
    }

    fn set_container(&mut self, container: *mut Sharded<Self>) {
        self.peer.set_container(container);
    }
}

thread_local! {
    /// A reference to shard 0's instance, kept to delay the stop.
    static HELD: RefCell<Option<SharedPtr<Peer>>> = const { RefCell::new(None) };
}

engine_thread_test_case!(services_see_their_container_and_delay_the_stop, {
    let mut s = pin!(Sharded::<Peer>::new());
    let addr = &*s as *const Sharded<Peer> as usize;
    s.as_mut()
        .start(|| Peer {
            peer: PeeringShardedService::default(),
            tracker: AsyncShardedService::default(),
            shard: this_shard_id(),
        })
        .get()
        .unwrap();
    // Every instance holds the address of the one container.
    let containers = s
        .as_mut()
        .map(|p: &mut Peer| Ok::<usize, ExceptionPtr>(p.peer.container() as usize))
        .get()
        .unwrap();
    assert_eq!(containers, vec![addr; Smp::count() as usize]);
    s.as_mut()
        .invoke_on(0, |p: &mut Peer| {
            // SAFETY: the instance was created by `Sharded::start` and is used on its shard.
            let me = unsafe { p.tracker.shared_from_this(p) };
            assert_eq!(me.shard, 0);
            HELD.with(|h| *h.borrow_mut() = Some(me));
        })
        .get()
        .unwrap();
    // The stop waits for the reference taken above.
    let stopping = s.as_mut().stop();
    sleep(Duration::from_millis(20)).get().unwrap();
    assert!(!stopping.available(), "an instance is still referenced");
    HELD.with(|h| h.borrow_mut().take());
    stopping.get().unwrap();
    assert!(!s.local_is_initialized());

    // A container that was never attached is null; the accessor reaches an attached one.
    let unattached = PeeringShardedService::<Peer>::new();
    assert!(unattached.container().is_null());
    let mut other = pin!(Sharded::<Peer>::default());
    let mut attached = PeeringShardedService::<Peer>::new();
    // SAFETY: only the address is taken; the object stays pinned.
    attached.set_container(unsafe { other.as_mut().get_unchecked_mut() } as *mut Sharded<Peer>);
    // SAFETY: `other` is alive and pinned, and no other reference to it is used meanwhile.
    let container = unsafe { attached.container_mut() };
    assert!(!container.local_is_initialized());
});

/// A plain service holding its shard.
struct Plain {
    shard: u32,
}

impl ShardedService for Plain {}

engine_thread_test_case!(a_failing_constructor_stops_the_built_instances, {
    let last = Smp::count() - 1;
    let mut s = pin!(Sharded::<Plain>::new());
    let e = s
        .as_mut()
        .start(move || -> Result<Plain, ExceptionPtr> {
            if this_shard_id() == last {
                Err(ExceptionPtr::new(std::io::Error::other("no room on the last shard")))
            } else {
                Ok(Plain { shard: this_shard_id() })
            }
        })
        .get()
        .unwrap_err();
    assert!(e.to_string().contains("no room on the last shard"), "{e}");
    assert!(!s.local_is_initialized(), "the instances built were stopped");
});

engine_thread_test_case!(calls_to_missing_or_invalid_shards_fail, {
    let mut s = pin!(Sharded::<Plain>::new());
    // Not started: no local instance.
    // SAFETY: nothing is borrowed from the (missing) instance.
    let e = unsafe { s.get_local_service() }.err().expect("not started");
    assert!(e.downcast_ref::<NoShardedInstanceException>().is_some());
    assert!(e.to_string().contains("Plain"), "{e}");
    // Stopping an object that never started does nothing.
    // SAFETY: the object is not moved out of its pin.
    Stoppable::stop(unsafe { s.as_mut().get_unchecked_mut() }).get().unwrap();

    s.as_mut().start(|| Plain { shard: this_shard_id() }).get().unwrap();
    let n = Smp::count();
    // A range with a shard id that does not exist fails, the valid ones still run.
    let e = s.as_mut().invoke_on_range(0..n + 1, |_p: &mut Plain| {}).get().unwrap_err();
    assert!(e.to_string().contains("Invalid shard id in range"), "{e}");
    s.as_mut()
        .invoke_on_range(0..n, |p: &mut Plain| assert_eq!(p.shard, this_shard_id()))
        .get()
        .unwrap();
    // A mapper that fails on one shard fails the whole map.
    let e = s
        .as_mut()
        .map(|p: &mut Plain| -> Future<u32> {
            if p.shard == 0 {
                make_exception_future(ExceptionPtr::new(std::io::Error::other("mapper failed")))
            } else {
                make_ready_future(p.shard)
            }
        })
        .get()
        .unwrap_err();
    assert!(e.to_string().contains("mapper failed"), "{e}");
    // The stop helper stops a started object.
    // SAFETY: the object is not moved out of its pin.
    Stoppable::stop(unsafe { s.as_mut().get_unchecked_mut() }).get().unwrap();
    assert!(!s.local_is_initialized());

    if n > 1 {
        // A single instance lives on shard 0 only: a call to shard 1 has no instance.
        let mut single = pin!(Sharded::<Plain>::new());
        single.as_mut().start_single(|| Plain { shard: this_shard_id() }).get().unwrap();
        let e = single
            .as_mut()
            .invoke_on(1, |p: &mut Plain| Ok::<u32, ExceptionPtr>(p.shard))
            .get()
            .unwrap_err();
        assert!(e.downcast_ref::<NoShardedInstanceException>().is_some());
        assert_eq!(
            single
                .as_mut()
                .invoke_on(0, |p: &mut Plain| Ok::<u32, ExceptionPtr>(p.shard))
                .get()
                .unwrap(),
            0
        );
        single.as_mut().stop().get().unwrap();
    }
});

engine_thread_test_case!(parameters_and_wrappers_resolve_on_the_shard, {
    let param = ShardedParameter::new(|| this_shard_id() * 10);
    assert_eq!(param.clone().evaluate(), 0);
    assert_eq!(unwrap_sharded_parameter(&param), 0);
    let on_others =
        Smp::submit_to(Smp::count() - 1, move || Ok::<u32, ExceptionPtr>(param.evaluate()))
            .get()
            .unwrap();
    assert_eq!(on_others, (Smp::count() - 1) * 10);

    let mut s = pin!(Sharded::<Plain>::new());
    s.as_mut().start(|| Plain { shard: this_shard_id() }).get().unwrap();
    {
        let mut either: EitherShardedOrLocal<'_, Plain> = unwrap_sharded_ref(s.as_mut());
        assert!(either.sharded().local_is_initialized());
        // SAFETY: no call of the object is in flight while the reference is used.
        let local = unsafe { either.local() };
        assert_eq!(local.shard, 0);
        stop_sharded_instance(local).get().unwrap();
    }
    s.as_mut().stop().get().unwrap();

    // The shard loop helper runs once per shard id and reports a failure.
    let seen = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let counter = seen.clone();
    sharded_parallel_for_each(3, move |i| {
        counter.set(counter.get() + i + 1);
        make_ready_future(())
    })
    .get()
    .unwrap();
    assert_eq!(seen.get(), 6);
    let e = sharded_parallel_for_each(2, |i| {
        if i == 1 {
            make_exception_future(ExceptionPtr::new(std::io::Error::other("shard loop")))
        } else {
            make_ready_future(())
        }
    })
    .get()
    .unwrap_err();
    assert!(e.to_string().contains("shard loop"));

    let plain = NoShardedInstanceException::default();
    assert_eq!(plain.to_string(), "sharded instance does not exist");
    assert_eq!(NoShardedInstanceException::new().to_string(), plain.to_string());
    assert_eq!(
        NoShardedInstanceException::with_type("T").to_string(),
        "sharded instance does not exist: T"
    );
});

engine_thread_test_case!(foreign_pointers_copy_and_release_on_their_owner, {
    let p = ForeignPtr::new(std::sync::Arc::new(5u32));
    assert_eq!(p.get_owner_shard(), 0);
    // SAFETY: on the owner shard the copy is made at once.
    let local_copy = unsafe { p.copy() }.get().unwrap();
    assert_eq!(*local_copy, 5);
    if Smp::count() > 1 {
        let addr = &p as *const ForeignPtr<std::sync::Arc<u32>> as usize;
        // From shard 1 the copy is made on shard 0 and owned there.
        let (owner, value) = Smp::submit_to(1, move || {
            // SAFETY: `p` stays alive, in place and unmodified until the copy resolves.
            let p = unsafe { &*(addr as *const ForeignPtr<std::sync::Arc<u32>>) };
            // SAFETY: as above.
            unsafe { p.copy() }.then(|c: ForeignPtr<std::sync::Arc<u32>>| {
                let r = (c.get_owner_shard(), *c.get().unwrap());
                drop(c);
                Ok::<(u32, u32), ExceptionPtr>(r)
            })
        })
        .get()
        .unwrap();
        assert_eq!((owner, value), (0, 5));
    }
    let null = ForeignPtr::<Box<u32>>::default();
    assert!(!null.is_some());
    // Dereferencing a null pointer panics.
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| *null + 1));
    assert!(r.is_err());
    drop(local_copy);
    drop(p);
});
