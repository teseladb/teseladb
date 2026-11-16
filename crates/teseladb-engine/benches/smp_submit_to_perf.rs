//! Cross-shard `submit_to` round trips under load.
//!
//! Shards submit requests to each other while optional "thinker" tasks keep each shard
//! busy with CPU work (busy loops with Poisson-distributed lengths); the measured
//! statistics are printed at the end.

// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use teseladb::engine::core::app_template::AppTemplate;
use teseladb::engine::core::future::{ExceptionPtr, Future, RuntimeError, make_ready_future};
use teseladb::engine::core::loop_::{do_until, parallel_for_each};
use teseladb::engine::core::lowres_clock::LowresClock;
use teseladb::engine::core::program_options::bpo;
use teseladb::engine::core::reactor::{check_for_io_immediately, yield_};
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::sharded::{Sharded, ShardedService};
use teseladb::engine::core::sleep::sleep;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::thread::async_;
use teseladb::engine::core::timer::SteadyClockType;

/// Exponentially distributed pauses (a Poisson process), from a 32-bit Mersenne Twister
/// (MT19937).
struct PoissonProcess {
    mt: [u32; 624],
    idx: usize,
    lambda: f64,
}

impl PoissonProcess {
    fn new(period: Duration) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(5489);
        let mut mt = [0u32; 624];
        mt[0] = seed;
        for i in 1..624 {
            mt[i] =
                1812433253u32.wrapping_mul(mt[i - 1] ^ (mt[i - 1] >> 30)).wrapping_add(i as u32);
        }
        PoissonProcess { mt, idx: 624, lambda: 1.0 / period.as_secs_f64() }
    }

    fn next(&mut self) -> u32 {
        if self.idx >= 624 {
            for i in 0..624 {
                let y = (self.mt[i] & 0x8000_0000) | (self.mt[(i + 1) % 624] & 0x7FFF_FFFF);
                let mut v = self.mt[(i + 397) % 624] ^ (y >> 1);
                if y & 1 != 0 {
                    v ^= 0x9908_B0DF;
                }
                self.mt[i] = v;
            }
            self.idx = 0;
        }
        let mut y = self.mt[self.idx];
        self.idx += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9D2C_5680;
        y ^= (y << 15) & 0xEFC6_0000;
        y ^= y >> 18;
        y
    }

    /// `generate_canonical<double, 53>` over two 32-bit draws.
    fn canonical(&mut self) -> f64 {
        let lo = f64::from(self.next());
        let hi = f64::from(self.next());
        let r = (lo + hi * 4294967296.0) / 18446744073709551616.0;
        if r >= 1.0 { f64::from_bits(1.0f64.to_bits() - 1) } else { r }
    }

    fn get(&mut self) -> Duration {
        Duration::from_secs_f64(-(1.0 - self.canonical()).ln() / self.lambda)
    }
}

// The pause between submissions lives in the thinkers' closures, which share it.
struct Thinker {
    stop: Rc<std::cell::Cell<bool>>,
    done: Option<Future<()>>,
}

impl Thinker {
    fn start_thinking(
        concurrency: u32,
        pause: Rc<RefCell<PoissonProcess>>,
        stop: Rc<std::cell::Cell<bool>>,
    ) -> Future<()> {
        parallel_for_each(0..concurrency, move |_f: u32| {
            let pause = pause.clone();
            let stop = stop.clone();
            do_until(
                move || stop.get(),
                move || {
                    let until = Instant::now() + pause.borrow_mut().get();
                    while Instant::now() < until {
                        // do nothing
                    }
                    make_ready_future(())
                },
            )
        })
    }

    fn new(concurrency: u32, think: Duration) -> Self {
        let pause = Rc::new(RefCell::new(PoissonProcess::new(think)));
        let stop = Rc::new(std::cell::Cell::new(false));
        let done = Some(Self::start_thinking(concurrency, pause, stop.clone()));
        println!(
            "shard {} starts {}x{}us thinkers",
            this_shard_id(),
            concurrency,
            think.as_micros()
        );
        Thinker { stop, done }
    }

    fn stop(&mut self) -> Future<()> {
        self.stop.set(true);
        self.done.take().unwrap_or_else(|| make_ready_future(()))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RespondType {
    Ready,
    Yield,
    Io,
    Timer,
}

fn parse_respond_type(s: &str) -> Result<RespondType, ExceptionPtr> {
    match s {
        "ready" => Ok(RespondType::Ready),
        "yield" => Ok(RespondType::Yield),
        "io" => Ok(RespondType::Io),
        "timer" => Ok(RespondType::Timer),
        _ => Err(ExceptionPtr::new(RuntimeError("unknown respond type".to_string()))),
    }
}

#[derive(Clone, Copy)]
struct WorkerConfig {
    targets: u32,
    thinkers: u32,
    think: Duration,
    respond: RespondType,
    respond_tmo: Duration,
    concurrency: u32,
}

struct Worker {
    to: u32,
    think: Option<Thinker>,
    total: Rc<std::cell::Cell<u64>>,
    stop: Rc<std::cell::Cell<bool>>,
    done: Option<Future<()>>,
}

impl Worker {
    fn my_target(targets: u32) -> u32 {
        let group_size = Smp::count().div_ceil(targets);
        let group_no = this_shard_id() / group_size;
        group_size * group_no
    }

    fn start_working(
        to: u32,
        concurrency: u32,
        resp: RespondType,
        tmo: Duration,
        total: Rc<std::cell::Cell<u64>>,
        stop: Rc<std::cell::Cell<bool>>,
    ) -> Future<()> {
        parallel_for_each(0..concurrency, move |_f: u32| {
            let total = total.clone();
            let stop = stop.clone();
            do_until(
                move || stop.get(),
                move || {
                    let total = total.clone();
                    Smp::submit_to(to, move || -> Future<()> {
                        match resp {
                            RespondType::Ready => make_ready_future(()),
                            RespondType::Yield => yield_(),
                            RespondType::Io => check_for_io_immediately(),
                            RespondType::Timer => sleep::<LowresClock>(tmo),
                        }
                    })
                    .then(move |()| {
                        total.set(total.get() + 1);
                        make_ready_future(())
                    })
                },
            )
        })
    }

    fn new(cfg: WorkerConfig) -> Self {
        let to = Self::my_target(cfg.targets);
        let is_target = to == this_shard_id();
        let think = if is_target && cfg.thinkers > 0 {
            Some(Thinker::new(cfg.thinkers, cfg.think))
        } else {
            None
        };
        let total = Rc::new(std::cell::Cell::new(0u64));
        let stop = Rc::new(std::cell::Cell::new(false));
        let done = Some(Self::start_working(
            to,
            cfg.concurrency,
            cfg.respond,
            cfg.respond_tmo,
            total.clone(),
            stop.clone(),
        ));
        Worker { to, think, total, stop, done }
    }

    fn stop_(&mut self) -> Future<()> {
        if self.stop.get() {
            return make_ready_future(());
        }
        self.stop.set(true);
        let this: *mut Self = self;
        self.done.take().unwrap_or_else(|| make_ready_future(())).then(move |()| {
            // SAFETY: the worker outlives its stop() (the sharded container stops it later).
            match unsafe { &mut (*this).think } {
                Some(t) => t.stop(),
                None => make_ready_future(()),
            }
        })
    }

    fn is_target(&self) -> bool {
        self.to == this_shard_id()
    }

    fn total(&self) -> u64 {
        self.total.get()
    }
}

impl ShardedService for Worker {
    fn stop(&mut self) -> Future<()> {
        self.stop_()
    }
}

/// The two stats the remote shards append to, passed by pointer to their closures.
struct StatsRefs(*mut Stats, *mut Stats);

// SAFETY: each remote call dereferences the pointers while the owner thread is blocked in
// get() waiting for it, so there is never a concurrent access.
unsafe impl Send for StatsRefs {}

struct Stats {
    min: u64,
    max: u64,
    sum: u64,
    nr: u32,
    norm: u32,
}

impl Stats {
    fn new(norm: u32) -> Self {
        Stats { min: u64::MAX, max: u64::MIN, sum: 0, nr: 0, norm }
    }

    fn append(&mut self, val: u64) {
        self.min = self.min.min(val);
        self.max = self.max.max(val);
        self.sum += val;
        self.nr += 1;
    }

    fn min(&self) -> f64 {
        self.min as f64 / f64::from(self.norm)
    }
    fn max(&self) -> f64 {
        self.max as f64 / f64::from(self.norm)
    }
    fn avg(&self) -> f64 {
        if self.nr > 0 { self.sum as f64 / f64::from(self.nr) / f64::from(self.norm) } else { 0.0 }
    }
    fn nr(&self) -> u32 {
        self.nr
    }
}

pub(crate) fn main() {
    let mut at = AppTemplate::new(Default::default());
    at.add_options()
        .opt("duration", bpo::value::<u32>().default_value(32), "time to run the test (seconds)")
        .opt("targets", bpo::value::<u32>().default_value(1), "number of responder shards")
        .opt(
            "thinkers",
            bpo::value::<u32>().default_value(0),
            "thinker fibers to run in parallel on targets",
        )
        .opt("think", bpo::value::<u32>().default_value(100), "time (us) thinkers busyloop for")
        .opt(
            "respond",
            bpo::value::<String>().default_value("ready".to_string()),
            "how to respond on target (ready, yield, io, timer)",
        )
        .opt(
            "respond-timeout",
            bpo::value::<u32>().default_value(1),
            "the 'timer' respond timeout (us)",
        )
        .opt(
            "concurrency",
            bpo::value::<u32>().default_value(1),
            "smp::submit_to operations to issue in parallel",
        );
    let atp: *mut AppTemplate = &mut *at;
    let args: Vec<String> = std::env::args().collect();
    let status = at.run(args, move || {
        // SAFETY: the app outlives its run().
        let c = unsafe { (*atp).configuration() };
        let duration = Duration::from_secs(u64::from(*c.get::<u32>("duration").expect("default")));
        let respond = parse_respond_type(c.get::<String>("respond").expect("default"));
        let cfg = respond.map(|respond| WorkerConfig {
            targets: *c.get::<u32>("targets").expect("default"),
            thinkers: *c.get::<u32>("thinkers").expect("default"),
            think: Duration::from_micros(u64::from(*c.get::<u32>("think").expect("default"))),
            respond,
            respond_tmo: Duration::from_micros(u64::from(
                *c.get::<u32>("respond-timeout").expect("default"),
            )),
            concurrency: *c.get::<u32>("concurrency").expect("default"),
        });
        async_(move || -> Result<(), ExceptionPtr> {
            let cfg = cfg?;
            let mut workers = Box::pin(Sharded::<Worker>::new());
            let start = Instant::now();
            workers.as_mut().start(move || Worker::new(cfg)).get()?;
            sleep::<SteadyClockType>(duration).get()?;
            workers.as_mut().invoke_on_all(|w: &mut Worker| w.stop_()).get()?;
            let real_duration = start.elapsed().as_secs() as u32;
            println!("took {}s (expected {}s)", real_duration, duration.as_secs());

            let mut st = Stats::new(real_duration);
            let mut st_targets = Stats::new(real_duration);
            for i in 0..Smp::count() {
                // `[&st, &st_targets]`: the remote shard appends to this shard's stats while
                // this thread waits in get(), one shard at a time.
                let stats = StatsRefs(&mut st, &mut st_targets);
                workers
                    .as_mut()
                    .invoke_on(i, move |w: &mut Worker| {
                        let stats = stats;
                        // SAFETY: both stats outlive the call, which get() waits for, and
                        // nothing else touches them meanwhile.
                        unsafe {
                            if w.is_target() {
                                (*stats.1).append(w.total());
                            } else {
                                (*stats.0).append(w.total());
                            }
                        }
                    })
                    .get()?;
            }

            println!(
                "workers({:2}): min {:.1} avg {:.1} max {:.1} op/s",
                st.nr(),
                st.min(),
                st.avg(),
                st.max()
            );
            println!(
                "targets({:2}): min {:.1} avg {:.1} max {:.1} op/s",
                st_targets.nr(),
                st_targets.min(),
                st_targets.avg(),
                st_targets.max()
            );

            workers.as_mut().stop().get()?;
            Ok(())
        })
    });
    std::process::exit(status);
}
