#![cfg(target_os = "linux")]
//! Shards that sleep between messages keep waking each other on every backend, and a start that
//! asks for more shards than CPUs fails cleanly.
//!
//! Three shards with no idle polling (`--idle-poll-time-us=0`, so a shard with nothing to do
//! goes to sleep at once). Shard 0 sends, many times in a row, a message to shard 1 that sends
//! one on to shard 2 and one to shard 2 that sends one on to shard 1, and waits for both: a
//! sleeping shard is often woken by two others at nearly the same time. Every round must
//! finish, within a bound far above the healthy run time.
//!
//! Three shards need three CPUs: like the reference (resource.cc, allocate()), the engine
//! refuses to start more shards than the CPUs it may run on, with "insufficient processing
//! units". So each backend runs the race when this process may use three CPUs or more, and
//! otherwise must refuse the start with that error (exit 1, no panic). Whatever the CPUs, one
//! more start asks for one shard more than there are CPUs and must be refused the same way.
//!
//! Each case runs in a process of its own, chosen on the command line, and a process that does
//! not finish in time counts as a hang. A program with its own `main` (`harness = false`): one
//! engine per process, so each case runs this program again with `SMP_CROSS_WAKEUPS_BACKEND`
//! naming the backend and `SMP_CROSS_WAKEUPS_SHARDS` the shard count.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use teseladb::engine::core::app_template::AppTemplate;
use teseladb::engine::core::future::{ExceptionPtr, RuntimeError, make_ready_future};
use teseladb::engine::core::posix::get_current_cpuset;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::thread::async_ as thread_async;

const BACKEND_VAR: &str = "SMP_CROSS_WAKEUPS_BACKEND";
const SHARDS_VAR: &str = "SMP_CROSS_WAKEUPS_SHARDS";
const BACKENDS: &[&str] = &["io_uring", "linux-aio", "epoll"];
/// How long a case's process may run before it counts as hung.
const CASE_TIMEOUT: Duration = Duration::from_secs(60);
/// The shards of the race: shard 0 and the two that wake each other.
const RACE_SHARDS: usize = 3;
/// The exit code of a start that `smp::configure()` refuses (app_template).
const START_REFUSED: i32 = 1;

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

/// Runs the workload on `backend` with `shards` shards in this process and returns the
/// engine's exit code.
fn run_backend(backend: &str, shards: usize) -> i32 {
    let args = vec![
        "smp_cross_wakeups_test".to_string(),
        format!("--reactor-backend={backend}"),
        format!("--smp={shards}"),
        "--idle-poll-time-us=0".to_string(),
        "--memory=1G".to_string(),
    ];
    let mut app = AppTemplate::new(Default::default());
    app.run(args, || thread_async(workload))
}

/// How a case's process ended: its exit code (none if killed by a signal or by the timeout)
/// and what it wrote to stderr.
struct Outcome {
    code: Option<i32>,
    hung: bool,
    stderr: String,
}

/// Runs this program for `backend` with `shards` shards in a child process.
fn run_case(backend: &str, shards: usize) -> Outcome {
    let exe = std::env::current_exe().expect("this program");
    let start = Instant::now();
    let mut child = Command::new(&exe)
        .env(BACKEND_VAR, backend)
        .env(SHARDS_VAR, shards.to_string())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the case's process");
    let mut pipe = child.stderr.take().expect("the case's stderr");
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = pipe.read_to_string(&mut s);
        s
    });
    let status = loop {
        if let Some(status) = child.try_wait().expect("the case's status") {
            break Some(status);
        }
        if start.elapsed() > CASE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stderr = reader.join().unwrap_or_default();
    eprint!("{stderr}");
    Outcome { code: status.and_then(|s| s.code()), hung: status.is_none(), stderr }
}

/// Checks that a start with `shards` shards on `cpus` CPUs was refused with the reference's
/// error and exit code.
fn check_refused(what: &str, outcome: &Outcome, shards: usize, cpus: usize) -> bool {
    let expected = format!("insufficient processing units: needed {shards} available {cpus}");
    let ok = outcome.code == Some(START_REFUSED) && outcome.stderr.contains(&expected);
    if ok {
        println!("{what}: start refused as expected ({expected})");
    } else {
        println!(
            "{what}: expected exit {START_REFUSED} with \"{expected}\", got exit {:?}{}",
            outcome.code,
            if outcome.hung { " (hung, killed)" } else { "" }
        );
    }
    ok
}

/// Runs every case and checks its outcome.
fn run_all() -> i32 {
    let cpus = get_current_cpuset().len();
    println!("CPUs available to this process: {cpus}");
    let mut failures = Vec::new();
    for &backend in BACKENDS {
        let start = Instant::now();
        let outcome = run_case(backend, RACE_SHARDS);
        let what = format!("backend {backend}, {RACE_SHARDS} shards");
        let ok = if cpus >= RACE_SHARDS {
            match outcome.code {
                Some(0) => {
                    println!("{what}: ok in {:?}", start.elapsed());
                    true
                }
                _ if outcome.hung => {
                    println!("{what}: hung, killed after {CASE_TIMEOUT:?}");
                    false
                }
                code => {
                    println!("{what}: exit {code:?}");
                    false
                }
            }
        } else {
            check_refused(&what, &outcome, RACE_SHARDS, cpus)
        };
        if !ok {
            failures.push(what);
        }
    }
    let over = cpus + 1;
    let what = format!("backend io_uring, {over} shards on {cpus} CPUs");
    if !check_refused(&what, &run_case("io_uring", over), over, cpus) {
        failures.push(what);
    }
    if failures.is_empty() {
        0
    } else {
        eprintln!("cases that hung, failed or did not refuse the start: {failures:?}");
        1
    }
}

pub(crate) fn main() {
    let code = match std::env::var(BACKEND_VAR) {
        Ok(backend) => {
            let shards =
                std::env::var(SHARDS_VAR).ok().and_then(|s| s.parse().ok()).unwrap_or(RACE_SHARDS);
            run_backend(&backend, shards)
        }
        Err(_) => run_all(),
    };
    std::process::exit(code);
}
