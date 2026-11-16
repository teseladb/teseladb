//! Tests of the test and benchmark support: benchmark runs of every body kind (synchronous,
//! with an iteration count, ready and pending futures, failures), manually measured
//! iterations, benchmark statistics and registration, the optimisation barrier, the
//! performance counters, test registration and filtering, exception predicates and the
//! blocking exchanger.
#![cfg(target_os = "linux")]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{
    ExceptionPtr, Future, RuntimeError, make_exception_future, make_ready_future,
};
use teseladb::engine::core::reactor::{later, yield_};
use teseladb::engine::testing::engine_test::testing::exception_predicate::{
    message_contains, message_equals,
};
use teseladb::engine::testing::engine_test::testing::{
    EngineTest, get_name, register_test, run_harness_test, run_registered_tests, run_test,
};
use teseladb::engine::testing::exchanger::testing::Exchanger;
use teseladb::engine::testing::linux_perf_event::{LinuxPerfEvent, PerfEventAttr};
use teseladb::engine::testing::perf_tests::perf_tests::internal::{
    ConcretePerformanceTest, PerfStats, PerfTestBody, PerformanceTest, PerformanceTestBase,
    RunResult, TestRet, all_tests, measure_time, test_registrar,
};
use teseladb::engine::testing::perf_tests::perf_tests::{
    do_not_optimize, start_measuring_time, stop_measuring_time,
};
use teseladb::engine_test_case;

// ---------------------------------------------------------------------------------------
// Benchmark bodies, one per result kind

#[derive(Default)]
struct SyncUnit(u64);
impl PerfTestBody for SyncUnit {
    type Ret = ();
    fn run(&mut self) {
        self.0 += 1;
        do_not_optimize(&self.0);
    }
}

#[derive(Default)]
struct SyncMany;
impl PerfTestBody for SyncMany {
    type Ret = usize;
    fn run(&mut self) -> usize {
        4
    }
}

/// Ready on even calls, yields on odd ones.
#[derive(Default)]
struct AsyncMixed(u64);
impl PerfTestBody for AsyncMixed {
    type Ret = Future<()>;
    fn run(&mut self) -> Future<()> {
        self.0 += 1;
        if self.0.is_multiple_of(2) { make_ready_future(()) } else { yield_() }
    }
}

/// Fails on the third call.
#[derive(Default)]
struct AsyncFailing(u64);
impl PerfTestBody for AsyncFailing {
    type Ret = Future<()>;
    fn run(&mut self) -> Future<()> {
        self.0 += 1;
        if self.0 == 3 {
            return make_exception_future(ExceptionPtr::new(RuntimeError::new("third run")));
        }
        make_ready_future(())
    }
}

/// Two ready iterations, then three after a reactor round.
#[derive(Default)]
struct IterMixed(u64);
impl PerfTestBody for IterMixed {
    type Ret = Future<usize>;
    fn run(&mut self) -> Future<usize> {
        self.0 += 1;
        if self.0.is_multiple_of(2) {
            return make_ready_future(2);
        }
        make_coroutine(async move {
            later().await?;
            Ok(3usize)
        })
    }
}

#[derive(Default)]
struct IterFailing;
impl PerfTestBody for IterFailing {
    type Ret = Future<usize>;
    fn run(&mut self) -> Future<usize> {
        make_exception_future(ExceptionPtr::new(RuntimeError::new("no iterations")))
    }
}

/// Measures only the middle of each iteration.
#[derive(Default)]
struct ManuallyTimed(u64);
impl PerfTestBody for ManuallyTimed {
    type Ret = ();
    fn run(&mut self) {
        self.0 += 1;
        start_measuring_time();
        do_not_optimize(&self.0);
        stop_measuring_time();
    }
}

/// Runs one benchmark run of `T` limited to `max` iterations; returns the iterations done
/// and the run's result.
async fn run_once<T: PerfTestBody>(max: u64) -> Result<(u64, RunResult), ExceptionPtr> {
    let mut t = ConcretePerformanceTest::<T>::new("case", "group");
    t.set_up();
    t.base_mut()._max_single_run_iterations.store(max, Ordering::Relaxed);
    let r = t.do_single_run().await;
    let n = t.base()._single_run_iterations;
    t.tear_down();
    r.map(|rr| (n, rr))
}

engine_test_case!(benchmark_runs_count_their_iterations, {
    make_coroutine(async move {
        let (n, r) = run_once::<SyncUnit>(10).await?;
        assert_eq!(n, 10);
        assert!(r.duration < Duration::from_secs(10));

        let (n, _) = run_once::<SyncMany>(10).await?;
        assert_eq!(n, 12, "three runs of four");

        let (n, r) = run_once::<AsyncMixed>(6).await?;
        assert_eq!(n, 6);
        assert!(r.stats.tasks_executed > 0, "the yields ran tasks");

        let (n, _) = run_once::<IterMixed>(9).await?;
        assert_eq!(n, 10, "3 + 2 + 3 + 2");

        let (n, r) = run_once::<ManuallyTimed>(5).await?;
        assert_eq!(n, 5);
        assert!(r.duration < Duration::from_secs(10));
        Ok(())
    })
});

engine_test_case!(failing_benchmark_runs_report_the_error, {
    make_coroutine(async move {
        let e = run_once::<AsyncFailing>(10).await.unwrap_err();
        assert!(message_equals("third run")(&e));
        let e = run_once::<IterFailing>(10).await.unwrap_err();
        assert!(message_contains("iterations")(&e));
        // A test whose fixture was never set up runs no iteration.
        let mut t = ConcretePerformanceTest::<SyncUnit>::new("unset", "group");
        t.base_mut()._max_single_run_iterations.store(3, Ordering::Relaxed);
        let r = t.do_single_run().await?;
        assert_eq!(t.base()._single_run_iterations, 0);
        assert!(r.duration < Duration::from_secs(10));
        Ok(())
    })
});

engine_test_case!(statistics_snapshots_follow_the_reactor, {
    make_coroutine(async move {
        let a = PerfStats::snapshot(None, None);
        yield_().await?;
        let v: Vec<u64> = (0..64).collect();
        do_not_optimize(&v);
        let b = PerfStats::snapshot(None, None);
        assert!(b.tasks_executed >= a.tasks_executed);
        assert!(b.allocations >= a.allocations);
        assert_eq!(a.instructions_retired, 0);
        assert_eq!(a.cpu_cycles_retired, 0);
        // Iterations measured outside a run only accumulate.
        start_measuring_time();
        stop_measuring_time();
        let _ = measure_time();
        assert!(get_name().contains("statistics_snapshots_follow_the_reactor"));
        Ok(())
    })
});

#[test]
fn statistics_add_and_subtract_with_wrapping() {
    let a = PerfStats::new(1, 2, 3, 4);
    let b = PerfStats::new(10, 20, 30, 40);
    assert_eq!(a + b, PerfStats::new(11, 22, 33, 44));
    assert_eq!(b - a, PerfStats::new(9, 18, 27, 36));
    let w = a - b;
    assert_eq!(w.allocations, 1u64.wrapping_sub(10));
    assert_eq!(w + b, a);
    let mut c = b;
    let taken = PerfStats::take(&mut c);
    assert_eq!(taken, b);
    assert_eq!(c, PerfStats::default());
    let r = RunResult::default();
    assert_eq!(r.duration, Duration::ZERO);
}

#[test]
fn benchmarks_register_with_their_group_and_case() {
    let base = PerformanceTestBase::new("case_x", "group_y");
    assert_eq!(base.name(), "group_y.case_x");
    assert_eq!(base.test_case(), "case_x");
    assert_eq!(base.test_group(), "group_y");
    // A fresh base has a limit of zero iterations: it is already done.
    assert!(base.stop_iteration());

    test_registrar::<SyncMany>("cov_group", "cov_case");
    let names: Vec<String> = all_tests().iter().map(|t| t.base().name()).collect();
    assert!(names.contains(&"cov_group.cov_case".to_owned()), "{names:?}");
}

#[test]
fn results_used_as_another_kind_are_rejected() {
    fn rejected<R>(f: impl FnOnce() -> R) {
        assert!(catch_unwind(AssertUnwindSafe(f)).is_err());
    }
    rejected(|| ().unready());
    rejected(|| ().iterations_future());
    rejected(|| 5usize.unready());
    rejected(|| 5usize.iterations_future());
    rejected(|| make_ready_future(()).sync_iterations());
    rejected(|| make_ready_future(()).iterations_future());
    rejected(|| make_ready_future(5usize).sync_iterations());
    rejected(|| make_ready_future(5usize).unready());
    // The supported kinds.
    assert_eq!(().sync_iterations(), 1);
    assert_eq!(7usize.sync_iterations(), 7);
    assert!(make_ready_future(()).unready().is_none());
    assert_eq!(make_ready_future(3usize).iterations_future().get().unwrap(), 3);
    const { assert!(!<() as TestRet>::IS_ASYNC && !<() as TestRet>::RETURNS_ITERATIONS) };
    const { assert!(<Future<usize> as TestRet>::IS_ASYNC) };
}

#[test]
fn the_optimisation_barrier_accepts_any_value() {
    do_not_optimize(&1u8);
    do_not_optimize(&-1i8);
    do_not_optimize(&2u16);
    do_not_optimize(&-2i16);
    do_not_optimize(&3u32);
    do_not_optimize(&-3i32);
    do_not_optimize(&4u64);
    do_not_optimize(&-4i64);
    do_not_optimize(&5usize);
    do_not_optimize(&-5isize);
    do_not_optimize(&true);
    do_not_optimize(&1.5f64);
    do_not_optimize("a str");
    do_not_optimize(&[1u8, 2, 3][..]);
    do_not_optimize(&String::from("owned"));
}

/// The descriptor of a counter, from its debug form (`LinuxPerfEvent { _fd: N }`).
fn counter_fd(e: &LinuxPerfEvent) -> i64 {
    let s = format!("{e:?}");
    let digits: String = s.chars().filter(|c| c.is_ascii_digit() || *c == '-').collect();
    digits.parse().unwrap()
}

#[test]
fn performance_counters_read_zero_when_unavailable() {
    let mut ins = LinuxPerfEvent::user_instructions_retired();
    let mut cyc = LinuxPerfEvent::user_cpu_cycles_retired();
    ins.enable();
    cyc.enable();
    let mut acc = 0u64;
    for i in 0..100_000u64 {
        acc = acc.wrapping_add(i * i);
    }
    do_not_optimize(&acc);
    ins.disable();
    cyc.disable();
    let (i, c) = (ins.read(), cyc.read());
    if counter_fd(&ins) == -1 {
        assert_eq!(i, 0);
    }
    if counter_fd(&cyc) == -1 {
        assert_eq!(c, 0);
    }

    // A software clock counter of this task, where the kernel allows it.
    const PERF_TYPE_SOFTWARE: u32 = 1;
    const PERF_COUNT_SW_TASK_CLOCK: u64 = 1;
    const DISABLED_EXCLUDE_KERNEL_HV: u64 = 1 | (1 << 5) | (1 << 6);
    let attr = PerfEventAttr {
        type_: PERF_TYPE_SOFTWARE,
        size: size_of::<PerfEventAttr>() as u32,
        config: PERF_COUNT_SW_TASK_CLOCK,
        flags: DISABLED_EXCLUDE_KERNEL_HV,
        ..PerfEventAttr::default()
    };
    let mut clock = LinuxPerfEvent::new(&attr, 0, -1, -1, 0);
    clock.enable();
    let mut acc = 0u64;
    for i in 0..1_000_000u64 {
        acc = acc.wrapping_add(i ^ acc);
    }
    do_not_optimize(&acc);
    clock.disable();
    let elapsed = clock.read();
    if counter_fd(&clock) == -1 {
        assert_eq!(elapsed, 0);
    } else {
        assert!(elapsed > 0);
    }

    // Assignment closes the counter it replaces and takes over the other one.
    let mut other = LinuxPerfEvent::new(&attr, 0, -1, -1, 0);
    let clock_fd = counter_fd(&clock);
    other.assign(clock);
    assert_eq!(counter_fd(&other), clock_fd);
    let mut unavailable = LinuxPerfEvent::new(&PerfEventAttr::default(), -2, -2, -2, 0);
    assert_eq!(counter_fd(&unavailable), -1);
    assert_eq!(unavailable.read(), 0);
    unavailable.assign(other);
}

// ---------------------------------------------------------------------------------------
// Test registration and running

struct PassingCase;
impl EngineTest for PassingCase {
    fn name(&self) -> &'static str {
        "cov_misc_passing_case"
    }
    fn get_test_file(&self) -> &'static str {
        file!()
    }
    fn run_test_case(&self) -> Future<()> {
        make_ready_future(())
    }
}

struct FailingCase;
impl EngineTest for FailingCase {
    fn name(&self) -> &'static str {
        "cov_misc_failing_case"
    }
    fn get_test_file(&self) -> &'static str {
        file!()
    }
    fn run_test_case(&self) -> Future<()> {
        make_exception_future(ExceptionPtr::new(RuntimeError::new("cov failing case")))
    }
}

struct PanickingCase;
impl EngineTest for PanickingCase {
    fn name(&self) -> &'static str {
        "cov_misc_panicking_case"
    }
    fn get_test_file(&self) -> &'static str {
        file!()
    }
    fn run_test_case(&self) -> Future<()> {
        panic!("cov panicking case")
    }
}

static PASSING: PassingCase = PassingCase;
static FAILING: FailingCase = FailingCase;
static PANICKING: PanickingCase = PanickingCase;

fn init_ok(_: &[String]) -> bool {
    true
}

fn init_fails(_: &[String]) -> bool {
    false
}

#[test]
fn tests_run_and_registered_tests_filter_by_name() {
    // Starts the shared test reactor if no other test has yet.
    run_harness_test(&PASSING);
    assert_eq!(PASSING.get_test_line(), 0);
    PASSING.run().unwrap();
    let e = run_test(&FAILING).unwrap_err();
    assert!(message_equals("cov failing case")(&e));
    assert!(!message_equals("another message")(&e));
    assert!(!message_contains("absent")(&e));
    // A body that panics before returning its future fails with the panic message.
    let e = run_test(&PANICKING).unwrap_err();
    assert!(message_equals("cov panicking case")(&e));

    register_test(&FAILING);
    register_test(&PASSING);
    let args = |filter: &str| vec!["prog".to_owned(), format!("--run_test={filter}")];
    assert_eq!(run_registered_tests(init_fails, &args("cov_misc_failing_case")), 200);
    assert_eq!(run_registered_tests(init_ok, &args("no_such_test")), 0);
    assert_eq!(run_registered_tests(init_ok, &args("cov_misc_passing_case")), 0);
    assert_eq!(run_registered_tests(init_ok, &args("cov_misc_failing_case")), 1);
}

#[test]
fn exception_predicates_handle_a_null_pointer() {
    let null = ExceptionPtr::null();
    assert!(message_equals("")(&null));
    assert!(!message_equals("x")(&null));
    assert!(message_contains("")(&null));
}

// ---------------------------------------------------------------------------------------
// Exchanger

#[test]
fn exchangers_hand_values_over_and_report_interruptions() {
    let ex = Arc::new(Exchanger::<u32>::default());
    let ex2 = ex.clone();
    let giver = std::thread::spawn(move || {
        for v in 1..=3 {
            ex2.give(v).unwrap();
        }
    });
    let got: Vec<u32> = (0..3).map(|_| ex.take().unwrap()).collect();
    giver.join().unwrap();
    assert_eq!(got, vec![1, 2, 3]);

    // An interruption wakes a waiting taker and fails every later call with the first error.
    let ex2 = ex.clone();
    let taker = std::thread::spawn(move || ex2.take().map_err(|e| e.to_string()));
    std::thread::sleep(Duration::from_millis(20));
    ex.interrupt(RuntimeError::new("stop"));
    ex.interrupt(RuntimeError::new("ignored"));
    let r = taker.join().unwrap();
    assert!(r.unwrap_err().contains("stop"));
    let e = ex.give(9).unwrap_err();
    assert!(message_equals("stop")(&e));
    assert!(message_equals("stop")(&ex.take().unwrap_err()));
}
