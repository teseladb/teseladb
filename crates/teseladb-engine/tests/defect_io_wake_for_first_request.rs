#![cfg(target_os = "linux")]
//! When an I/O stream reserves capacity for several queued requests, the reactor is asked to
//! wake when the first of them can be dispatched, not when the whole reservation is
//! available: with idle polling off, it slept past the first request's turn (for eight
//! 4 KiB requests at 1000 requests per second, about 8 ms instead of 1 ms). With no request
//! left (they were cancelled) there is nothing to wake for.

#![allow(unsafe_code)]

use std::pin::pin;
use std::time::Duration;

use teseladb::engine::core::fair_queue::ClockTypeTimePoint;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::internal::io_request::{IoDirectionAndLength, IoRequest};
use teseladb::engine::core::internal::io_sink::IoSink;
use teseladb::engine::core::io_intent::IoIntent;
use teseladb::engine::core::io_queue::internal::{PriorityClass, get_fair_group};
use teseladb::engine::core::io_queue::{IoGroup, IoGroupPtr, IoQueue, IoQueueConfig, IovecKeeper};
use teseladb::engine::core::reactor::IoCompletion;
use teseladb::engine::core::scheduling::current_scheduling_group;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::util::shared_token_bucket::internal::{
    Period, RateResolution, SteadyClock, TokenBucketClock,
};
use teseladb::engine_test_case;

/// The size of each request.
const REQUEST_SIZE: usize = 4096;

/// A queue for a disk of 1 GiB/s and 1000 requests per second, as the reactor would
/// configure one queue of one group for it, with a 100 ms latency goal.
fn config(mountpoint: &str) -> IoQueueConfig {
    let base = IoQueue::READ_REQUEST_BASE_COUNT as u64;
    IoQueueConfig {
        id: 0,
        blocks_count_rate: (base * (1u64 << 30)) >> IoQueue::BLOCK_SIZE_SHIFT,
        disk_blocks_write_to_read_multiplier: base as u32,
        req_count_rate: base * 1000,
        disk_req_write_to_read_multiplier: base as u32,
        mountpoint: mountpoint.to_string(),
        rate_limit_duration: Duration::from_millis(100),
        block_count_limit_min: (64 << 10) >> IoQueue::BLOCK_SIZE_SHIFT,
        ..IoQueueConfig::default()
    }
}

/// An I/O queue over a test sink, never kicked by a timer. The queue lives in a block of its
/// own, reached through its root pointer as the reactor keeps it, and goes before the sink.
struct TestQueue {
    queue: *mut IoQueue,
    sink: Box<IoSink>,
    _group: IoGroupPtr,
}

impl TestQueue {
    fn new(cfg: IoQueueConfig) -> Self {
        let group: IoGroupPtr = IoGroupPtr::new(IoGroup::new(cfg, 1).expect("io_group"));
        let mut sink = Box::new(IoSink::new());
        let queue =
            Box::into_raw(Box::new(std::mem::MaybeUninit::<IoQueue>::uninit())) as *mut IoQueue;
        // SAFETY: `queue` is a fresh, aligned, uninitialized block that stays put until `Drop`
        // destroys the queue in it; the sink lives in its box, dropped after the queue.
        unsafe { IoQueue::construct_in_place(queue, group.clone(), &mut *sink) }.expect("io_queue");
        TestQueue { queue, sink, _group: group }
    }

    // A mutable reference from a shared handle, as the reactor reaches its queues.
    #[allow(clippy::mut_from_ref)]
    fn q(&self) -> &mut IoQueue {
        // SAFETY: the root pointer of the live queue; each use is a short access with no
        // other reference to the queue live.
        unsafe { &mut *self.queue }
    }

    fn write(&self, pos: usize, intent: *mut IoIntent) -> Future<usize> {
        let pc = PriorityClass::new(&current_scheduling_group());
        let dnl = IoDirectionAndLength::new(IoDirectionAndLength::WRITE_IDX, REQUEST_SIZE);
        let req = IoRequest::make_write(0, pos as u64, std::ptr::null(), REQUEST_SIZE, false);
        // SAFETY: the queue's root pointer, with no reference to the queue live.
        unsafe { IoQueue::queue_request(self.queue, pc, dnl, req, intent, IovecKeeper::new()) }
    }

    /// Uses up the initial burst allowance, as earlier I/O would, and polls the queue so
    /// that it reserves capacity for the queued requests.
    fn reserve_queued_capacity(&self) {
        let fg = get_fair_group(self.q(), 0);
        fg.replenish_capacity(<SteadyClock as TokenBucketClock>::now());
        fg.grab_capacity(fg.maximum_capacity());
        self.poll();
    }

    /// How long one request's capacity takes to replenish, in nanoseconds.
    fn first_request_ns(&self) -> f64 {
        let fg = get_fair_group(self.q(), 0);
        let dnl = IoDirectionAndLength::new(IoDirectionAndLength::WRITE_IDX, REQUEST_SIZE);
        nanos(fg.capacity_duration(self.q().request_capacity(dnl)))
    }

    fn poll(&self) {
        // SAFETY: the queue's root pointer, with no reference to the queue live.
        unsafe { IoQueue::poll_io_queue(self.queue) };
    }

    /// Replenishes, dispatches and completes what can go.
    fn kick(&mut self) {
        get_fair_group(self.q(), 0).replenish_capacity(<SteadyClock as TokenBucketClock>::now());
        self.poll();
        self.sink.drain(|rq: &IoRequest, desc: *mut IoCompletion| -> bool {
            // SAFETY: `desc` is the live completion of the request the sink hands out.
            unsafe { IoCompletion::complete_with(desc, rq.as_write().size as isize) };
            true
        });
    }
}

impl Drop for TestQueue {
    fn drop(&mut self) {
        // SAFETY: `queue` is the root pointer of the queue built in `new`, in a block of its
        // own that nothing else frees; no reference to it is live.
        unsafe {
            std::ptr::drop_in_place(self.queue);
            drop(Box::from_raw(self.queue as *mut std::mem::MaybeUninit<IoQueue>));
        }
    }
}

/// A duration of the token bucket, in nanoseconds.
fn nanos<P: Period>(d: RateResolution<P>) -> f64 {
    d.count() * P::NUM / P::DEN * 1e9
}

fn now_ns() -> ClockTypeTimePoint {
    <SteadyClock as TokenBucketClock>::now()
}

engine_test_case!(test_pending_io_wakes_for_first_request, {
    thread_async(|| -> Result<(), ExceptionPtr> {
        let mut tq = TestQueue::new(config("pending-first-request"));
        let mut requests: Vec<Future<usize>> =
            (0..8).map(|i| tq.write(i * REQUEST_SIZE, std::ptr::null_mut())).collect();

        tq.reserve_queued_capacity();
        let first = tq.first_request_ns();
        let wakeup = tq.q().next_pending_aio();
        let after = now_ns();
        assert!(wakeup != ClockTypeTimePoint::MAX, "no reservation is pending");
        let wait_ns = (wakeup - after) as f64;
        eprintln!(
            "wake-up in {:.3} ms; first request in {:.3} ms",
            wait_ns / 1e6,
            first / 1e6
        );
        assert!(
            wait_ns <= first * 2.0,
            "the queue wakes in {:.3} ms; its first request can go in {:.3} ms",
            wait_ns / 1e6,
            first / 1e6
        );

        for _ in 0..100 {
            if requests.iter().all(|r| r.available()) {
                break;
            }
            tq.kick();
            sleep(Duration::from_millis(1)).get()?;
        }
        for r in requests.drain(..) {
            assert!(r.available());
            assert_eq!(r.get()?, REQUEST_SIZE);
        }
        Ok(())
    })
});

engine_test_case!(test_pending_io_has_no_wakeup_after_cancellation, {
    thread_async(|| -> Result<(), ExceptionPtr> {
        let tq = TestQueue::new(config("pending-cancelled-request"));
        let mut intent = pin!(IoIntent::new());
        // SAFETY: the intent is pinned and outlives the request, which resolves below.
        let ip: *mut IoIntent = unsafe { intent.as_mut().get_unchecked_mut() };
        let request = tq.write(0, ip);
        tq.reserve_queued_capacity();
        assert!(tq.q().next_pending_aio() != ClockTypeTimePoint::MAX, "no reservation is pending");
        intent.as_mut().cancel();
        tq.poll();
        assert_eq!(tq.q().next_pending_aio(), ClockTypeTimePoint::MAX, "a wake-up for no request");
        assert!(request.get().is_err(), "the cancelled request did not fail");
        Ok(())
    })
});
