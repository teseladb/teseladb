//! The global test runner.
//!
//! [`testing::TestRunner`] starts one application (and its reactor) on a dedicated thread
//! the first time a test runs, and feeds it test bodies one at a time through an
//! [`Exchanger`](crate::engine::testing::exchanger::testing::Exchanger). Callers block in
//! [`run_sync`](testing::TestRunner::run_sync) until the body's future resolves. At start
//! the runner prints `random-seed=<n>` and seeds each shard's
//! [`local_random`](crate::engine::testing::random::testing::local_random) with
//! `n + shard id`. When the runner stops, abandoned failed futures make the run fail with
//! exit code 3 (unless `--fail-on-abandoned-failed-futures=false`).

/// The test runner.
pub mod testing {
    use std::ffi::c_int;
    use std::fmt;
    use std::hash::BuildHasher;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

    use crate::engine::core::app_template::AppTemplate;
    use crate::engine::core::future::{ExceptionPtr, Future, make_ready_future};
    use crate::engine::core::loop_::do_until;
    use crate::engine::core::posix::PosixThread;
    use crate::engine::core::program_options::bpo;
    use crate::engine::core::reactor::{engine, engine_is_ready};
    use crate::engine::core::smp::{Smp, this_shard_id};
    use crate::engine::testing::exchanger::testing::Exchanger;
    use crate::engine::testing::random::testing::seed_local_random_engine;
    use crate::engine_assert;

    /// A task for the reactor of the runner.
    pub type Task = Box<dyn FnOnce() -> Future<()> + Send>;

    /// Ends the loop of the runner.
    #[derive(Debug)]
    struct StopExecution;

    impl fmt::Display for StopExecution {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("stop_execution")
        }
    }

    impl std::error::Error for StopExecution {}

    /// The exit code of a run that leaves abandoned failed futures behind.
    const ABANDONED_FAILED_FUTURES_EXIT_CODE: i32 = 3;

    struct StartThreadArgs {
        args: Vec<String>,
    }

    struct Inner {
        _thread: Option<PosixThread>,
        _done: bool,
        _st_args: Option<StartThreadArgs>,
    }

    /// The exchanger of one `run_sync` call: the outcome of its task.
    type Outcome = Exchanger<Option<ExceptionPtr>>;

    /// Runs the tests on a reactor started in its own thread.
    pub struct TestRunner {
        _inner: Mutex<Inner>,
        _started: AtomicBool,
        _task: Arc<Exchanger<Task>>,
        // We guarantee that only one thread is running.
        // We only read this after that one thread is joined, so this is safe.
        _exit_code: Arc<Mutex<i32>>,
        /// Held for a whole `run_sync`: a test program's own `main` calls it from one thread,
        /// test after test, but the Rust harness calls it from several threads at once; this
        /// keeps the tests running one at a time on the reactor, and the exchangers with one
        /// giver and one taker each.
        _serial: Mutex<()>,
        /// The outcome exchanger of the `run_sync` in progress, so that a reactor thread that
        /// dies of a panic wakes its caller instead of leaving it waiting forever.
        _current: Arc<Mutex<Option<Arc<Outcome>>>>,
    }

    static INSTANCE: LazyLock<TestRunner> = LazyLock::new(|| TestRunner {
        _inner: Mutex::new(Inner { _thread: None, _done: false, _st_args: None }),
        _started: AtomicBool::new(false),
        _task: Arc::new(Exchanger::default()),
        _exit_code: Arc::new(Mutex::new(0)),
        _serial: Mutex::new(()),
        _current: Arc::new(Mutex::new(None)),
    });

    /// The exit code of a reactor thread that died of a panic that escaped a test.
    const PANICKED_REACTOR_EXIT_CODE: i32 = 1;

    /// The error given to waiting callers when the reactor thread dies of a panic; it
    /// carries the panic message.
    #[derive(Debug)]
    struct ReactorPanicked(String);

    impl fmt::Display for ReactorPanicked {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "the test reactor thread panicked: {}", self.0)
        }
    }

    impl std::error::Error for ReactorPanicked {}

    #[inline]
    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        match m.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// `sigset_t`
    #[repr(C)]
    struct SigsetT {
        #[cfg(target_os = "linux")]
        _bits: [u64; 16],
        #[cfg(not(target_os = "linux"))]
        _bits: u32,
    }

    #[cfg(target_os = "linux")]
    const SIG_BLOCK: c_int = 0;
    #[cfg(not(target_os = "linux"))]
    const SIG_BLOCK: c_int = 1;
    const SIGSEGV: c_int = 11;

    // The reactor declares these with the Linux `sigset_t`, which this one equals on Linux;
    // elsewhere it is the platform's own.
    #[cfg_attr(not(target_os = "linux"), allow(clashing_extern_declarations))]
    unsafe extern "C" {
        fn sigfillset(set: *mut SigsetT) -> c_int;
        fn sigdelset(set: *mut SigsetT, sig: c_int) -> c_int;
        fn pthread_sigmask(how: c_int, set: *const SigsetT, old: *mut SigsetT) -> c_int;
        fn abort() -> !;
    }

    /// Blocks every signal except `SIGSEGV` on the calling thread, so that only the
    /// reactor's own signal handling receives them.
    ///
    /// [`TestRunner::start`] calls it on the thread that runs the tests. Under the Rust
    /// harness the test bodies run on libtest's worker threads, and its `main` thread would
    /// otherwise take the signals tests send to the process (`kill(getpid(), SIGHUP)`)
    /// without a reactor; so the registration function of each test
    /// ([`engine_register_test!`](crate::engine_register_test), before `main`) also calls
    /// this on the main thread. Every thread created later, the reactor's included,
    /// inherits the mask.
    ///
    /// Aborts the process if the mask cannot be set.
    pub fn block_signals() {
        // Don't interfere with the reactor's signal handling.
        #[cfg(target_os = "linux")]
        let mut mask = SigsetT { _bits: [0; 16] };
        #[cfg(not(target_os = "linux"))]
        let mut mask = SigsetT { _bits: 0 };
        // SAFETY: `mask` is a live, writable `sigset_t`-sized buffer for both calls.
        unsafe {
            sigfillset(&mut mask);
            // The list of signals left unblocked (currently only SIGSEGV).
            #[allow(clippy::single_element_loop)]
            for sig in [SIGSEGV] {
                sigdelset(&mut mask, sig);
            }
        }
        // SAFETY: `mask` is initialized above; a null `old` pointer is allowed.
        let r = unsafe { pthread_sigmask(SIG_BLOCK, &mask, std::ptr::null_mut()) };
        if r != 0 {
            eprintln!("Error blocking signals. Aborting.");
            // SAFETY: aborting the process.
            unsafe { abort() };
        }
    }

    impl TestRunner {
        /// Prepares the runner with the command-line `args`; returns whether initialization
        /// was successful (currently always `true`).
        ///
        /// Only the first call has an effect: it blocks signals on the calling thread and
        /// records `args`. The reactor thread itself is started lazily by the first
        /// [`run_sync`](Self::run_sync).
        pub fn start(&self, args: &[String]) -> bool {
            if self
                ._started
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                return true;
            }
            block_signals();
            lock(&self._inner)._st_args = Some(StartThreadArgs { args: args.to_vec() });
            true
        }

        fn start_thread(&self, inner: &mut Inner, args: Vec<String>) -> bool {
            let init_outcome = Arc::new(Exchanger::<bool>::default());
            let outcome = init_outcome.clone();
            let task = self._task.clone();
            let task_on_panic = self._task.clone();
            let current = self._current.clone();
            let exit_code = self._exit_code.clone();
            let thread = PosixThread::new(Box::new(move || {
                // A panic that escapes a continuation of a test (an assertion after the
                // test's first suspension) unwinds the reactor. It is caught here, at the top of the thread, so that the
                // waiting test and every later one fail instead of waiting forever.
                let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut app =
                        AppTemplate::new(crate::engine::core::app_template::Config::default());
                    app.add_options()
                        .opt("random-seed", bpo::value::<u32>(), "Random number generator seed")
                        .opt(
                            "fail-on-abandoned-failed-futures",
                            bpo::value::<bool>().default_value(true),
                            "Fail the test if there are any abandoned failed futures",
                        );
                    let app_p: *mut AppTemplate = &mut *app;
                    let out = outcome.clone();
                    let code = app.run_with_exit_code(args, move || {
                    // Giving cannot fail: nothing interrupts the outcome exchanger.
                    let _ = out.give(true);
                    let init = move || {
                        // SAFETY: `app` lives on this thread's stack until
                        // `run_with_exit_code` returns, which is after this closure runs.
                        let conf_seed = unsafe { (*app_p).configuration() }.get::<u32>("random-seed").copied();
                        let seed = match conf_seed {
                            Some(s) => s,
                            None => std::collections::hash_map::RandomState::new().hash_one(0u8) as u32,
                        };
                        println!("random-seed={}", seed);
                        Smp::invoke_on_all(move || {
                            let local_seed = seed.wrapping_add(this_shard_id() as u32);
                            seed_local_random_engine(local_seed);
                        })
                    };
                    let task = task.clone();
                    let done = Arc::new(AtomicBool::new(false));
                    let d = done.clone();
                    init()
                        .then(move |()| {
                            do_until(
                                move || d.load(Ordering::Relaxed),
                                move || {
                                    // this will block the reactor briefly, but we don't care
                                    match task.take() {
                                        Ok(func) => func(),
                                        Err(e) if e.is::<StopExecution>() => {
                                            done.store(true, Ordering::Relaxed);
                                            make_ready_future(())
                                        }
                                        Err(e) => crate::engine::core::future::make_exception_future(e),
                                    }
                                },
                            )
                            .or_terminate()
                        })
                        .then(move |()| {
                            // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
                            if unsafe { (*engine()).abandoned_failed_futures() } != 0 {
                                eprintln!(
                                    "*** {} abandoned failed future(s) detected",
                                    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
                                    unsafe { (*engine()).abandoned_failed_futures() }
                                );
                                // SAFETY: `app` is still alive (see `init`).
                                if unsafe { (*app_p).configuration() }
                                    .get::<bool>("fail-on-abandoned-failed-futures")
                                    .is_some_and(|v| *v)
                                {
                                    eprintln!(
                                        "Failing the test because fail was requested by --fail-on-abandoned-failed-futures"
                                    );
                                    return crate::engine::core::future::make_ready_future(ABANDONED_FAILED_FUTURES_EXIT_CODE);
                                }
                            }
                            crate::engine::core::future::make_ready_future(0)
                        })
                });
                    *lock(&exit_code) = code;
                }));
                if let Err(p) = run {
                    let ex = ExceptionPtr::from_panic(p);
                    let what = ex.get().map(|x| x.what()).unwrap_or_default();
                    *lock(&exit_code) = PANICKED_REACTOR_EXIT_CODE;
                    task_on_panic.interrupt(ReactorPanicked(what.clone()));
                    if let Some(e) = lock(&current).take() {
                        e.interrupt(ReactorPanicked(what));
                    }
                }
                // Giving cannot fail: nothing interrupts the outcome exchanger.
                let _ = outcome.give(false);
            }));
            match thread {
                Ok(t) => inner._thread = Some(t),
                Err(e) => {
                    // The start is reported as failed, which run_sync() turns into a done
                    // runner.
                    eprintln!("Failed to start the test reactor thread: {}", e);
                    return false;
                }
            }
            init_outcome.take().unwrap_or(false)
        }

        /// Runs `task` on the reactor and blocks until its future resolves. Starts the
        /// reactor thread on the first call. Calls from several threads run one after
        /// another.
        ///
        /// If the reactor could not be started, returns `Ok(())` without running `task`;
        /// check [`reactor_failed`](Self::reactor_failed).
        ///
        /// # Errors
        ///
        /// Returns the error of the task's future, a panic of the task converted to an
        /// error, or an error if the reactor thread died while the task ran.
        pub fn run_sync(&self, task: Task) -> Result<(), ExceptionPtr> {
            let _serial = lock(&self._serial);
            let mut inner = lock(&self._inner);
            if let Some(sa) = inner._st_args.take()
                && !self.start_thread(&mut inner, sa.args)
            {
                // something bad happened when starting the reactor or app, and
                // the _thread has exited before taking any task. but we need to
                // move on. let's report this bad news with exit code
                inner._done = true;
            }
            if inner._done {
                // we failed to start the worker reactor, so we cannot send the task to
                // it.
                return Ok(());
            }
            drop(inner);
            let e = Arc::new(Outcome::default());
            *lock(&self._current) = Some(e.clone());
            let e2 = e.clone();
            let given = self._task.give(Box::new(move || {
                engine_assert!(engine_is_ready());
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(task)) {
                    Ok(f) => f.then_wrapped(move |f: Future<()>| {
                        // Giving cannot fail: only a dead reactor interrupts this exchanger,
                        // and this runs on the live one.
                        let _ = e2.give(f.get().err());
                    }),
                    Err(p) => {
                        // As above.
                        let _ = e2.give(Some(ExceptionPtr::from_panic(p)));
                        make_ready_future(())
                    }
                }
            }));
            let r = given.and_then(|()| e.take());
            *lock(&self._current) = None;
            match r? {
                Some(ex) => Err(ex),
                None => Ok(()),
            }
        }

        /// Whether the reactor failed to start, so that [`TestRunner::run_sync`] returned
        /// without running its task. A `#[test]` has no process exit code to report it
        /// through, so it checks this instead.
        pub fn reactor_failed(&self) -> bool {
            lock(&self._inner)._done
        }

        /// Stops the reactor thread (if it was started), joins it and returns the exit code
        /// of the application run on it (1 if it died of a panic, 3 for abandoned failed
        /// futures).
        pub fn finalize(&self) -> i32 {
            let mut inner = lock(&self._inner);
            if let Some(t) = inner._thread.take() {
                self._task.interrupt(StopExecution);
                t.join();
            }
            *lock(&self._exit_code)
        }
    }

    /// The runner of the tests of the process.
    pub fn global_test_runner() -> &'static TestRunner {
        &INSTANCE
    }
}
