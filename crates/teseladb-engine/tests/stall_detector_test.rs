//! Tests of the stall detector: no reports for cooperative tasks, reports for stalls in user
//! space and in the kernel, report rate limiting, and no crash under many reports.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[cfg(engine_debug)]
use teseladb::engine::core::thread::async_ as thread_async;
#[cfg(engine_debug)]
use teseladb::engine_test_case;

#[cfg(not(engine_debug))]
mod not_debug {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, LazyLock};
    use std::time::{Duration, Instant};

    use teseladb::engine::core::future::{ExceptionPtr, Promise, RuntimeError, make_ready_future};
    use teseladb::engine::core::internal::stall_detector::ClockType as CpuStallDetectorClock;
    use teseladb::engine::core::loop_::parallel_for_each;
    use teseladb::engine::core::preempt::need_preempt;
    use teseladb::engine::core::reactor::{Milliseconds, ReactorTest, engine, yield_};
    use teseladb::engine::core::thread::Thread;
    use teseladb::engine::core::thread::async_ as thread_async;
    use teseladb::engine::core::thread_cputime_clock::ThreadCputimeDuration;
    use teseladb::engine::util::log::Logger;
    use teseladb::{engine_test_case, log_info};

    static TESTLOG: LazyLock<std::pin::Pin<Box<Logger>>> =
        LazyLock::new(|| Logger::new("testlog").expect("logger registered once"));

    type ReportFn = Option<Arc<dyn Fn() + Send + Sync>>;

    struct TemporaryStallDetectorSettings {
        old_threshold: Milliseconds,
        old_report: ReportFn,
    }

    impl TemporaryStallDetectorSettings {
        /// Temporarily (until destructor) overload the stall detector threshold and reporting function.
        ///
        /// Also resets the reported stalls counter to zero, so the next backtraces will not be supressed.
        fn new(threshold: Duration, report: ReportFn) -> Self {
            // SAFETY: on this shard's reactor thread; the reactor is reached for this call only.
            let old_threshold = unsafe { (*engine()).get_blocked_reactor_notify_ms() };
            let old_report = ReactorTest::get_stall_detector_report_function();
            // SAFETY: as above.
            unsafe {
                (*engine()).update_blocked_reactor_notify_ms(Duration::from_millis(
                    threshold.as_millis() as u64,
                ))
            };
            ReactorTest::set_stall_detector_report_function(report);
            TemporaryStallDetectorSettings { old_threshold, old_report }
        }
    }

    impl Drop for TemporaryStallDetectorSettings {
        fn drop(&mut self) {
            // SAFETY: on this shard's reactor thread; the reactor is reached for this call only.
            unsafe { (*engine()).update_blocked_reactor_notify_ms(self.old_threshold) };
            ReactorTest::set_stall_detector_report_function(self.old_report.take());
        }
    }

    fn counter(reports: &Arc<AtomicU32>) -> ReportFn {
        let r = reports.clone();
        Some(Arc::new(move || {
            r.fetch_add(1, Ordering::Relaxed);
        }))
    }

    fn spin(how_much: Duration, body: &dyn Fn()) {
        let end = CpuStallDetectorClock::now() + ThreadCputimeDuration::from_std(how_much);
        while CpuStallDetectorClock::now() < end {
            body(); // spin!
        }
    }

    fn spin_user_hires(how_much: Duration) {
        let end = Instant::now() + how_much;
        while Instant::now() < end {}
    }

    fn spin_some_cooperatively(how_much: Duration, body: &dyn Fn()) {
        let end = Instant::now() + how_much;
        while Instant::now() < end {
            spin(Duration::from_micros(200), body);
            if need_preempt() {
                Thread::yield_();
            }
        }
    }

    fn nothing() {}

    engine_test_case!(normal_case, {
        thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
            let reports = Arc::new(AtomicU32::new(0));
            let _tsds =
                TemporaryStallDetectorSettings::new(Duration::from_millis(10), counter(&reports));
            spin_some_cooperatively(Duration::from_secs(1), &nothing);
            assert_eq!(reports.load(Ordering::Relaxed), 0);
            Ok(())
        })
    });

    engine_test_case!(simple_stalls, {
        thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
            let reports = Arc::new(AtomicU32::new(0));
            let _tsds =
                TemporaryStallDetectorSettings::new(Duration::from_millis(10), counter(&reports));
            let nr = 10u32;
            for _ in 0..nr {
                spin_some_cooperatively(Duration::from_millis(100), &nothing);
                spin(Duration::from_millis(20), &nothing);
            }
            spin_some_cooperatively(Duration::from_millis(100), &nothing);

            // blocked-reactor-reports-per-minute defaults to 5, so we don't
            // get all 10 reports.
            assert_eq!(reports.load(Ordering::Relaxed), 5);
            Ok(())
        })
    });

    engine_test_case!(no_poll_no_stall, {
        thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
            let reports = Arc::new(AtomicU32::new(0));
            let _tsds =
                TemporaryStallDetectorSettings::new(Duration::from_millis(10), counter(&reports));
            spin_some_cooperatively(Duration::from_millis(1), &nothing); // need to yield so that stall detector change from above take effect
            const TASKS: u32 = 2000;
            let mut p = Promise::<()>::new();
            let f = p.get_future();
            let pp: *mut Promise<()> = &mut p;
            parallel_for_each(0..TASKS, move |i: u32| {
                let _ = yield_().then(move |()| {
                    spin(Duration::from_micros(500), &nothing);
                    if i == TASKS - 1 {
                        // SAFETY: `p` lives until `f` resolves, below.
                        unsafe { (*pp).set_value(()) };
                    }
                });
                make_ready_future(())
            })
            .get()?;
            f.get()?;
            assert_eq!(reports.load(Ordering::Relaxed), 0);
            Ok(())
        })
    });

    // Triggers stalls by spinning with a specify "body" function
    // which takes most of the spin time.
    fn test_spin_with_body(what: &str, body: &dyn Fn()) {
        // The !count_stacks mode outputs stall notification to stderr as usual
        // and do not assert anything, but are intended for diagnosing
        // stall problems by inspecting the output. We expect the userspace
        // spin test to show no kernel callstack, and the kernel test to
        // show kernel backtraces in the mmap or munmap path, but this is
        // not exact since neither test spends 100% of its time in the
        // selected mode (of course, kernel stacks only appear if the
        // perf-based stall detected could be enabled).
        //
        // Then the count_stacks mode tests that the right number of stacks
        // were output.
        for count_stacks in [false, true] {
            log_info!(TESTLOG, "Starting spin test: {}", what);
            let reports = Arc::new(AtomicU32::new(0));
            let reporter = if count_stacks { counter(&reports) } else { None };
            let _tsds = TemporaryStallDetectorSettings::new(Duration::from_millis(10), reporter);
            const NR: u32 = 5;
            for _ in 0..NR {
                spin_some_cooperatively(Duration::from_millis(100), body);
                spin(Duration::from_millis(20), body);
            }
            log_info!(TESTLOG, "Ending spin test: {}", what);
            assert_eq!(reports.load(Ordering::Relaxed), if count_stacks { 5 } else { 0 });
        }
    }

    engine_test_case!(spin_in_userspace, {
        thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
            // a body which spends almost all of its time in userspace
            test_spin_with_body("userspace", &|| spin_user_hires(Duration::from_millis(1)));
            Ok(())
        })
    });

    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;
    const MAP_PRIVATE: i32 = 0x02;
    const MAP_ANONYMOUS: i32 = 0x20;
    const MAP_POPULATE: i32 = 0x8000;
    const MAP_FAILED: *mut u8 = !0usize as *mut u8;

    unsafe extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
        fn munmap(addr: *mut u8, len: usize) -> i32;
    }

    fn mmap_populate(len: usize) {
        // SAFETY: an anonymous private mapping, unmapped right after.
        let p = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_POPULATE,
                0,
                0,
            )
        };
        assert!(p != MAP_FAILED);
        // SAFETY: `p` is the mapping of `len` bytes made above.
        assert!(unsafe { munmap(p, len) } == 0);
    }

    engine_test_case!(spin_in_kernel, {
        thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
            // a body which spends almost all of its time in the kernel
            // doing 128K mmaps
            test_spin_with_body("kernel", &|| mmap_populate(128 * 1024));
            Ok(())
        })
    });

    fn recursive_thrower(x: i32) -> Result<(), ExceptionPtr> {
        if x <= 0 {
            return Err(ExceptionPtr::new(RuntimeError("foo".to_string())));
        }
        if let Err(e) = recursive_thrower(x - 1)
            && (x & 0xF) != 0
        {
            return Err(e);
        }
        Ok(())
    }

    // This test reproduces a crash of the stall detector under frequent reports.
    // The issue is reproduced most quickly using the following arguments:
    // --blocked-reactor-notify-ms=1
    // but it will happen with default arguments as well.
    engine_test_case!(stall_detector_crash, {
        thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
            // increase total_iters to increase chance of failure
            // the value below is tuned to take about 1 second in
            // release builds
            const TOTAL_ITERS: i32 = 100000;
            const MAX_DEPTH: i32 = 20;

            let now = Instant::now;

            let mut next_yield = now();
            for a in 1..TOTAL_ITERS {
                if now() > next_yield {
                    // we need to periodically yield or else the stall reports will become
                    // less and less frequent as exponentially grow the report interval while
                    // the same task is running
                    Thread::yield_();
                    next_yield = now() + Duration::from_millis(40);

                    // the next line resets the suppression state which allows many more reports
                    // per second, increasing the chance of a failure
                    ReactorTest::set_stall_detector_report_function(None);
                }
                let _ = recursive_thrower(a % MAX_DEPTH);
                if a % 100000 == 0 {
                    println!(
                        "Making progress: {:6.3}%",
                        100.0 * f64::from(a) / f64::from(TOTAL_ITERS)
                    );
                }
            }
            Ok(())
        })
    });
}

#[cfg(engine_debug)]
engine_test_case!(stall_detector_test_not_valid_in_debug_mode, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> { Ok(()) })
});
