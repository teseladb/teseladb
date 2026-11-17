#![cfg(target_os = "linux")]
//! Shards that sleep between messages keep waking each other on every backend.
//!
//! Three shards with no idle polling (`--idle-poll-time-us=0`, so a shard with nothing to do
//! goes to sleep at once). Shard 0 sends, many times in a row, a message to shard 1 that sends
//! one on to shard 2 and one to shard 2 that sends one on to shard 1, and waits for both: a
//! sleeping shard is often woken by two others at nearly the same time. Every round must
//! finish, within a bound far above the healthy run time.
//!
//! Each backend (io_uring, linux-aio, epoll) runs in a process of its own, chosen on the
//! command line, and a process that does not finish in time counts as a hang. A program with
//! its own `main` (`harness = false`): one engine per process, so each backend runs this
//! program again with `SMP_CROSS_WAKEUPS_BACKEND` naming it.

use std::process::Command;
use std::time::{Duration, Instant};

use teseladb::engine::core::app_template::AppTemplate;
use teseladb::engine::core::future::{ExceptionPtr, RuntimeError, make_ready_future};
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::thread::async_ as thread_async;

const BACKEND_VAR: &str = "SMP_CROSS_WAKEUPS_BACKEND";
const BACKENDS: &[&str] = &["io_uring", "linux-aio", "epoll"];
/// How long a backend's process may run before it counts as hung.
const CASE_TIMEOUT: Duration = Duration::from_secs(60);

const ROUNDS: u32 = 20_000;
const ROUNDS_BOUND: Duration = Duration::from_secs(30);

/// A message to shard `via` that forwards an empty message to shard `to`.
fn relay(via: u32, to: u32) -> teseladb::engine::core::future::Future<()> {
    Smp::submit_to(via, move || Smp::submit_to(to, || make_ready_future(())))
}

fn workload() -> Result<(), ExceptionPtr> {
    let start = Instant::now();
    for _ in 0..ROUNDS {
        let a = relay(1, 2);
        let b = relay(2, 1);
        a.get()?;
        b.get()?;
    }
    let elapsed = start.elapsed();
    println!("{ROUNDS} rounds in {elapsed:?}");
    if elapsed <= ROUNDS_BOUND {
        return Ok(());
    }
    Err(ExceptionPtr::new(RuntimeError::new(format!(
        "{ROUNDS} rounds took {elapsed:?}, more than {ROUNDS_BOUND:?}"
    ))))
}

/// Runs the workload on `backend` in this process and returns the engine's exit code.
fn run_backend(backend: &str) -> i32 {
    let args = vec![
        "smp_cross_wakeups_test".to_string(),
        format!("--reactor-backend={backend}"),
        "--smp=3".to_string(),
        "--idle-poll-time-us=0".to_string(),
        "--memory=1G".to_string(),
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
        eprintln!("backends whose shards stopped waking each other or that failed: {failures:?}");
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
