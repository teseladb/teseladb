#![cfg(target_os = "linux")]
//! A reactor that never sleeps keeps firing its high resolution timers on every backend.
//!
//! A poller that is never passive and never has work (like the native stack's transmit
//! queue when it is empty) keeps the reactor polling without sleeping. Two workloads run on
//! such a reactor:
//!
//! - rounds of "arm a timer, cancel it, sleep": the cancelled timer leaves the timer
//!   descriptor armed for its deadline, so the descriptor expires once with no timer due
//!   before the sleep's own deadline;
//! - 1 500 sleeps of 1 ms in a row; the nearest-wakeup timers the idle loop arms and
//!   cancels on each iteration cause the same early expiry about once a second.
//!
//! Each workload must finish within its bound. Each backend (io_uring, linux-aio, epoll)
//! runs in a process of its own, chosen on the command line, and a process that does not
//! finish in time counts as a hang. A program with its own `main` (`harness = false`): one
//! engine per process, so each backend runs this program again with `HRTIMER_BUSY_REACTOR_BACKEND`
//! naming it.

use std::pin::pin;
use std::process::Command;
use std::time::{Duration, Instant};

use teseladb::engine::core::app_template::AppTemplate;
use teseladb::engine::core::future::{ExceptionPtr, RuntimeError};
use teseladb::engine::core::internal::poll::Poller;
use teseladb::engine::core::reactor::yield_;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::timer::{SteadyClockType, Timer};

const BACKEND_VAR: &str = "HRTIMER_BUSY_REACTOR_BACKEND";
const BACKENDS: &[&str] = &["io_uring", "linux-aio", "epoll"];
/// How long a backend's process may run before it counts as hung.
const CASE_TIMEOUT: Duration = Duration::from_secs(60);

const CANCELLED_ROUNDS: u32 = 10;
/// The cancelled timer's delay: shorter than the sleep, so it is the earliest timer.
const CANCELLED_DELAY: Duration = Duration::from_millis(2);
const CANCELLED_SLEEP: Duration = Duration::from_millis(20);
const CANCELLED_BOUND: Duration = Duration::from_secs(5);

const SHORT_SLEEPS: u32 = 1500;
const SHORT_SLEEP: Duration = Duration::from_millis(1);
const SHORT_BOUND: Duration = Duration::from_secs(15);

fn within(what: &str, elapsed: Duration, bound: Duration) -> Result<(), ExceptionPtr> {
    if elapsed <= bound {
        return Ok(());
    }
    Err(ExceptionPtr::new(RuntimeError::new(format!(
        "{what} took {elapsed:?}, more than {bound:?}"
    ))))
}

/// Arms a timer, cancels it while it is the earliest one, then sleeps past its deadline.
fn cancelled_timer_rounds() -> Result<(), ExceptionPtr> {
    let start = Instant::now();
    for round in 0..CANCELLED_ROUNDS {
        let mut decoy = pin!(Timer::<SteadyClockType>::new());
        decoy.as_mut().set_callback(|| {});
        decoy.as_mut().arm_in(CANCELLED_DELAY);
        decoy.as_mut().cancel();
        sleep(CANCELLED_SLEEP).get()?;
        println!("cancelled-timer round {round} done");
    }
    within("the cancelled-timer rounds", start.elapsed(), CANCELLED_BOUND)
}

/// Sleeps 1 ms, many times in a row.
fn short_sleeps() -> Result<(), ExceptionPtr> {
    let start = Instant::now();
    for _ in 0..SHORT_SLEEPS {
        sleep(SHORT_SLEEP).get()?;
    }
    println!("{SHORT_SLEEPS} short sleeps done");
    within("the short sleeps", start.elapsed(), SHORT_BOUND)
}

fn workload() -> Result<(), ExceptionPtr> {
    // Never passive, never any work: the reactor polls without sleeping.
    let _busy = Poller::simple(|| false);
    // The poller is registered by a task; let it run.
    yield_().get()?;
    cancelled_timer_rounds()?;
    short_sleeps()
}

/// Runs the workloads on `backend` in this process and returns the engine's exit code.
fn run_backend(backend: &str) -> i32 {
    let args = vec![
        "hrtimer_busy_reactor_test".to_string(),
        format!("--reactor-backend={backend}"),
        "--smp=1".to_string(),
    ];
    let mut app = AppTemplate::new(Default::default());
    app.run(args, || thread_async(workload))
}

/// Runs each backend in a child process and checks that it finishes in time with exit 0.
fn run_all() -> i32 {
    let exe = std::env::current_exe().expect("this program");
    let mut failures = Vec::new();
    for &backend in BACKENDS {
        let start = Instant::now();
        let mut child =
            Command::new(&exe).env(BACKEND_VAR, backend).spawn().expect("the backend's process");
        let status = loop {
            if let Some(status) = child.try_wait().expect("the backend's status") {
                break Some(status);
            }
            if start.elapsed() > CASE_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        match status.and_then(|s| s.code()) {
            Some(0) => println!("backend {backend}: ok in {:?}", start.elapsed()),
            Some(code) => {
                println!("backend {backend}: exit {code}");
                failures.push(backend);
            }
            None => {
                println!("backend {backend}: hung, killed after {CASE_TIMEOUT:?}");
                failures.push(backend);
            }
        }
    }
    if failures.is_empty() {
        0
    } else {
        eprintln!("backends whose timers stopped firing or that failed: {failures:?}");
        1
    }
}

pub(crate) fn main() {
    let code = match std::env::var(BACKEND_VAR) {
        Ok(backend) => run_backend(&backend),
        Err(_) => run_all(),
    };
    std::process::exit(code);
}
