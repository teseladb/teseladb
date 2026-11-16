//! The micro-benchmark framework.
//!
//! A benchmark is declared with [`perf_test!`](crate::perf_test) (no state) or
//! [`perf_test_f!`](crate::perf_test_f) (with a `Default` fixture), and the coroutine forms
//! [`perf_test_c!`](crate::perf_test_c) / [`perf_test_cn!`](crate::perf_test_cn). The body
//! is one iteration, or returns how many inner iterations it ran; it may return a future.
//! The runner in `benches/common` repeats each body on the reactor for a fixed duration,
//! several runs, and reports per-iteration time, allocations, tasks and (where the kernel
//! allows) instructions and cycles from [`LinuxPerfEvent`](crate::engine::testing::linux_perf_event::LinuxPerfEvent).
//!
//! The measured loops are hand-tuned: [`do_not_optimize`](perf_tests::do_not_optimize) and
//! the run loop keep the measured code from being removed while adding as little as
//! possible around it. Comments in this file record why each choice was made.

/// The benchmark declarations and runtime support.
#[allow(clippy::module_inception)]
pub mod perf_tests {
    /// Runtime support used by the declaration macros and the runner.
    pub mod internal {
        use std::cell::Cell;
        use std::ops::{Add, AddAssign, Sub, SubAssign};
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{Duration, Instant};

        use crate::engine::core::coroutine::make_coroutine;
        use crate::engine::core::future::{ExceptionPtr, Future};
        use crate::engine::core::memory;
        use crate::engine::core::preempt::need_preempt;
        use crate::engine::core::reactor::engine;
        use crate::engine::testing::linux_perf_event::LinuxPerfEvent;

        /// The monotonic clock used to time runs.
        pub type ClockType = Instant;

        /// The counters of a run.
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub struct PerfStats {
            /// The allocations.
            pub allocations: u64,
            /// The tasks executed.
            pub tasks_executed: u64,
            /// The instructions retired.
            pub instructions_retired: u64,
            /// The cpu cycles.
            pub cpu_cycles_retired: u64,
        }

        impl PerfStats {
            /// The counters given.
            #[inline]
            pub const fn new(
                allocations: u64,
                tasks_executed: u64,
                instructions_retired: u64,
                cpu_cycles_retired: u64,
            ) -> Self {
                PerfStats { allocations, tasks_executed, instructions_retired, cpu_cycles_retired }
            }

            /// Takes the counters of `o`, leaving zeros.
            #[inline]
            pub fn take(o: &mut PerfStats) -> Self {
                std::mem::take(o)
            }

            fn perf_mallocs() -> u64 {
                memory::stats().mallocs()
            }

            fn perf_tasks_processed() -> u64 {
                // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
                unsafe { (*engine()).get_sched_stats() }.tasks_processed
            }

            /// The counters now.
            pub fn snapshot(
                instructions_retired_counter: Option<&mut LinuxPerfEvent>,
                cpu_cycles_retired_counter: Option<&mut LinuxPerfEvent>,
            ) -> PerfStats {
                PerfStats::new(
                    Self::perf_mallocs(),
                    Self::perf_tasks_processed(),
                    instructions_retired_counter.map_or(0, |c| c.read()),
                    cpu_cycles_retired_counter.map_or(0, |c| c.read()),
                )
            }
        }

        impl Add for PerfStats {
            type Output = PerfStats;
            #[inline]
            fn add(mut self, b: PerfStats) -> PerfStats {
                self += b;
                self
            }
        }

        impl Sub for PerfStats {
            type Output = PerfStats;
            #[inline]
            fn sub(mut self, b: PerfStats) -> PerfStats {
                self -= b;
                self
            }
        }

        impl AddAssign for PerfStats {
            #[inline]
            fn add_assign(&mut self, b: PerfStats) {
                self.allocations = self.allocations.wrapping_add(b.allocations);
                self.tasks_executed = self.tasks_executed.wrapping_add(b.tasks_executed);
                self.instructions_retired =
                    self.instructions_retired.wrapping_add(b.instructions_retired);
                self.cpu_cycles_retired =
                    self.cpu_cycles_retired.wrapping_add(b.cpu_cycles_retired);
            }
        }

        impl SubAssign for PerfStats {
            #[inline]
            fn sub_assign(&mut self, b: PerfStats) {
                self.allocations = self.allocations.wrapping_sub(b.allocations);
                self.tasks_executed = self.tasks_executed.wrapping_sub(b.tasks_executed);
                self.instructions_retired =
                    self.instructions_retired.wrapping_sub(b.instructions_retired);
                self.cpu_cycles_retired =
                    self.cpu_cycles_retired.wrapping_sub(b.cpu_cycles_retired);
            }
        }

        /// The outcome of one run.
        #[derive(Clone, Copy, Debug, Default)]
        pub struct RunResult {
            /// How long the run took.
            pub duration: Duration,
            /// The counters of the run.
            pub stats: PerfStats,
        }

        /// The data members of `performance_test`.
        pub struct PerformanceTestBase {
            _test_case: String,
            _test_group: String,

            /// The iterations of the current run.
            pub _single_run_iterations: u64,
            /// The iterations a run stops at.
            pub _max_single_run_iterations: AtomicU64,
            /// The instructions counter.
            pub _instructions_retired_counter: LinuxPerfEvent,
            /// The cycles counter.
            pub _cpu_cycles_retired_counter: LinuxPerfEvent,
        }

        impl PerformanceTestBase {
            /// The data of test `test_case` of group `test_group`.
            pub fn new(test_case: &str, test_group: &str) -> Self {
                PerformanceTestBase {
                    _test_case: test_case.to_string(),
                    _test_group: test_group.to_string(),
                    _single_run_iterations: 0,
                    _max_single_run_iterations: AtomicU64::new(0),
                    _instructions_retired_counter: LinuxPerfEvent::user_instructions_retired(),
                    _cpu_cycles_retired_counter: LinuxPerfEvent::user_cpu_cycles_retired(),
                }
            }

            /// Whether the run reached its iterations.
            #[inline(always)]
            pub fn stop_iteration(&self) -> bool {
                self._single_run_iterations
                    >= self._max_single_run_iterations.load(Ordering::Relaxed)
            }

            /// Counts `n` more iterations.
            #[inline(always)]
            pub fn next_iteration(&mut self, n: usize) {
                self._single_run_iterations += n as u64;
            }

            /// The test case.
            pub fn test_case(&self) -> &str {
                &self._test_case
            }

            /// The test group.
            pub fn test_group(&self) -> &str {
                &self._test_group
            }

            /// The full name, `group.case`.
            pub fn name(&self) -> String {
                format!("{}.{}", self.test_group(), self.test_case())
            }
        }

        /// A microbenchmark (`performance_test`'s virtual interface).
        pub trait PerformanceTest: Send {
            /// The common data.
            fn base(&self) -> &PerformanceTestBase;
            /// The common data, mutable.
            fn base_mut(&mut self) -> &mut PerformanceTestBase;
            /// Builds the test object.
            fn set_up(&mut self);
            /// Destroys the test object.
            fn tear_down(&mut self);
            /// One run of iterations until stop_iteration().
            fn do_single_run(&mut self) -> Future<RunResult>;
        }

        // Helper for measuring time.
        // Each microbenchmark can either use the default behaviour which measures
        // only the start and stop time of the whole run or manually invoke
        // start_measuring_time() and stop_measuring_time() in order to measure
        // only parts of each iteration.
        /// The clock of the runs and of the measured parts of their iterations.
        ///
        /// The state is in `Cell`s: the measurement is reached through a shared reference from
        /// the run loop and from the test body (`start_measuring_time()`), on the reactor thread.
        pub struct TimeMeasurement {
            _run_start_time: Cell<Option<Instant>>,
            _start_time: Cell<Option<Instant>>,
            _total_time: Cell<Duration>,

            _start_stats: Cell<PerfStats>,
            _total_stats: Cell<PerfStats>,

            _instructions_retired_counter: Cell<*mut LinuxPerfEvent>,
            _cpu_cycles_retired_counter: Cell<*mut LinuxPerfEvent>,
        }

        impl TimeMeasurement {
            const fn new() -> Self {
                TimeMeasurement {
                    _run_start_time: Cell::new(None),
                    _start_time: Cell::new(None),
                    _total_time: Cell::new(Duration::ZERO),
                    _start_stats: Cell::new(PerfStats::new(0, 0, 0, 0)),
                    _total_stats: Cell::new(PerfStats::new(0, 0, 0, 0)),
                    _instructions_retired_counter: Cell::new(std::ptr::null_mut()),
                    _cpu_cycles_retired_counter: Cell::new(std::ptr::null_mut()),
                }
            }

            #[inline(always)]
            fn snapshot(&self) -> PerfStats {
                // SAFETY: the counters are null outside a run, and during a run they are the
                // test's, valid until stop_run() (the contract of start_run()); nothing else
                // uses them while a snapshot reads them.
                unsafe {
                    PerfStats::snapshot(
                        self._instructions_retired_counter.get().as_mut(),
                        self._cpu_cycles_retired_counter.get().as_mut(),
                    )
                }
            }

            /// Starts a run, counting with the given counters.
            ///
            /// # Safety
            /// The counters stay valid, and are not used elsewhere while a measurement reads
            /// them, until `stop_run()`.
            #[inline(always)]
            pub unsafe fn start_run(
                &self,
                instructions_retired_counter: *mut LinuxPerfEvent,
                cpu_cycles_retired_counter: *mut LinuxPerfEvent,
            ) {
                self._instructions_retired_counter.set(instructions_retired_counter);
                self._cpu_cycles_retired_counter.set(cpu_cycles_retired_counter);
                self._total_time.set(Duration::ZERO);
                self._total_stats.set(PerfStats::default());
                let t = Instant::now();
                self._run_start_time.set(Some(t));
                self._start_time.set(Some(t));
                self._start_stats.set(self.snapshot());
            }

            /// Ends a run.
            #[inline(always)]
            pub fn stop_run(&self) -> RunResult {
                let t = Instant::now();
                let mut ret = RunResult::default();
                if self._start_time.get() == self._run_start_time.get() {
                    ret.duration = self._start_time.get().map_or(Duration::ZERO, |s| t - s);
                    let stats = self.snapshot();
                    ret.stats = stats - self._start_stats.get();
                } else {
                    ret.duration = self._total_time.get();
                    ret.stats = self._total_stats.get();
                }
                self._instructions_retired_counter.set(std::ptr::null_mut());
                self._cpu_cycles_retired_counter.set(std::ptr::null_mut());
                ret
            }

            /// Starts the measured part of an iteration.
            #[inline(always)]
            pub fn start_iteration(&self) {
                self._start_time.set(Some(Instant::now()));
                self._start_stats.set(self.snapshot());
            }

            /// Ends the measured part of an iteration.
            #[inline(always)]
            pub fn stop_iteration(&self) {
                let t = Instant::now();
                if let Some(s) = self._start_time.get() {
                    self._total_time.set(self._total_time.get() + (t - s));
                }
                let stats = self.snapshot();
                self._total_stats.set(self._total_stats.get() + (stats - self._start_stats.get()));
            }
        }

        thread_local! {
            /// The time measurement of the running benchmark, used only by the reactor
            /// thread that runs the microbenchmarks; one per thread, without a destructor (nothing in it needs drop), so a const-initialized TLS slot.
            static MEASURE_TIME: TimeMeasurement = const { TimeMeasurement::new() };
        }

        /// The global time measurement.
        #[inline(always)]
        pub fn measure_time() -> &'static TimeMeasurement {
            MEASURE_TIME.with(|m| {
                let p: *const TimeMeasurement = m;
                // SAFETY: TimeMeasurement does not need drop, so the thread registers no
                // destructor and the slot lives until the thread ends; the reference stays on
                // the thread because TimeMeasurement is !Sync (Cell), so &TimeMeasurement is !Send.
                unsafe { &*p }
            })
        }

        /// What a test's `run()` returns: nothing, an iteration count, or a future of either.
        ///
        /// `IS_ASYNC` selects the measured loop of `do_single_run`: a synchronous test is
        /// measured with a plain loop (run once, count one iteration), without futures.
        /// `RETURNS_ITERATIONS` tells how an asynchronous test is awaited: a
        /// `Future<usize>` is awaited as it is (`iterations_future()`), a `Future<()>` only
        /// when it is not available (`unready()`).
        ///
        /// Neither future is kept in the coroutine frame unless it is awaited: a ready
        /// `Future<usize>` with no preemption pending is taken before the awaiter is built
        /// (the awaiter would take it in place), a `Future<()>` is awaited only when it is not
        /// available, and the value that carries either (a `Result`, an `Option`) is moved
        /// whole on every arm. A value matched with a binding that moves only its payload
        /// stays in the frame across the await: it was stored there each iteration and its
        /// future copied into the awaiter with a 16-byte load, which measurably slowed the
        /// loop.
        pub trait TestRet: Sized + 'static {
            /// Whether the body returns a future.
            const IS_ASYNC: bool;
            /// Whether an asynchronous body returns an iteration count.
            const RETURNS_ITERATIONS: bool;
            /// The iterations a synchronous call ran (`1` for `void`).
            fn sync_iterations(self) -> usize;
            /// A `future<>` that is not available, to be awaited; `None` when it is (the
            /// iteration is counted without the await machinery).
            fn unready(self) -> Option<Future<()>>;
            /// The `future<size_t>` of the iterations, to be awaited.
            fn iterations_future(self) -> Future<usize>;
        }

        /// The asynchronous members of a synchronous return type, or the members of the other
        /// kind of future: never called (do_single_run picks by `IS_ASYNC` and
        /// `RETURNS_ITERATIONS`).
        #[cold]
        #[inline(never)]
        fn not_this_kind() -> ! {
            unreachable!("a test result is used as another kind of result")
        }

        impl TestRet for () {
            const IS_ASYNC: bool = false;
            const RETURNS_ITERATIONS: bool = false;
            #[inline(always)]
            fn sync_iterations(self) -> usize {
                1
            }
            #[inline(always)]
            fn unready(self) -> Option<Future<()>> {
                not_this_kind()
            }
            #[inline(always)]
            fn iterations_future(self) -> Future<usize> {
                not_this_kind()
            }
        }

        impl TestRet for usize {
            const IS_ASYNC: bool = false;
            const RETURNS_ITERATIONS: bool = true;
            #[inline(always)]
            fn sync_iterations(self) -> usize {
                self
            }
            #[inline(always)]
            fn unready(self) -> Option<Future<()>> {
                not_this_kind()
            }
            #[inline(always)]
            fn iterations_future(self) -> Future<usize> {
                not_this_kind()
            }
        }

        impl TestRet for Future<()> {
            const IS_ASYNC: bool = true;
            const RETURNS_ITERATIONS: bool = false;
            #[inline(always)]
            fn sync_iterations(self) -> usize {
                not_this_kind()
            }
            #[inline(always)]
            fn unready(self) -> Option<Future<()>> {
                // The available() check is functionally redundant, but is significantly faster
                // than invoking the await machinery on a future-returning function.
                if !self.available() {
                    return Some(self);
                }
                None
            }
            #[inline(always)]
            fn iterations_future(self) -> Future<usize> {
                not_this_kind()
            }
        }

        impl TestRet for Future<usize> {
            const IS_ASYNC: bool = true;
            const RETURNS_ITERATIONS: bool = true;
            #[inline(always)]
            fn sync_iterations(self) -> usize {
                not_this_kind()
            }
            #[inline(always)]
            fn unready(self) -> Option<Future<()>> {
                not_this_kind()
            }
            /// Every iteration behaves as if it went through the awaiter, whose readiness
            /// test is `available() && !need_preempt()`: at the end of each task quota the
            /// coroutine yields and the reactor runs. `do_single_run` makes that test first
            /// (`ready_iterations`) and awaits the future otherwise: the same outcome, and
            /// the awaiter yields when the task quota is over.
            #[inline(always)]
            fn iterations_future(self) -> Future<usize> {
                self
            }
        }

        /// The future of a `Some`, taking the whole `Option` (see do_single_run).
        #[inline(always)]
        fn some_future(pending: Option<Future<()>>) -> Future<()> {
            match pending {
                Some(f) => f,
                None => not_this_kind(),
            }
        }

        /// What awaiting `f` gives at once (an available future with no preemption pending:
        /// the awaiter's `await_ready()`), or `f` to be awaited.
        #[inline(always)]
        fn ready_iterations(
            f: Future<usize>,
        ) -> Result<Result<usize, ExceptionPtr>, Future<usize>> {
            if f.available() && !need_preempt() {
                return Ok(f.get());
            }
            Err(f)
        }

        /// The result of [`ready_iterations`] when it is ready, taking the whole value.
        #[inline(always)]
        fn ready_value(
            r: Result<Result<usize, ExceptionPtr>, Future<usize>>,
        ) -> Result<usize, ExceptionPtr> {
            match r {
                Ok(v) => v,
                Err(_) => not_this_kind(),
            }
        }

        /// The future of [`ready_iterations`] when it is not ready, taking the whole value.
        #[inline(always)]
        fn pending_iterations(
            r: Result<Result<usize, ExceptionPtr>, Future<usize>>,
        ) -> Future<usize> {
            match r {
                Err(f) => f,
                Ok(_) => not_this_kind(),
            }
        }

        /// A microbenchmark body: `run()` is one iteration (or returns how many it ran).
        pub trait PerfTestBody: Default + Send + 'static {
            /// The return type of `run()`.
            type Ret: TestRet;
            /// One call of the body.
            fn run(&mut self) -> Self::Ret;
        }

        /// The measured loop of a synchronous test: until the run should stop, call the
        /// body and count one iteration (or the count the body returns).
        ///
        /// It is a function of its own so that the run state and the test come in as two
        /// `&mut` arguments, which alias nothing else. Inside the coroutine the loop reached
        /// both through the test pointer kept in the coroutine's frame, and since the counter
        /// is stored through that pointer, the compiler reloaded the pointer and the counter
        /// after every iteration. Not inlined, so the arguments keep that guarantee; the call
        /// is made once per run.
        #[inline(never)]
        fn run_synchronous<Test: PerfTestBody>(base: &mut PerformanceTestBase, test: &mut Test) {
            while !base.stop_iteration() {
                let n = test.run().sync_iterations();
                base.next_iteration(n);
            }
        }

        /// The microbenchmark of `Test`.
        pub struct ConcretePerformanceTest<Test: PerfTestBody> {
            base: PerformanceTestBase,
            _test: Option<Test>,
        }

        impl<Test: PerfTestBody> ConcretePerformanceTest<Test> {
            /// The test `test_case` of group `test_group`.
            pub fn new(test_case: &str, test_group: &str) -> Self {
                ConcretePerformanceTest {
                    base: PerformanceTestBase::new(test_case, test_group),
                    _test: None,
                }
            }
        }

        impl<Test: PerfTestBody> PerformanceTest for ConcretePerformanceTest<Test> {
            fn base(&self) -> &PerformanceTestBase {
                &self.base
            }

            fn base_mut(&mut self) -> &mut PerformanceTestBase {
                &mut self.base
            }

            fn set_up(&mut self) {
                self._test = Some(Test::default());
            }

            fn tear_down(&mut self) {
                self._test = None;
            }

            fn do_single_run(&mut self) -> Future<RunResult> {
                let this: *mut ConcretePerformanceTest<Test> = self;
                make_coroutine(async move {
                    // SAFETY: the runner keeps the test alive until the run resolves.
                    let s = unsafe { &mut *this };
                    s.base._instructions_retired_counter.enable();
                    s.base._cpu_cycles_retired_counter.enable();
                    // SAFETY: the counters are fields of the test, which the runner keeps alive
                    // until the run resolves, and stop_run() below forgets them before the
                    // test disables them.
                    unsafe {
                        measure_time().start_run(
                            &mut s.base._instructions_retired_counter,
                            &mut s.base._cpu_cycles_retired_counter,
                        );
                    }
                    let mut failed = None;
                    // set_up() made the test; it is taken once, before the measured loop, so
                    // no iteration pays for the check.
                    if let Some(t) = s._test.as_mut() {
                        if <Test::Ret as TestRet>::IS_ASYNC {
                            while !s.base.stop_iteration() {
                                let iterations = if <Test::Ret as TestRet>::RETURNS_ITERATIONS {
                                    // The test of the awaiter's await_ready(), made before
                                    // the awaiter exists: a ready result with no preemption
                                    // pending is taken as the awaiter would take it, without
                                    // building the awaiter in the frame; otherwise the
                                    // future is awaited (and the awaiter yields at the end
                                    // of the task quota). The whole Result
                                    // is moved on both arms, so it never lives in the frame.
                                    let r = ready_iterations(t.run().iterations_future());
                                    match r {
                                        Ok(_) => ready_value(r),
                                        Err(_) => pending_iterations(r).await,
                                    }
                                } else {
                                    // The whole Option is moved into the awaiter's future: a
                                    // binding that moves only its payload leaves the Option
                                    // in the coroutine frame across the await, stored there
                                    // each iteration and copied into the awaiter with a
                                    // 16-byte load.
                                    let pending = t.run().unready();
                                    match pending {
                                        // Moved on both arms, so no drop flag is kept.
                                        None => {
                                            drop(pending);
                                            Ok(1)
                                        }
                                        Some(_) => some_future(pending).await.map(|()| 1),
                                    }
                                };
                                match iterations {
                                    Ok(n) => s.base.next_iteration(n),
                                    Err(e) => {
                                        failed = Some(e);
                                        break;
                                    }
                                }
                            }
                        } else {
                            run_synchronous(&mut s.base, t);
                        }
                    }
                    // The run ends also when an iteration fails, so the measurement never keeps
                    // the counters past the run.
                    let ret = measure_time().stop_run();
                    if let Some(e) = failed {
                        s.base._instructions_retired_counter.disable();
                        s.base._cpu_cycles_retired_counter.disable();
                        return Err(e);
                    }
                    s.base._instructions_retired_counter.disable();
                    s.base._cpu_cycles_retired_counter.disable();
                    Ok(ret)
                })
            }
        }

        static ALL_TESTS: Mutex<Vec<Box<dyn PerformanceTest>>> = Mutex::new(Vec::new());

        /// The registered microbenchmarks.
        pub fn all_tests() -> std::sync::MutexGuard<'static, Vec<Box<dyn PerformanceTest>>> {
            match ALL_TESTS.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            }
        }

        /// Registers a microbenchmark.
        pub fn register_test(test: Box<dyn PerformanceTest>) {
            all_tests().push(test);
        }

        /// Registers the microbenchmark of `Test` under `test_group.test_case`.
        pub fn test_registrar<Test: PerfTestBody>(test_group: &str, test_case: &str) {
            register_test(Box::new(ConcretePerformanceTest::<Test>::new(test_case, test_group)));
        }
    }

    /// Starts the measured part of an iteration.
    #[inline(always)]
    pub fn start_measuring_time() {
        internal::measure_time().start_iteration();
    }

    /// Ends the measured part of an iteration.
    #[inline(always)]
    pub fn stop_measuring_time() {
        internal::measure_time().stop_iteration();
    }

    /// Keeps `v` from being optimized away.
    ///
    /// The goal is an empty asm statement that takes `v` as an input operand and declares
    /// no clobbers: `v` must be computed, but every other value the compiler holds stays
    /// where it is across the statement; ideally a value held in a register is used as is
    /// (a loop counter: nothing is stored) and a value in memory is referenced by its
    /// addressing mode (an element of a container: nothing is computed into a register).
    /// Rust's `asm!` has no memory operands, so:
    ///
    /// - an integer or `bool` (`T` one of the primitive integer types or `bool`: no padding,
    ///   every byte initialized) is passed by value, in a register, to an empty asm that
    ///   touches no memory (`nomem`). A value already in a register costs nothing, as `r`
    ///   does; a value in memory is loaded with the load's own addressing mode
    ///   (`ldr w, [base, idx, lsl #2]`), which takes the place of the address computation
    ///   the pointer form needs;
    /// - any other `T` is passed by address, in a register, to an empty asm that may read
    ///   memory but never writes it (`readonly`): the stores to `v` are done before it and
    ///   nothing is reloaded after it.
    ///
    /// Neither asm is removed (no `pure`). `std::hint::black_box` would not do: it may write
    /// any memory, so every value a loop keeps in memory (the fields of a container, the
    /// iteration counter of the run) is reloaded after each call, and it stores the reference
    /// itself to the stack to hand its address to the asm.
    #[inline(always)]
    pub fn do_not_optimize<T: ?Sized>(v: &T) {
        let p = std::ptr::from_ref(v);
        macro_rules! by_value {
            ($($t:ty as $u:ty),*) => {$(
                if scalar::is::<T, $t>() {
                    // SAFETY: `T` is `$t` (`scalar::is`), so `p` points to a live, aligned and
                    // initialized `$t`.
                    let x = unsafe { p.cast::<$t>().read() } as $u as u64;
                    // SAFETY: the asm is empty: it reads nothing, writes nothing, does not
                    // touch the stack or the flags, and only names the register that holds `x`.
                    unsafe {
                        std::arch::asm!("/* {0} */", in(reg) x, options(nomem, nostack, preserves_flags));
                    }
                    return;
                }
            )*};
        }
        // Zero-extended to 64 bits: free for a value loaded from memory or computed by a
        // 32-bit instruction, both of which clear the upper half of the register.
        by_value!(
            u32 as u32,
            i32 as u32,
            u64 as u64,
            i64 as u64,
            usize as u64,
            isize as u64,
            u8 as u8,
            i8 as u8,
            u16 as u16,
            i16 as u16,
            bool as u8
        );
        // SAFETY: the asm is empty: it reads nothing, writes nothing, does not touch the stack
        // or the flags, and only names the register that holds `p`.
        unsafe {
            std::arch::asm!("/* {0} */", in(reg) p.cast::<u8>(), options(readonly, nostack, preserves_flags));
        }
    }

    /// Whether a type, which may borrow (no `'static` bound, as `do_not_optimize` has none),
    /// is a given primitive type. Decided at compile time: the calls fold to constants.
    mod scalar {
        use std::any::TypeId;
        use std::marker::PhantomData;

        trait TypeIdOf {
            fn type_id_of(&self) -> TypeId
            where
                Self: 'static;
        }

        impl<T: ?Sized> TypeIdOf for PhantomData<T> {
            #[inline(always)]
            fn type_id_of(&self) -> TypeId
            where
                Self: 'static,
            {
                TypeId::of::<T>()
            }
        }

        /// The `TypeId` of `T` with its lifetimes taken as `'static`.
        #[inline(always)]
        fn type_id_erased<T: ?Sized>() -> TypeId {
            let marker = PhantomData::<T>;
            let object: &dyn TypeIdOf = &marker;
            // SAFETY: only the lifetime bound of the trait object changes (same layout). The
            // one method called through it computes `TypeId::of::<T>()` and touches no value
            // of `T`, so no reference outlives what it borrows; the result is the id of `T`
            // with every lifetime replaced by `'static`.
            let object: &(dyn TypeIdOf + 'static) = unsafe { std::mem::transmute(object) };
            object.type_id_of()
        }

        /// `T == S`, for an `S` without lifetimes: then the lifetime-erased id of `T` equals
        /// the id of `S` exactly when `T` is `S`.
        #[inline(always)]
        pub(super) fn is<T: ?Sized, S: 'static>() -> bool {
            type_id_erased::<T>() == TypeId::of::<S>()
        }
    }
}

// `perf_test!` and `perf_test_f!` accept both synchronous and asynchronous bodies: the former
// return `()`, the latter `Future<()>`. `perf_test_c!` runs a coroutine body, and
// `perf_test_cn!` a coroutine body that returns the number of inner loops.
//
// A test case may perform several operations in a single run, which is useful when one
// operation is very cheap; depending on how the test is written it then measures latency or
// throughput. Such a body returns `usize` (synchronous) or `Future<usize>` (asynchronous): the
// number of iterations done in that run.

/// `perf_test_f!(test_group, test_case, |s| -> Ret { body })`: a microbenchmark whose state is the
/// fixture `test_group` (a `Default` type) and whose body returns `()`, `usize`,
/// `Future<()>` or `Future<usize>`. It declares `fn register()` in a module named after
/// the test case; the runner calls the registrations before running.
#[macro_export]
macro_rules! perf_test_f {
    ($test_group:ident, $test_case:ident, |$s:ident| -> $ret:ty $body:block) => {
        #[allow(non_snake_case)]
        #[doc = concat!("The `", stringify!($test_group), ".", stringify!($test_case), "` microbenchmark and its registration.")]
        pub mod $test_case {
            use super::*;

            /// The body of the test, over its fixture.
            #[derive(Default)]
            pub struct Test(pub $test_group);

            impl $crate::engine::testing::perf_tests::perf_tests::internal::PerfTestBody for Test {
                type Ret = $ret;
                #[inline(always)]
                fn run(&mut self) -> $ret {
                    let $s = &mut self.0;
                    $body
                }
            }

            /// Registers the test.
            pub fn register() {
                $crate::engine::testing::perf_tests::perf_tests::internal::test_registrar::<Test>(
                    stringify!($test_group),
                    stringify!($test_case),
                );
            }
        }
    };
}

/// `perf_test!(test_group, test_case, -> Ret { body })`: a microbenchmark without a fixture.
#[macro_export]
macro_rules! perf_test {
    ($test_group:ident, $test_case:ident, -> $ret:ty $body:block) => {
        #[allow(non_snake_case)]
        #[doc = concat!("The `", stringify!($test_group), ".", stringify!($test_case), "` microbenchmark and its registration.")]
        pub mod $test_case {
            use super::*;

            /// The body of the test.
            #[derive(Default)]
            pub struct Test;

            impl $crate::engine::testing::perf_tests::perf_tests::internal::PerfTestBody for Test {
                type Ret = $ret;
                #[inline(always)]
                fn run(&mut self) -> $ret $body
            }

            /// Registers the test.
            pub fn register() {
                $crate::engine::testing::perf_tests::perf_tests::internal::test_registrar::<Test>(
                    stringify!($test_group),
                    stringify!($test_case),
                );
            }
        }
    };
}

/// `perf_test_c!(test_group, test_case, |s| { body })`: a fixture test whose body is a
/// coroutine (an `async` block run as a future); `s` is a raw pointer to the fixture.
#[macro_export]
macro_rules! perf_test_c {
    ($test_group:ident, $test_case:ident, |$s:ident| $body:block) => {
        $crate::perf_test_f!($test_group, $test_case, |$s| -> $crate::engine::core::future::Future<()> {
            // The coroutine outlives the borrow, so it reaches the fixture through a pointer,
            // which stays valid while the benchmark runs.
            let $s: *mut $test_group = $s;
            $crate::engine::core::coroutine::make_coroutine(async move $body)
        });
    };
}

/// `perf_test_cn!(test_group, test_case, |s| { body })`: a fixture coroutine test returning
/// the number of inner loops.
#[macro_export]
macro_rules! perf_test_cn {
    ($test_group:ident, $test_case:ident, |$s:ident| $body:block) => {
        $crate::perf_test_f!($test_group, $test_case, |$s| -> $crate::engine::core::future::Future<usize> {
            // The coroutine outlives the borrow, so it reaches the fixture through a pointer,
            // which stays valid while the benchmark runs.
            let $s: *mut $test_group = $s;
            $crate::engine::core::coroutine::make_coroutine(async move $body)
        });
    };
}
