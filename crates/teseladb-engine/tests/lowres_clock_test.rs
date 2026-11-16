//! Accuracy of the low-resolution clocks: the steady clock against the high-resolution one,
//! with and without preemption, and the system clock against the wall clock.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::do_with::do_with;
use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::core::loop_::{StopIteration, repeat};
use teseladb::engine::core::lowres_clock::{LowresClock, LowresSystemClock};
use teseladb::engine::core::reactor::yield_;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine_test_case;

//
// Sanity check the accuracy of the steady low-resolution clock.
//
engine_test_case!(steady_clock_sanity, {
    do_with(LowresClock::now(), |t1| {
        const SLEEP_DURATION: Duration = Duration::from_millis(100);
        let t1 = *t1;
        sleep(SLEEP_DURATION).then(move |()| {
            let elapsed = LowresClock::now() - t1;
            // `elapsed` is the duration of `t2 - t1`, counted in nanoseconds.
            let minimum_elapsed = SLEEP_DURATION.mul_f64(0.9).as_nanos() as i64;

            assert!(elapsed >= minimum_elapsed);

            make_ready_future(())
        })
    })
});

//
// Sanity check the accuracy of the steady low-resolution clock in debug mode
// where preemption is handled differently.
//
engine_test_case!(steady_clock_sanity_preempt, {
    make_coroutine(async move {
        let t1 = LowresClock::now();
        // Sleep duration must be higher than the task quota to ensure that
        // preemption is requested before we yield.
        const SLEEP_DURATION: Duration = Duration::from_millis(100);
        std::thread::sleep(SLEEP_DURATION);
        // Yield to the reactor to give it a chance to update the
        // low-resolution clock. Yield just schedules an empty task and
        // waits for it to be executed. There is nothing special about it.
        yield_().await?;
        let elapsed = LowresClock::now() - t1;
        // `elapsed` is the duration of `now - t1`, counted in nanoseconds.
        let minimum_elapsed = SLEEP_DURATION.mul_f64(0.9).as_nanos() as i64;
        assert!(elapsed >= minimum_elapsed);
        Ok(())
    })
});

/// The `struct tm` of the C library.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Tm {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_year: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_isdst: i32,
    tm_gmtoff: i64,
    tm_zone: usize,
}

unsafe extern "C" {
    fn localtime_r(t: *const i64, result: *mut Tm) -> *mut Tm;
}

fn localtime(t: i64) -> Tm {
    let mut tm = Tm::default();
    // SAFETY: both pointers are valid for the call.
    unsafe { localtime_r(&t, &mut tm) };
    tm
}

//
// At the very least, we can verify that the low-resolution system clock is within a second of the
// high-resolution system clock.
//
engine_test_case!(system_clock_sanity, {
    let check_matching = || {
        let system_time = SystemTime::now();
        let lowres_time = LowresSystemClock::now();

        let t1 = system_time.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let t2 = LowresSystemClock::to_time_t(lowres_time);

        let lt1 = localtime(t1);
        let lt2 = localtime(t2);

        (lt1.tm_isdst == lt2.tm_isdst)
            && (lt1.tm_year == lt2.tm_year)
            && (lt1.tm_mon == lt2.tm_mon)
            && (lt1.tm_yday == lt2.tm_yday)
            && (lt1.tm_mday == lt2.tm_mday)
            && (lt1.tm_wday == lt2.tm_wday)
            && (lt1.tm_hour == lt2.tm_hour)
            && (lt1.tm_min == lt2.tm_min)
            && (lt1.tm_sec == lt2.tm_sec)
    };

    //
    // Check two out of three samples in order to account for the possibility that the high-resolution clock backing
    // the low-resoltuion clock was captured in the range of the 990th to 999th millisecond of the second. This would
    // make the low-resolution clock and the high-resolution clock disagree on the current second.
    //

    let index = Rc::new(Cell::new(0usize));
    let success_count = Rc::new(Cell::new(0usize));
    repeat(move || {
        if index.get() >= 3 {
            assert!(success_count.get() >= 2usize);
            return make_ready_future(StopIteration::YES);
        }

        let index = index.clone();
        let success_count = success_count.clone();
        sleep(Duration::from_millis(10)).then(move |()| {
            if check_matching() {
                success_count.set(success_count.get() + 1);
            }

            index.set(index.get() + 1);
            make_ready_future(StopIteration::NO)
        })
    })
});

//
// Verify that the low-resolution clock updates its reported time point over time.
//
engine_test_case!(system_clock_dynamic, {
    do_with(LowresSystemClock::now(), |t1| {
        let t1 = *t1;
        sleep(Duration::from_millis(100)).then(move |()| {
            let t2 = LowresSystemClock::now();
            assert_ne!(t1.nanos_since_epoch(), t2.nanos_since_epoch());

            make_ready_future(())
        })
    })
});
