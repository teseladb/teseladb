//! Tests of timers on the steady and low-resolution clocks: arming, rearming, periodic
//! timers, cancellation and expiry order. A program with its own `main` (`harness = false`).

// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::time::Duration;

use teseladb::engine::core::app_template::AppTemplate;
use teseladb::engine::core::future::{ExceptionPtr, Future, Promise, RuntimeError};
use teseladb::engine::core::lowres_clock::LowresClock;
use teseladb::engine::core::reactor::{
    Reactor, create_scheduling_group_with_shares, destroy_scheduling_group,
};
use teseladb::engine::core::scheduling::{SchedulingGroup, current_scheduling_group};
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::thread::{ThreadAttributes, async_, async_with_attributes};
use teseladb::engine::core::timer::{SteadyClockType, Timer, TimerClock};

/// Reports the failing location and fails with the runtime error `test failed`. Inside a
/// timer callback, which cannot return an error, the failure aborts the test.
macro_rules! bug {
    () => {{
        eprintln!("ERROR @ {}:{}", file!(), line!());
        panic!("{}", RuntimeError("test failed".to_string()));
    }};
}

/// Reports a passed check with its location.
macro_rules! ok {
    () => {
        eprintln!("OK @ {}:{}", file!(), line!());
    };
}

/// The pin that arming or cancelling a timer takes.
///
/// # Safety
/// `t` is a field of a `TimerTest`, which lives in its box in main() until the app exits,
/// or a boxed timer: it does not move while armed.
unsafe fn pin_timer<C: TimerClock>(t: &mut Timer<C>) -> Pin<&mut Timer<C>> {
    // SAFETY: the caller's contract.
    unsafe { Pin::new_unchecked(t) }
}

struct TimerTest<C: TimerClock> {
    t1: Timer<C>,
    t2: Timer<C>,
    t3: Timer<C>,
    t4: Timer<C>,
    t5: Timer<C>,
    pr1: Promise<()>,
    pr2: Promise<()>,
}

impl<C: TimerClock + 'static> TimerTest<C> {
    fn new() -> Self {
        TimerTest {
            t1: Timer::new(),
            t2: Timer::new(),
            t3: Timer::new(),
            t4: Timer::new(),
            t5: Timer::new(),
            pr1: Promise::new(),
            pr2: Promise::new(),
        }
    }

    fn run(&mut self) -> Future<()> {
        let this: *mut Self = self;
        // SAFETY: the test object lives in its box in main()
        // until the app exits, so `this` and its timers stay valid and in place.
        unsafe { pin_timer(&mut self.t1) }.set_callback(move || {
            // SAFETY: the test object lives in its box until the app exits, and the callback runs alone on this shard.
            let me = unsafe { &mut *this };
            ok!();
            println!(" 500ms timer expired");
            // SAFETY: the timers are fields of the boxed test object, which stays in place until the app exits.
            if !unsafe { pin_timer(&mut me.t4) }.cancel() {
                bug!();
            }
            // SAFETY: the timers are fields of the boxed test object, which stays in place until the app exits.
            if !unsafe { pin_timer(&mut me.t5) }.cancel() {
                bug!();
            }
            // SAFETY: the timers are fields of the boxed test object, which stays in place until the app exits.
            unsafe { pin_timer(&mut me.t5) }.arm_in(Duration::from_millis(1100));
        });
        // SAFETY: the timers are fields of the boxed test object, which stays in place until the app exits.
        unsafe { pin_timer(&mut self.t2) }.set_callback(|| {
            ok!();
            println!(" 900ms timer expired");
        });
        // SAFETY: the timers are fields of the boxed test object, which stays in place until the app exits.
        unsafe { pin_timer(&mut self.t3) }.set_callback(|| {
            ok!();
            println!("1000ms timer expired");
        });
        // SAFETY: the timers are fields of the boxed test object, which stays in place until the app exits.
        unsafe { pin_timer(&mut self.t4) }.set_callback(|| {
            ok!();
            println!("  BAD cancelled timer expired");
        });
        // SAFETY: the timers are fields of the boxed test object, which stays in place until the app exits.
        unsafe { pin_timer(&mut self.t5) }.set_callback(move || {
            ok!();
            println!("1600ms rearmed timer expired");
            // SAFETY: the test object lives in its box until the app exits.
            unsafe { (*this).pr1.set_value(()) };
        });
        // SAFETY: the timers are fields of the boxed test object, which stays in place until the app exits.
        unsafe { pin_timer(&mut self.t1) }.arm_in(Duration::from_millis(500));
        // SAFETY: the timers are fields of the boxed test object, which stays in place until the app exits.
        unsafe { pin_timer(&mut self.t2) }.arm_in(Duration::from_millis(900));
        // SAFETY: the timers are fields of the boxed test object, which stays in place until the app exits.
        unsafe { pin_timer(&mut self.t3) }.arm_in(Duration::from_millis(1000));
        // SAFETY: the timers are fields of the boxed test object, which stays in place until the app exits.
        unsafe { pin_timer(&mut self.t4) }.arm_in(Duration::from_millis(700));
        // SAFETY: the timers are fields of the boxed test object, which stays in place until the app exits.
        unsafe { pin_timer(&mut self.t5) }.arm_in(Duration::from_millis(800));
        self.pr1
            .get_future()
            // SAFETY: the test object lives in its box until the app exits.
            .then(move |()| unsafe { (*this).test_timer_cancelling() })
            // SAFETY: the test object lives in its box until the app exits.
            .then(move |()| unsafe { (*this).test_timer_with_scheduling_groups() })
    }

    fn test_timer_cancelling(&mut self) -> Future<()> {
        let t1: *mut Timer<C> = Box::into_raw(Box::new(Timer::<C>::new()));
        let this: *mut Self = self;
        // SAFETY: `t1` is freed in the continuation below, after its last use.
        unsafe {
            pin_timer(&mut *t1).set_callback(|| bug!());
            pin_timer(&mut *t1).arm_in(Duration::from_millis(100));
            pin_timer(&mut *t1).cancel();

            pin_timer(&mut *t1).arm_in(Duration::from_millis(100));
            pin_timer(&mut *t1).cancel();

            pin_timer(&mut *t1).set_callback(move || {
                ok!();
                (*this).pr2.set_value(());
            });
            pin_timer(&mut *t1).arm_in(Duration::from_millis(100));
        }
        self.pr2.get_future().then(move |()| {
            // SAFETY: allocated above with Box::new; nothing uses it any more.
            drop(unsafe { Box::from_raw(t1) });
        })
    }

    fn test_timer_with_scheduling_groups(&mut self) -> Future<()> {
        async_(|| -> Result<(), ExceptionPtr> {
            let sg1 = create_scheduling_group_with_shares("sg1".into(), 100.0).get()?;
            let sg2 = create_scheduling_group_with_shares("sg2".into(), 100.0).get()?;
            let t1attr = ThreadAttributes { sched_group: Some(sg1), ..ThreadAttributes::default() };
            let expirations = Rc::new(Cell::new(0));
            let exp = expirations.clone();
            async_with_attributes(t1attr, move || -> Result<(), ExceptionPtr> {
                let make_callback_checking_sg = |sg_to_check: SchedulingGroup| {
                    let expirations = exp.clone();
                    move || {
                        expirations.set(expirations.get() + 1);
                        if current_scheduling_group() != sg_to_check {
                            bug!();
                        }
                    }
                };
                let mut t1 = pin!(Timer::<C>::with_callback(make_callback_checking_sg(sg1)));
                t1.as_mut().arm_in(Duration::from_millis(10));
                let mut t2 = pin!(Timer::<C>::with_group(sg2, make_callback_checking_sg(sg2)));
                t2.as_mut().arm_in(Duration::from_millis(10));
                sleep(Duration::from_millis(500)).get()?;
                if exp.get() != 2 {
                    bug!();
                }
                ok!();
                Ok(())
            })
            .get()?;
            destroy_scheduling_group(sg1).get()?;
            destroy_scheduling_group(sg2).get()?;
            drop(expirations);
            Ok(())
        })
    }
}

pub(crate) fn main() {
    let mut app = AppTemplate::new(Default::default());
    let t1: *mut TimerTest<SteadyClockType> = Box::into_raw(Box::new(TimerTest::new()));
    let t2: *mut TimerTest<LowresClock> = Box::into_raw(Box::new(TimerTest::new()));
    let args: Vec<String> = std::env::args().collect();
    let status = app.run_deprecated(
        args,
        Box::new(move || {
            println!("=== Start High res clock test");
            // SAFETY: both tests live until the end of main().
            let _ = unsafe { (*t1).run() }
                .then(move |()| {
                    println!("=== Start Low  res clock test");
                    // SAFETY: both tests live in their boxes until the end of main().
                    unsafe { (*t2).run() }
                })
                .then(|()| {
                    println!("Done");
                    Reactor::exit(0);
                });
        }),
    );
    // SAFETY: the reactor has stopped; nothing refers to the tests any more.
    unsafe {
        drop(Box::from_raw(t1));
        drop(Box::from_raw(t2));
    }
    std::process::exit(status);
}
