//! Microbenchmarks of the fair queue: queueing and dispatching requests under contention,
//! with a fair group local to the shard or shared by the shards.

// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/perf_tests.rs"]
mod perf_tests;

use std::pin::Pin;

use teseladb::engine::core::fair_queue::{
    CapacityT, ClassId, FairGroup, FairGroupConfig, FairQueue, FairQueueConfig, FairQueueEntry,
};
use teseladb::engine::core::future::{Future, make_ready_future};
use teseladb::engine::core::loop_::{do_until, parallel_for_each};
use teseladb::engine::core::sharded::{Sharded, ShardedService};
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::when_all::when_all_succeed;

const CID: ClassId = 0;

/// The members are declared so that they drop in a safe order: `sfq`, `fq`, then the
/// `fg` that `fq` points at.
pub struct LocalFqAndClass {
    executed: u32,
    sfq: FairQueue,
    fq: FairQueue,
    // Owns the group `fq` points to: kept alive, never read.
    #[allow(dead_code)]
    fg: Box<FairGroup>,
}

impl LocalFqAndClass {
    fn fg_config() -> FairGroupConfig {
        FairGroupConfig::default()
    }

    #[inline]
    fn queue(&mut self, local: bool) -> &mut FairQueue {
        if local { &mut self.fq } else { &mut self.sfq }
    }

    fn new(sfg: &FairGroup) -> Self {
        let fg = Box::new(FairGroup::new(Self::fg_config(), 1).expect("fair_group"));
        // SAFETY: `fg` is boxed (it does not move with the struct) and is dropped after `fq`.
        let mut fq = unsafe { FairQueue::new(&*fg, FairQueueConfig::default()) };
        // SAFETY: the shared group outlives the sharded service, which is stopped before it.
        let mut sfq = unsafe { FairQueue::new(sfg, FairQueueConfig::default()) };
        fq.register_priority_class(CID, 1);
        sfq.register_priority_class(CID, 1);
        LocalFqAndClass { executed: 0, sfq, fq, fg }
    }
}

impl Drop for LocalFqAndClass {
    fn drop(&mut self) {
        self.fq.unregister_priority_class(CID);
        self.sfq.unregister_priority_class(CID);
    }
}

impl ShardedService for LocalFqAndClass {}

/// A queued request. `ent` is the first field, so a pointer to it is a pointer to the
/// entry.
#[repr(C)]
struct LocalFqEntry {
    ent: FairQueueEntry,
    submit: Box<dyn FnMut()>,
}

impl LocalFqEntry {
    fn new<F: FnMut() + 'static>(cap: CapacityT, f: F) -> Self {
        LocalFqEntry { ent: FairQueueEntry::new(cap), submit: Box::new(f) }
    }
}

/// The fixture of the fair queue benchmarks: one local fair queue per shard over a fair
/// group shared by all shards.
#[allow(non_camel_case_types)]
pub struct perf_fair_queue {
    local_fq: Pin<Box<Sharded<LocalFqAndClass>>>,
    // Owns the group every shard's `sfq` points to: kept alive, never read.
    #[allow(dead_code)]
    shared_fg: Box<FairGroup>,
}

// SAFETY: the runner builds the fixture in set_up(), runs it and drops it in tear_down(), all
// on shard 0's reactor thread; the registered test only crosses threads while it holds none.
unsafe impl Send for perf_fair_queue {}

const REQUESTS_TO_DISPATCH: u32 = 1000;

impl perf_fair_queue {
    fn fg_config() -> FairGroupConfig {
        FairGroupConfig::default()
    }

    fn test(&mut self, loc: bool) -> Future<()> {
        let invokers = self.local_fq.as_mut().invoke_on_all(move |local: &mut LocalFqAndClass| {
            let localp: *mut LocalFqAndClass = local;
            parallel_for_each(0..REQUESTS_TO_DISPATCH, move |_dummy: u32| {
                // SAFETY: the shard's service outlives the test run.
                let local = unsafe { &mut *localp };
                let cap = local
                    .queue(loc)
                    .tokens_capacity(1.0 / f64::from(i32::MAX) + 1.0 / f64::from(i32::MAX));
                let req = Box::new(LocalFqEntry::new(cap, move || {
                    // SAFETY: `localp` points at this shard's service, which outlives the test run; the request callback runs on this shard after the closure above has returned, so no other reference to it is live.
                    let local = unsafe { &mut *localp };
                    local.executed += 1;
                    local.queue(loc).notify_request_finished(cap);
                }));
                let req = Box::into_raw(req);
                // SAFETY: the entry is leaked until the dispatcher below dequeues and frees it,
                // so it stays alive and in place while queued (`req.release()`).
                unsafe { local.queue(loc).queue(CID, std::ptr::addr_of_mut!((*req).ent)) };
                make_ready_future(())
            })
        });
        let collectors =
            self.local_fq.as_mut().invoke_on_all(move |local: &mut LocalFqAndClass| {
                // Zeroing this counter must be here, otherwise should the collectors win the
                // execution order in when_all_succeed(), the do_until()'s stopping callback
                // would return true immediately and the queue would not be dispatched.
                //
                // At the same time, although this counter is incremented by the lambda from
                // invokers, it's not called until the fq.dispatch_requests() is, so there's no
                // opposite problem if zeroing it here.
                local.executed = 0;
                let localp: *mut LocalFqAndClass = local;
                do_until(
                    // SAFETY: as above.
                    move || unsafe { (*localp).executed == REQUESTS_TO_DISPATCH },
                    move || {
                        // SAFETY: `localp` points at this shard's service, which outlives the test run; the loop body runs after its condition has returned, so no other reference to it is live.
                        let local = unsafe { &mut *localp };
                        let dispatch = |ent: *mut FairQueueEntry| {
                            let le = ent as *mut LocalFqEntry;
                            // SAFETY: every queued entry is the `ent` of a boxed LocalFqEntry.
                            unsafe {
                                ((*le).submit)();
                                drop(Box::from_raw(le));
                            }
                        };
                        // SAFETY: the queue is live and the callback does not touch it.
                        unsafe { FairQueue::dispatch_requests(local.queue(loc), dispatch) };
                        make_ready_future(())
                    },
                )
            });
        when_all_succeed((invokers, collectors)).discard_result()
    }
}

impl Default for perf_fair_queue {
    fn default() -> Self {
        let shared_fg =
            Box::new(FairGroup::new(Self::fg_config(), Smp::count()).expect("fair_group"));
        let mut local_fq = Box::pin(Sharded::<LocalFqAndClass>::new());
        let sfg = &*shared_fg as *const FairGroup as usize;
        // SAFETY: the shared group is boxed and outlives the sharded service (stopped in drop).
        local_fq
            .as_mut()
            .start(move || LocalFqAndClass::new(unsafe { &*(sfg as *const FairGroup) }))
            .get()
            .expect("start");
        perf_fair_queue { local_fq, shared_fg }
    }
}

impl Drop for perf_fair_queue {
    fn drop(&mut self) {
        let _ = self.local_fq.as_mut().stop().get();
    }
}

teseladb::perf_test_f!(perf_fair_queue, contended_local, |s| -> Future<()> { s.test(true) });

teseladb::perf_test_f!(perf_fair_queue, contended_shared, |s| -> Future<()> { s.test(false) });

pub(crate) fn main() {
    contended_local::register();
    contended_shared::register();
    perf_tests::main();
}
