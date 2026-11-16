//! Tests of `FairQueue`: requests of classes with equal and unequal shares, costs and
//! capacities are dispatched in proportion to the shares, also over long and random runs
//! and when the shares change; a class cannot monopolize the queue beyond its preemption
//! allowance.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/minstd.rs"]
mod minstd;

use std::time::Duration;

use minstd::UniformIntDistribution;
use teseladb::engine::core::fair_queue::{
    CapacityT, ClassId, FairGroup, FairGroupConfig, FairQueue, FairQueueConfig, FairQueueEntry,
};
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::reactor::yield_;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::testing::random::testing::local_random;
use teseladb::engine::util::shared_token_bucket::internal::{SteadyClock, TokenBucketClock};
use teseladb::engine_test_case;

/// A queued request: `fqent` is the first member of a `#[repr(C)]` struct, so a pointer to
/// it is a pointer to the request.
#[repr(C)]
struct Request {
    fqent: FairQueueEntry,
    handle: Option<Box<dyn FnOnce(Request)>>,
    index: u32,
}

impl Request {
    fn new<F: FnOnce(Request) + 'static>(cap: CapacityT, index: u32, h: F) -> Self {
        Request { fqent: FairQueueEntry::new(cap), handle: Some(Box::new(h)), index }
    }

    /// Runs the handler, which takes the request out of
    /// its heap block, which is freed on return.
    // The box is the request's heap block, freed here.
    #[allow(clippy::boxed_local)]
    fn submit(self: Box<Self>) {
        let mut req = *self;
        if let Some(handle) = req.handle.take() {
            handle(req);
        }
    }

    /// Submits the request whose `fqent` is `ent`.
    fn submit_entry(ent: *mut FairQueueEntry) {
        // SAFETY: every entry queued by `do_op` is the first field of a leaked `Box<Request>`,
        // queued with the request's own pointer.
        let req = unsafe { Box::from_raw(ent as *mut Request) };
        req.submit();
    }
}

/// A fair group, its queue and the per-class results. The queue points to its group, so the group lives in a box that does
/// not move with the environment; the handlers point to the environment, which stays in
/// its test's frame.
struct TestEnv {
    fq: FairQueue,
    fg: Box<FairGroup>,
    results: Vec<i32>,
    exceptions: Vec<Vec<ExceptionPtr>>,
    nr_classes: ClassId,
    inflight: Vec<Request>,
}

/// A duration in the nanoseconds of `fair_group::clock_type` time points.
fn nanos(d: Duration) -> i64 {
    d.as_nanos() as i64
}

impl TestEnv {
    fn fg_config(cap: u32) -> FairGroupConfig {
        FairGroupConfig {
            rate_limit_duration: Duration::from_micros(u64::from(cap)),
            ..FairGroupConfig::default()
        }
    }

    fn fq_config() -> FairQueueConfig {
        FairQueueConfig { tau: Duration::from_micros(50), ..FairQueueConfig::default() }
    }

    fn drain(&mut self) {
        while self.tick(1) != 0 {}
    }

    fn new(capacity: u32) -> Self {
        let fg = Box::new(FairGroup::new(Self::fg_config(capacity), 1).expect("fair_group"));
        // SAFETY: the group lives in its box, which the environment owns next to the queue
        // and drops after it (`fq` is declared before `fg`).
        let fq = unsafe { FairQueue::new(&*fg, Self::fq_config()) };
        let env = TestEnv {
            fq,
            fg,
            results: Vec::new(),
            exceptions: Vec::new(),
            nr_classes: 0,
            inflight: Vec::new(),
        };
        // Move _fg._replenished_ts() to far future.
        // This will prevent any `maybe_replenish_capacity` calls (indirectly done by `FairQueue::dispatch_requests()`)
        // from replenishing tokens on its own, and ensure that the only source of replenishment will be tick().
        //
        // Otherwise the rate of replenishment might be greater than expected by the test, breaking the results.
        env.fg.replenish_capacity(
            <SteadyClock as TokenBucketClock>::now() + nanos(Duration::from_secs(24 * 3600)),
        );
        env
    }

    // As long as there is a request sitting in the queue, tick() will process
    // at least one request. The only situation in which tick() will return nothing
    // is if no requests were sent to the fair_queue (obviously).
    //
    // Because of this property, one useful use of tick() is to implement a drain()
    // method (see above) in which all requests currently sent to the queue are drained
    // before the queue is destroyed.
    fn tick(&mut self, n: u32) -> u32 {
        let mut processed = 0u32;
        self.fg.replenish_capacity(self.fg.replenished_ts() + nanos(Duration::from_micros(1)));
        // SAFETY: the queue is live and the callback does not touch it.
        unsafe { FairQueue::dispatch_requests(&mut self.fq, Request::submit_entry) };

        for _ in 0..n {
            let curr = std::mem::take(&mut self.inflight);

            for req in &curr {
                processed += 1;
                self.results[req.index as usize] += 1;
                self.fq.notify_request_finished(req.fqent.capacity());
            }

            self.fg.replenish_capacity(self.fg.replenished_ts() + nanos(Duration::from_micros(1)));
            // SAFETY: the queue is live and the callback does not touch it.
            unsafe { FairQueue::dispatch_requests(&mut self.fq, Request::submit_entry) };
        }
        processed
    }

    fn register_priority_class(&mut self, shares: u32) -> ClassId {
        self.results.push(0);
        self.exceptions.push(Vec::new());
        self.fq.register_priority_class(self.nr_classes, shares);
        let id = self.nr_classes;
        self.nr_classes += 1;
        id
    }

    fn do_op(&mut self, id: ClassId, weight: u32) {
        let index = id;
        let cap = self.fq.tokens_capacity(f64::from(weight) / 1_000_000.0);
        let this: *mut TestEnv = self;
        let req = Box::new(Request::new(cap, index, move |req: Request| {
            // SAFETY: the environment outlives its requests: its drop drains the queue.
            // A failed allocation aborts, so no allocation failure is recorded for
            // the class.
            unsafe { (*this).inflight.push(req) };
        }));
        let req = Box::into_raw(req);
        // SAFETY: `req` is a live leaked box, so its entry does not move; it is freed by
        // `Request::submit_entry` once the queue hands it out, and `id` is registered.
        unsafe { self.fq.queue(id, std::ptr::addr_of_mut!((*req).fqent)) };
    }

    fn update_shares(&mut self, id: ClassId, shares: u32) {
        self.fq.update_shares_for_class(id, shares);
    }

    fn reset_results(&mut self, index: ClassId) {
        self.results[index as usize] = 0;
    }

    // Verify if the ratios are what we expect. Because we can't be sure about
    // precise timing issues, we can always be off by some percentage. In simpler
    // tests we really expect it to very low, but in more complex tests, with share
    // changes, for instance, they can accumulate
    //
    // The ratios argument is the ratios towards the first class
    fn verify(&self, name: &str, ratios: &[u32], expected_error: u32) {
        assert!(ratios.len() == self.results.len());
        let mut str = format!("{}:", name);
        for (i, r) in self.results.iter().enumerate() {
            str += &format!(" r[{}] = {}", i, r);
        }
        println!("{}", str);
        for (i, &ratio) in ratios.iter().enumerate() {
            // The bounds use wrapping unsigned arithmetic.
            let min_expected =
                ratio.wrapping_mul((self.results[0] as u32).wrapping_sub(expected_error)) as i32;
            let max_expected =
                ratio.wrapping_mul((self.results[0] as u32).wrapping_add(expected_error)) as i32;
            assert!(self.results[i] >= min_expected);
            assert!(self.results[i] <= max_expected);
            assert!(self.exceptions[i].is_empty());
        }
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        self.drain();
        for id in 0..self.nr_classes {
            self.fq.unregister_priority_class(id);
        }
    }
}

// Equal ratios. Expected equal results.
engine_test_case!(test_fair_queue_equal_2classes, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut env = TestEnv::new(1);

        let a = env.register_priority_class(10);
        let b = env.register_priority_class(10);

        for _ in 0..100 {
            env.do_op(a, 1);
            env.do_op(b, 1);
        }
        yield_().get()?;
        // allow half the requests in
        env.tick(100);
        env.verify("equal_2classes", &[1, 1], 1);
        Ok(())
    })
});

// Equal results, spread among 4 classes.
engine_test_case!(test_fair_queue_equal_4classes, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut env = TestEnv::new(1);

        let a = env.register_priority_class(10);
        let b = env.register_priority_class(10);
        let c = env.register_priority_class(10);
        let d = env.register_priority_class(10);

        for _ in 0..100 {
            env.do_op(a, 1);
            env.do_op(b, 1);
            env.do_op(c, 1);
            env.do_op(d, 1);
        }
        yield_().get()?;
        // allow half the requests in
        env.tick(200);
        env.verify("equal_4classes", &[1, 1, 1, 1], 1);
        Ok(())
    })
});

// Class2 twice as powerful. Expected class2 to have 2 x more requests.
engine_test_case!(test_fair_queue_different_shares, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut env = TestEnv::new(1);

        let a = env.register_priority_class(10);
        let b = env.register_priority_class(20);

        for _ in 0..100 {
            env.do_op(a, 1);
            env.do_op(b, 1);
        }
        yield_().get()?;
        // allow half the requests in
        env.tick(100);
        env.verify("different_shares", &[1, 2], 1);
        Ok(())
    })
});

// Equal ratios, high capacity queue. Should still divide equally.
//
// Note that we sleep less because now more requests will be going through the
// queue.
engine_test_case!(test_fair_queue_equal_hi_capacity_2classes, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut env = TestEnv::new(10);

        let a = env.register_priority_class(10);
        let b = env.register_priority_class(10);

        for _ in 0..100 {
            env.do_op(a, 1);
            env.do_op(b, 1);
        }
        yield_().get()?;

        // queue has capacity 10, 10 x 10 = 100, allow half the requests in
        env.tick(10);
        env.verify("hi_capacity_2classes", &[1, 1], 1);
        Ok(())
    })
});

// Class2 twice as powerful, queue is high capacity. Still expected class2 to
// have 2 x more requests.
//
// Note that we sleep less because now more requests will be going through the
// queue.
engine_test_case!(test_fair_queue_different_shares_hi_capacity, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut env = TestEnv::new(10);

        let a = env.register_priority_class(10);
        let b = env.register_priority_class(20);

        for _ in 0..100 {
            env.do_op(a, 1);
            env.do_op(b, 1);
        }
        yield_().get()?;
        // queue has capacity 10, 10 x 10 = 100, allow half the requests in
        env.tick(10);
        env.verify("different_shares_hi_capacity", &[1, 2], 1);
        Ok(())
    })
});

// Classes equally powerful. But Class1 issues twice as expensive requests. Expected Class2 to have 2 x more requests.
engine_test_case!(test_fair_queue_different_weights, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut env = TestEnv::new(2);

        let a = env.register_priority_class(10);
        let b = env.register_priority_class(10);

        for _ in 0..100 {
            env.do_op(a, 2);
            env.do_op(b, 1);
        }
        yield_().get()?;
        // allow half the requests in
        env.tick(100);
        env.verify("different_weights", &[1, 2], 1);
        Ok(())
    })
});

// Class2 pushes many requests over. Right after, don't expect Class2 to be able to push anything else.
engine_test_case!(test_fair_queue_dominant_queue, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut env = TestEnv::new(1);

        let a = env.register_priority_class(10);
        let b = env.register_priority_class(10);

        for _ in 0..100 {
            env.do_op(b, 1);
        }
        yield_().get()?;

        // consume all requests
        env.tick(100);
        // zero statistics.
        env.reset_results(b);
        for _ in 0..20 {
            env.do_op(a, 1);
            env.do_op(b, 1);
        }
        // allow half the requests in
        env.tick(20);
        env.verify("dominant_queue", &[1, 0], 1);
        Ok(())
    })
});

// Class2 pushes many requests at first. Right after, don't expect Class1 to be able to do the same
engine_test_case!(test_fair_queue_forgiving_queue, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut env = TestEnv::new(1);

        // The fair_queue preemption logic allows one class to gain exclusive
        // queue access for at most tau duration. Test queue configures the
        // request rate to be 1/us and tau to be 50us, so after (re-)activation
        // a queue can overrun its peer by at most 50 requests.

        let a = env.register_priority_class(10);
        let b = env.register_priority_class(10);

        for _ in 0..100 {
            env.do_op(a, 1);
        }
        yield_().get()?;

        // consume all requests
        env.tick(100);
        env.reset_results(a);

        for _ in 0..100 {
            env.do_op(a, 1);
            env.do_op(b, 1);
        }
        yield_().get()?;

        // allow half the requests in
        env.tick(100);

        // 50 requests should be passed from b, other 100 should be shared 1:1
        env.verify("forgiving_queue", &[1, 3], 2);
        Ok(())
    })
});

// Classes push requests and then update swap their shares. In the end, should have executed
// the same number of requests.
engine_test_case!(test_fair_queue_update_shares, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut env = TestEnv::new(1);

        let a = env.register_priority_class(20);
        let b = env.register_priority_class(10);

        for _ in 0..500 {
            env.do_op(a, 1);
            env.do_op(b, 1);
        }

        yield_().get()?;
        // allow 25% of the requests in
        env.tick(250);
        env.update_shares(a, 10);
        env.update_shares(b, 20);

        yield_().get()?;
        // allow 25% of the requests in
        env.tick(250);
        env.verify("update_shares", &[1, 1], 2);
        Ok(())
    })
});

// Classes run for a longer period of time. Balance must be kept over many timer
// periods.
engine_test_case!(test_fair_queue_longer_run, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut env = TestEnv::new(1);

        let a = env.register_priority_class(10);
        let b = env.register_priority_class(10);

        for _ in 0..20000 {
            env.do_op(a, 1);
            env.do_op(b, 1);
        }
        // In total allow half the requests in, but do it over a
        // long period of time, ticking slowly
        for _ in 0..1000 {
            sleep(Duration::from_millis(1)).get()?;
            env.tick(2);
        }
        env.verify("longer_run", &[1, 1], 2);
        Ok(())
    })
});

// Classes run for a longer period of time. Proportional balance must be kept over many timer
// periods, despite unequal shares..
engine_test_case!(test_fair_queue_longer_run_different_shares, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut env = TestEnv::new(1);

        let a = env.register_priority_class(10);
        let b = env.register_priority_class(20);

        for _ in 0..20000 {
            env.do_op(a, 1);
            env.do_op(b, 1);
        }

        // In total allow half the requests in, but do it over a
        // long period of time, ticking slowly
        for _ in 0..1000 {
            sleep(Duration::from_millis(1)).get()?;
            env.tick(3);
        }
        env.verify("longer_run_different_shares", &[1, 2], 2);
        Ok(())
    })
});

// Classes run for a random period of time. Equal operations expected.
engine_test_case!(test_fair_queue_random_run, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut env = TestEnv::new(1);

        let a = env.register_priority_class(1);
        let b = env.register_priority_class(1);

        // The run lengths are drawn from the local random engine, in
        // multiples of 100usec - which is the approximate length of the request. We will
        // put a minimum of 10. Below that, it is hard to guarantee anything. The maximum is
        // about 50 seconds.
        let distribution = UniformIntDistribution::new(10, 500 * 1000);
        let reqs = distribution.sample_fn(&mut || u64::from(local_random())) as u32;

        // Enough requests for the maximum run (half per queue, + leeway)
        for _ in 0..reqs {
            env.do_op(a, 1);
            env.do_op(b, 1);
        }

        yield_().get()?;
        // In total allow half the requests in
        env.tick(reqs);

        // Accept 5 % error.
        let expected_error = std::cmp::max(1, (f64::from(reqs) * 0.05).round() as i32);
        env.verify(&format!("random_run ({} requests)", reqs), &[1, 1], expected_error as u32);
        Ok(())
    })
});
