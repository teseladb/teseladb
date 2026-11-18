#![cfg(target_os = "linux")]
//! A pointer a sharded service takes to itself is one more owner of its instance: the count
//! grows by one, and `Sharded::stop` waits until the pointer is released. The pointer used to be
//! made from a `&Service`, through which the count next to the service may not be written, and
//! an optimized build dropped the increment: the instance was destroyed while the pointer was
//! still in use (`distributed_test` and `cov_sharded_test` failed, 18-11-2026).
#![allow(unsafe_code)]

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::time::{Duration, Instant};

use teseladb::engine::core::sharded::{
    AsyncShardedService, Sharded, ShardedService, this_shard_id,
};
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine_thread_test_case;

/// How long the service keeps the pointer to itself.
const KEPT: Duration = Duration::from_millis(100);

struct Service {
    tracker: AsyncShardedService<Service>,
}

/// Whether the continuation that keeps the pointer to shard 0's instance ran, and whether that
/// instance was destroyed before it did.
static RAN: AtomicBool = AtomicBool::new(false);
static DROPPED_BEFORE_IT_RAN: AtomicBool = AtomicBool::new(false);

impl Drop for Service {
    fn drop(&mut self) {
        if this_shard_id() == 0 && !RAN.load(SeqCst) {
            DROPPED_BEFORE_IT_RAN.store(true, SeqCst);
        }
    }
}

impl ShardedService for Service {
    fn async_service(&mut self) -> Option<&mut AsyncShardedService<Self>> {
        Some(&mut self.tracker)
    }
}

engine_thread_test_case!(a_pointer_a_service_takes_to_itself_owns_the_instance, {
    // Leaked: the test ends with the instances stopped, and a started `Sharded` must not move.
    let leaked = Box::leak(Box::new(Sharded::<Service>::new()));
    // SAFETY: the leaked value never moves.
    let mut s = unsafe { Pin::new_unchecked(leaked) };
    s.as_mut().start(|| Service { tracker: AsyncShardedService::new() }).get().unwrap();
    // SAFETY: shard 0 runs this test, and nothing else uses its instance now.
    let owners = unsafe { s.local_shared() }.use_count();

    s.as_mut()
        .invoke_on(0, |p: &mut Service| {
            // SAFETY: the tracker is inside the instance `start` created, on its shard.
            let me = unsafe { p.tracker.shared_from_this() };
            let _ = sleep(KEPT).then(move |()| {
                let _kept = &me;
                RAN.store(true, SeqCst);
            });
        })
        .get()
        .unwrap();
    // SAFETY: as above.
    assert_eq!(unsafe { s.local_shared() }.use_count(), owners + 1);

    let stopping = Instant::now();
    s.as_mut().stop().get().unwrap();
    assert!(!DROPPED_BEFORE_IT_RAN.load(SeqCst), "the instance died with its pointer in use");
    assert!(RAN.load(SeqCst));
    assert!(stopping.elapsed() >= KEPT * 4 / 5, "the stop took {:?}", stopping.elapsed());
});
