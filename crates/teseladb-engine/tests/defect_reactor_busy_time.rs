#![cfg(target_os = "linux")]
//! The reactor's busy time is exported as a counter, so it never decreases, even when it is
//! read from code that runs while the reactor is idle or right after it wakes up (timer
//! callbacks, tasks resumed by a sleep). Work done by a task woken from sleep counts as busy
//! even when the reactor finds nothing else to do before sleeping again, and the sleeps do
//! not.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::pin::pin;
use std::rc::Rc;
use std::time::{Duration, Instant};

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::lowres_clock::LowresClock;
use teseladb::engine::core::reactor::{SchedDuration, engine};
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::timer::{SteadyClockType, Timer};
use teseladb::engine_test_case;

fn busy_time() -> SchedDuration {
    // SAFETY: called on a reactor thread, with no other reference to the reactor live.
    unsafe { (*engine()).total_busy_time() }
}

struct BusyTimeSampler {
    last: SchedDuration,
    samples: u32,
    decreases: u32,
    max_decrease: SchedDuration,
}

impl BusyTimeSampler {
    fn new() -> Self {
        Self { last: busy_time(), samples: 0, decreases: 0, max_decrease: SchedDuration::default() }
    }

    fn sample(&mut self) {
        let busy = busy_time();
        self.samples += 1;
        if busy < self.last {
            self.decreases += 1;
            self.max_decrease = self.max_decrease.max(self.last - busy);
        }
        self.last = busy;
    }
}

fn spin_for(d: Duration) {
    let start = Instant::now();
    while start.elapsed() < d {}
}

// A small xorshift generator, so the sleeps vary from run to run.
struct Rng(u64);

impl Rng {
    fn next_below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

// Samples the busy time from timer callbacks and from a task resumed by sleeps of varying
// length, so the idle periods preceding each sample differ.
engine_test_case!(test_busy_time_never_decreases, {
    thread_async(move || -> Result<(), ExceptionPtr> {
        let sampler = Rc::new(RefCell::new(BusyTimeSampler::new()));
        let mut t = pin!(Timer::<SteadyClockType>::new());
        let s = sampler.clone();
        t.as_mut().set_callback(move || s.borrow_mut().sample());
        t.as_mut().arm_periodic(Duration::from_millis(3));
        let mut lowres_t = pin!(Timer::<LowresClock>::new());
        let s = sampler.clone();
        lowres_t.as_mut().set_callback(move || s.borrow_mut().sample());
        lowres_t.as_mut().arm_periodic(Duration::from_millis(20));

        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.as_nanos() as u64 | 1);
        eprintln!("seed {seed}");
        let mut rng = Rng(seed);
        let deadline = Instant::now() + Duration::from_millis(2500);
        while Instant::now() < deadline {
            sleep(Duration::from_micros(rng.next_below(5001))).get()?;
            sampler.borrow_mut().sample();
        }
        t.as_mut().cancel();
        lowres_t.as_mut().cancel();

        let s = sampler.borrow();
        eprintln!(
            "{} samples, {} decreases, max decrease {}us",
            s.samples,
            s.decreases,
            s.max_decrease.to_std().as_micros()
        );
        assert_eq!(s.decreases, 0);
        Ok(())
    })
});

// Work done by a task woken up from sleep is busy time, even when the reactor finds nothing
// else to do before sleeping again, and the sleeps are not.
engine_test_case!(test_busy_time_counts_work_after_wakeup, {
    thread_async(move || -> Result<(), ExceptionPtr> {
        // Take the readings after a cross-shard round trip, which ends any idle period, so
        // they do not depend on how busy time reads while idle.
        let settle = || Smp::submit_to((this_shard_id() + 1) % Smp::count(), || {});
        settle().get()?;
        let busy_before = busy_time();
        let mut spun = Duration::ZERO;
        let mut slept = Duration::ZERO;
        for _ in 0..100 {
            let sleep_start = Instant::now();
            sleep(Duration::from_millis(2)).get()?;
            slept += sleep_start.elapsed();
            let start = Instant::now();
            spin_for(Duration::from_micros(500));
            spun += start.elapsed();
        }
        settle().get()?;
        let busy = (busy_time() - busy_before).to_std();

        eprintln!(
            "spun {}us, slept {}us, busy {}us",
            spun.as_micros(),
            slept.as_micros(),
            busy.as_micros()
        );
        assert!(busy >= spun);
        assert!(busy < spun + slept / 2);
        Ok(())
    })
});
