#![cfg(target_os = "linux")]
//! A request to another shard, and its response, reach the other side while the shard that
//! queued them is busy: the queue between two shards publishes an item when it is queued, not
//! when its sender next polls its queues or has sixteen of them (002 run p51: a message waited
//! 0.41 ms on average between cores, several times along each write).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use teseladb::engine::core::future::{Future, make_ready_future};
use teseladb::engine::core::reactor::later;
use teseladb::engine::core::smp::Smp;
use teseladb::engine_test_case;

/// Longer than a task quota and than any wait the queue may add.
const BUSY: Duration = Duration::from_millis(40);
/// What a crossing may take on a loaded test machine.
const CROSSING: Duration = Duration::from_millis(15);

fn busy(d: Duration) {
    let from = Instant::now();
    while from.elapsed() < d {
        std::hint::spin_loop();
    }
}

static STARTED_AFTER_NS: AtomicU64 = AtomicU64::new(u64::MAX);

engine_test_case!(a_request_reaches_its_shard_while_the_sender_is_busy, {
    assert!(Smp::count() >= 2, "this test needs at least two shards");
    let sent = Instant::now();
    let reply = Smp::submit_to(1, move || -> Future<()> {
        STARTED_AFTER_NS.store(sent.elapsed().as_nanos() as u64, Ordering::Relaxed);
        make_ready_future(())
    });
    busy(BUSY);
    reply.then(|()| {
        let started = Duration::from_nanos(STARTED_AFTER_NS.load(Ordering::Relaxed));
        assert!(started < CROSSING, "the request started {started:?} after it was sent");
    })
});

engine_test_case!(a_response_reaches_its_sender_while_the_responder_is_busy, {
    assert!(Smp::count() >= 2, "this test needs at least two shards");
    let sent = Instant::now();
    Smp::submit_to(1, || -> Future<()> {
        // The answer is ready in a task of the shard, and the task right after it keeps the
        // shard running before it polls its queues again.
        let answer = later();
        let _ = later().then(|()| busy(BUSY));
        answer
    })
    .then(move |()| {
        let back = sent.elapsed();
        assert!(back < CROSSING, "the response came {back:?} after the request was sent");
        // The test ends once the shard is free again: its busy task would hold the request of
        // the test that runs next (a whole run of the battery had it started 39.9 ms late).
        Smp::submit_to(1, || -> Future<()> { make_ready_future(()) })
    })
});
