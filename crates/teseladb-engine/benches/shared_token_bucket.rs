//! The test allows measuring if the shared token bucket allows the tokens
//! consumers to get tokens at the rate the bucket is configured with.
//!
//! Report example:
//!
//!     effective rate is 1016575.0t/s, [239668 ... 263522]
//!
//! The last line "effective rate" is the tokens-per-second rate all workers were
//! able to get. It should be equal to the configured rate (context::rate below)
//!
//! In braces there are minimal and maximum rate of individual shards. These numbers
//! should not differ to much from each other, if they do it means that the t.b.
//! is not fair

// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/perf_tests.rs"]
mod perf_tests;

#[path = "../tests/common/minstd.rs"]
mod minstd;

use std::collections::VecDeque;
use std::pin::Pin;
use std::time::{Duration, Instant};

use minstd::UniformIntDistribution;
use teseladb::engine::core::future::{Future, make_ready_future};
use teseladb::engine::core::loop_::do_until;
use teseladb::engine::core::reactor::yield_;
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::sharded::{Sharded, ShardedService};
use teseladb::engine::core::sleep::sleep;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::timer::{SteadyClockType, Timer};
use teseladb::engine::testing::random::testing::local_random;
use teseladb::engine::util::shared_token_bucket::internal::{
    CappedRelease, RateResolution, Ratio1, Rovers, RoversCapped, RoversUncapped, SharedTokenBucket,
    SteadyClock, TokenBucketClock,
};

type ClockType = Instant;

// The test uses uint64_t tokens with per-second time-measurement and can use
// capped and non-capped token buckets.
// Unused (the contexts name the rovers instead); kept for symmetry with the uncapped alias.
#[allow(dead_code)]
type CappedTokenBucketT = SharedTokenBucket<Ratio1, RoversCapped>;
#[allow(dead_code)]
type PureTokenBucketT = SharedTokenBucket<Ratio1, RoversUncapped>;

/// The next value of this thread's minimal-standard generator.
fn local_engine() -> u64 {
    u64::from(local_random())
}

/// An exponentially distributed value with rate `lambda`, from this thread's generator.
fn exponential(lambda: f64) -> f64 {
    let r = (local_engine() - 1) as f64 / 2_147_483_646.0;
    let r = if r >= 1.0 { f64::from_bits(1.0f64.to_bits() - 1) } else { r };
    -(1.0 - r).ln() / lambda
}

/// Converts a duration in seconds to whole microseconds, truncating toward zero.
fn duration_cast_micros(d: RateResolution<Ratio1>) -> Duration {
    Duration::from_micros((d.count() * 1e6) as u64)
}

// The test entry point calls map_reduce on the sharded<> workers set and this
// is what each worker produces, so that the entry point could accumulate the
// final result from
#[derive(Clone, Copy)]
struct WorkResult {
    tokens: u64,
    // Never read: accumulate() sums the tokens only.
    #[allow(dead_code)]
    released: u64,
}

#[derive(Clone, Copy)]
struct Statistics {
    total: u64,
    min: u64,
    max: u64,
}

impl Default for Statistics {
    fn default() -> Self {
        Statistics { total: 0, min: u64::MAX, max: u64::MIN }
    }
}

fn accumulate(acc: Statistics, val: WorkResult) -> Statistics {
    Statistics {
        total: acc.total + val.tokens,
        min: acc.min.min(val.tokens),
        max: acc.max.max(val.tokens),
    }
}

// Per-tick statistics. Collected, but not reported by default
// (read by print_and_clear_ticks() only, which nothing calls).
#[allow(dead_code)]
struct TickData {
    delay: Duration,
    sleep: Duration,
    defic: u64,
    tokens: u64,
    total: u64,
}

// Tokens are released in a timer. This mimics default reactor workflow
// when requests are reaped from the kernel each task-quota -- 0.5ms
const RELEASE_PERIOD: Duration = Duration::from_micros(500);

// The worker itself. Has a reference on the shared token bucket and tries to
// consume as many tokens as it can without artificial delays. Reports the number
// of tokens grabbed from bucket while running
struct Worker<R: Rovers + 'static> {
    container: *mut Sharded<Box<Worker<R>>>,
    tb: *const SharedTokenBucket<Ratio1, R>,

    // Capped bucket requires that t.b. user releases the tokens so that
    // they could be replenished. Respectively, the worker keeps track of
    // total number of grabbed tokens as well as the number of not-yet-released
    // tokens and the total number of released tokens. The "available" number
    // of tokens is used to decide if the worker can release them or not
    tokens: u64,
    available: u64,
    released: u64,

    release_per_tick: u64,
    last_release: ClockType,
    release_tokens: Timer,
    head: Option<(i32, u64)>,

    // The IO-scheduler doesn't get more than this number of tokens per tick.
    // The test tries to mimic this behavior
    threshold: u64,

    // The number of tokens to grab at a time. It's a distribution to resemble
    // IO queue that tries to grab different amount of tokens for requests of
    // different sizes and direction
    size: UniformIntDistribution,

    ticks: VecDeque<TickData>,
}

impl<R: Rovers + 'static> Worker<R> {
    fn tb(&self) -> &SharedTokenBucket<Ratio1, R> {
        // SAFETY: the context owns the bucket and outlives its workers.
        unsafe { &*self.tb }
    }

    /// Boxed: the release timer refers to the worker by address.
    fn new(tb_: *const SharedTokenBucket<Ratio1, R>) -> Box<Self> {
        // SAFETY: as in tb().
        let tb = unsafe { &*tb_ };
        let release_per_tick =
            (tb.rate() as f64 / f64::from(Smp::count()) * RELEASE_PERIOD.as_secs_f64()) as u64;
        let threshold = tb.limit() / u64::from(Smp::count());
        let mut w = Box::new(Worker {
            container: std::ptr::null_mut(),
            tb: tb_,
            tokens: 0,
            available: 0,
            released: 0,
            release_per_tick,
            last_release: Instant::now(),
            release_tokens: Timer::new(),
            head: None,
            threshold,
            size: UniformIntDistribution::new(1, (threshold as i64).min(128) as u64),
            ticks: VecDeque::new(),
        });
        let wp: *mut Self = &mut *w;
        // SAFETY: the worker is boxed, so its timer never moves.
        let mut tmr = unsafe { Pin::new_unchecked(&mut w.release_tokens) };
        // SAFETY: the timer is a field of the worker, so it never fires after the worker is gone.
        tmr.as_mut().set_callback(move || unsafe { (*wp).do_release() });
        tmr.arm_periodic(RELEASE_PERIOD);
        println!(
            "{} worker, threshold {}, release-per-tick {}",
            this_shard_id(),
            w.threshold,
            w.release_per_tick
        );
        w
    }

    fn do_release_n(&mut self, tokens: u64) {
        self.available -= tokens;
        self.released += tokens;
        if R::CAPPED == CappedRelease::Yes {
            self.tb().release(tokens);
        }
    }

    fn do_release(&mut self) {
        // Timer can fire later than programmed because of hogs not yielding in a timely
        // manner. If that was an IO queue more requests would have been reaped from the
        // kernel, so do the same here -- scale the number of releasable tokens proportionally
        let now = Instant::now();
        let real_delay = (now - self.last_release).as_secs_f64();
        self.last_release = now;
        let to_release =
            (real_delay * self.release_per_tick as f64 / RELEASE_PERIOD.as_secs_f64()) as u64;
        self.do_release_n(to_release.min(self.available));
    }

    fn work(&mut self, do_sleep: fn(RateResolution<Ratio1>) -> Future<()>) -> Future<WorkResult> {
        assert!(self.tokens == 0);
        let start = Instant::now();
        let end = start + Duration::from_secs(1);
        let this: *mut Self = self;
        // Run for 1 second. The perf suite would restart this method several times
        do_until(
            move || Instant::now() >= end,
            move || {
                // SAFETY: the worker outlives the loop, which map_reduce0 waits for.
                let me = unsafe { &mut *this };
                let mut d: u64 = 0;
                let mut l_tokens: u64 = 0;
                let mut sz: i32;

                while l_tokens < me.threshold {
                    if let Some((hsz, hh)) = me.head {
                        me.tb().replenish(<SteadyClock as TokenBucketClock>::now());
                        d = me.tb().deficiency(hh);
                        if d > 0 {
                            break;
                        }
                        sz = hsz;
                        me.head = None;
                    } else {
                        sz = me.size.sample_fn(&mut local_engine) as i32;
                        let h = me.tb().grab(sz as u64);
                        d = me.tb().deficiency(h);
                        if d > 0 {
                            me.head = Some((sz, h));
                            break;
                        }
                    }
                    me.tokens += sz as u64;
                    l_tokens += sz as u64;
                    me.available += sz as u64;
                }

                let p = me.tb().duration_for(d);
                me.ticks.push_back(TickData { delay: Instant::now() - start, sleep: duration_cast_micros(p), defic: d, tokens: l_tokens, total: me.tokens });
                if me.ticks.len() > 2048 {
                    me.ticks.pop_front();
                }

                do_sleep(p)
            },
        )
        .then(move |()| {
            // SAFETY: as above.
            let me = unsafe { &mut *this };
            // Reports:
            //  - shard-id
            //  - total number of tokens and total time taken
            //  - effective speed
            //  - expected speed (token-bucket.rate() / smp::count)
            //  - ticks -- the number of times the worker had change to grab tokens
            //  - the info about tokens releasing
            let delay = start.elapsed().as_secs_f64();
            println!(
                "{} {}t/{:.3}s, speed is {:.1}t/s goal {:.1}t/s, {} ticks, released {} (accumulated {})",
                this_shard_id(),
                me.tokens,
                delay,
                me.tokens as f64 / delay,
                me.tb().rate() as f64 / f64::from(Smp::count()),
                me.ticks.len(),
                me.released,
                me.available
            );
            let a = me.available;
            me.do_release_n(a);
            let r = WorkResult { tokens: std::mem::take(&mut me.tokens), released: std::mem::take(&mut me.released) };
            make_ready_future(r)
        })
    }

    // The below two are how worker waits for the token-bucket deficiency
    // to disappear (i.e. -- when the requested number of tokens are replenished
    //
    // Two options -- poll infinitely or sleep for the estimated (by the
    // bucket method) duration
    fn work_sleeping(&mut self) -> Future<WorkResult> {
        self.work(|d| sleep::<SteadyClockType>(duration_cast_micros(d)))
    }

    fn work_yielding(&mut self) -> Future<WorkResult> {
        self.work(|_| yield_())
    }

    // Not called by default: the ticks are collected but not reported.
    #[allow(dead_code)]
    fn print_and_clear_ticks(&mut self) -> Future<()> {
        println!("{} {} ticks", this_shard_id(), self.ticks.len());
        let mut p = Duration::ZERO;
        for td in &self.ticks {
            println!(
                "  {:8} +{:5} us {:5}/{:5} def {:3} sleep {:5} us",
                td.delay.as_micros(),
                (td.delay - p).as_micros(),
                td.tokens,
                td.total,
                td.defic,
                td.sleep.as_micros()
            );
            p = td.delay;
        }
        self.ticks.clear();
        if this_shard_id() == Smp::count() - 1 {
            return make_ready_future(());
        }
        // SAFETY: the container (peering_sharded_service) outlives its services.
        unsafe { Pin::new_unchecked(&mut *self.container) }
            .invoke_on(this_shard_id() + 1, |w: &mut Box<Worker<R>>| w.print_and_clear_ticks())
    }
}

/// `peering_sharded_service<worker<TokenBucket>>`: the service keeps its container.
impl<R: Rovers + 'static> ShardedService for Box<Worker<R>> {
    fn set_container(&mut self, container: *mut Sharded<Self>) {
        self.container = container;
    }
}

// CPU hog that occupies CPU for "busy" duration, then sleeps for "rest" duration
// The actual periods are randomized to be thus "on average"
struct Hog {
    busy: f64,
    rest: f64,
    stopped: Option<Future<()>>,
    keep_going: bool,
    iterations: u64,
}

impl Hog {
    fn new(b: Duration, r: Duration) -> Box<Self> {
        Box::new(Hog {
            busy: 1.0 / b.as_secs_f64(),
            rest: 1.0 / r.as_secs_f64(),
            stopped: None,
            keep_going: false,
            iterations: 0,
        })
    }

    fn work(&mut self) {
        assert!(self.stopped.is_none());
        self.keep_going = true;
        let this: *mut Self = self;
        self.stopped = Some(do_until(
            // SAFETY: the hog outlives its loop, which terminate() waits for.
            move || unsafe { !(*this).keep_going },
            move || {
                // SAFETY: `this` is the hog, which outlives its loop (terminate() waits for it); the loop condition and body run one after the other on this shard.
                let me = unsafe { &mut *this };
                let p = exponential(me.rest);
                sleep::<SteadyClockType>(Duration::from_micros((p * 1e6) as u64)).then(move |()| {
                    // SAFETY: as above.
                    let me = unsafe { &mut *this };
                    me.iterations += 1;
                    let until = Instant::now() + Duration::from_secs_f64(exponential(me.busy));
                    loop {
                        if !(Instant::now() < until && me.keep_going) {
                            break;
                        }
                    }
                })
            },
        ));
    }

    fn terminate(&mut self) -> Future<()> {
        assert!(self.stopped.is_some());
        self.keep_going = false;
        self.stopped.take().unwrap_or_else(|| make_ready_future(()))
    }
}

impl ShardedService for Box<Hog> {}

const RATE: u64 = 1_000_000;
const LIMIT: u64 = RATE / 2000;
const THRESHOLD: u64 = 1;

struct Context<R: Rovers + 'static> {
    // Owns the bucket every worker points to: kept alive, never read.
    #[allow(dead_code)]
    tb: Box<SharedTokenBucket<Ratio1, R>>,
    w: Pin<Box<Sharded<Box<Worker<R>>>>>,
    h: Pin<Box<Sharded<Box<Hog>>>>,
}

// SAFETY: a bench fixture lives and runs on one shard.
unsafe impl<R: Rovers + 'static> Send for Context<R> {}

impl<R: Rovers + 'static> Default for Context<R> {
    fn default() -> Self {
        let tb = Box::new(SharedTokenBucket::<Ratio1, R>::new(RATE, LIMIT, THRESHOLD, true));
        let tbp = &*tb as *const SharedTokenBucket<Ratio1, R> as usize;
        let mut w = Box::pin(Sharded::<Box<Worker<R>>>::new());
        let mut h = Box::pin(Sharded::<Box<Hog>>::new());
        w.as_mut()
            .start(move || Worker::new(tbp as *const SharedTokenBucket<Ratio1, R>))
            .get()
            .expect("start workers");
        h.as_mut()
            .start(|| Hog::new(Duration::from_micros(300), Duration::from_micros(100)))
            .get()
            .expect("start hogs");
        println!("Created tb {}t/s (limit {} threshold {})", tb.rate(), tb.limit(), tb.threshold());
        Context { tb, w, h }
    }
}

impl<R: Rovers + 'static> Drop for Context<R> {
    fn drop(&mut self) {
        let _ = self.h.as_mut().stop().get();
        let _ = self.w.as_mut().stop().get();
    }
}

impl<R: Rovers + 'static> Context<R> {
    fn run_workers(&mut self, fn_: fn(&mut Worker<R>) -> Future<WorkResult>) -> Future<()> {
        let start = Instant::now();
        self.w
            .as_mut()
            .map_reduce0(move |w: &mut Box<Worker<R>>| fn_(w), Statistics::default(), accumulate)
            .then(move |st: Statistics| {
                let delay = start.elapsed().as_secs_f64();
                println!(
                    "effective rate is {:.1}t/s, [{} ... {}]",
                    st.total as f64 / delay,
                    st.min,
                    st.max
                );
            })
    }

    fn test_sleeping(&mut self) -> Future<()> {
        println!("---8<---");
        self.run_workers(Worker::work_sleeping)
    }

    fn test_yielding(&mut self) -> Future<()> {
        println!("---8<---");
        self.run_workers(Worker::work_yielding)
    }

    fn test_sleeping_with_hog(&mut self) -> Future<()> {
        println!("---8<---");
        let this: *mut Self = self;
        self.h.as_mut().invoke_on_all(|h: &mut Box<Hog>| h.work()).then(move |()| {
            // SAFETY: the fixture outlives the test run.
            let me = unsafe { &mut *this };
            // SAFETY: `this` is the fixture, which outlives the test run; the continuation runs after `me` above is no longer used.
            me.run_workers(Worker::work_sleeping).then(move |()| unsafe {
                (*this).h.as_mut().invoke_on_all(|h: &mut Box<Hog>| h.terminate())
            })
        })
    }
}

// There are 3 tests run over 2 types of buckets:
//
// - poll token bucket for tokens in case of deficiency
// - sleep in case token bucket reports deficiency
// - sleep on deficiency, but run CPU hogs in the background
//
// All tests are run with capped and non-capped (called pure) token buckets

macro_rules! context_tests {
    ($group:ident) => {
        teseladb::perf_test_f!($group, yielding_throughput, |s| -> Future<()> {
            s.test_yielding()
        });
        teseladb::perf_test_f!($group, sleeping_throughput, |s| -> Future<()> {
            s.test_sleeping()
        });
        teseladb::perf_test_f!($group, sleeping_throughput_with_hog, |s| -> Future<()> {
            s.test_sleeping_with_hog()
        });

        pub fn register_all() {
            yielding_throughput::register();
            sleeping_throughput::register();
            sleeping_throughput_with_hog::register();
        }
    };
}

/// `struct perf_capped_context : public context<capped_token_bucket_t> {}`
#[allow(non_camel_case_types)]
mod group_capped {
    use super::*;
    pub type perf_capped_context = Context<RoversCapped>;
    context_tests!(perf_capped_context);
}

/// `struct perf_pure_context : public context<pure_token_bucket_t> {}`
#[allow(non_camel_case_types)]
mod group_pure {
    use super::*;
    pub type perf_pure_context = Context<RoversUncapped>;
    context_tests!(perf_pure_context);
}

fn _bucket_types(_: Option<&CappedTokenBucketT>, _: Option<&PureTokenBucketT>) {}

pub(crate) fn main() {
    group_capped::register_all();
    group_pure::register_all();
    perf_tests::main();
}
