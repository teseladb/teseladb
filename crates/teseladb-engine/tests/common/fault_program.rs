//! The driver of the fault-injection programs (`tests/cov4_err_*`): each case starts the
//! engine in a process of its own, with the seccomp filter of `syscall_faults.rs` installed for
//! the system calls it names before the engine starts, runs its body in an engine thread and
//! ends with an exit code the parent checks. A program with its own `main` (`harness = false`)
//! calls [`main`] with its cases; the parent runs this program again with `COV4_ERR_CASE`
//! naming each case.
#![allow(dead_code)]

use std::process::Command;
use std::time::{Duration, Instant};

use teseladb::engine::core::app_template::AppTemplate;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::thread::async_ as thread_async;

use super::syscall_faults;

const CASE_VAR: &str = "COV4_ERR_CASE";
const CASE_TIMEOUT: Duration = Duration::from_secs(120);

/// One case: the engine's options, the system calls to intercept, what to arm before the
/// start, the body (in an engine thread) and the exit codes the process may end with.
pub struct FaultCase {
    pub name: &'static str,
    pub args: &'static [&'static str],
    pub syscalls: &'static [i64],
    pub setup: fn(),
    pub body: fn() -> Result<(), ExceptionPtr>,
    pub exit_codes: &'static [i32],
}

pub const OK: &[i32] = &[0];

/// No setup.
pub fn nothing() {}

/// No body.
pub fn idle() -> Result<(), ExceptionPtr> {
    Ok(())
}

fn run_case(cases: &[FaultCase], name: &str) -> i32 {
    let Some(c) = cases.iter().find(|c| c.name == name) else {
        eprintln!("no case {name}");
        return 100;
    };
    if !c.syscalls.is_empty() {
        syscall_faults::install(c.syscalls);
    }
    (c.setup)();
    let mut args = vec![name.to_string()];
    args.extend(c.args.iter().map(|s| s.to_string()));
    let body = c.body;
    let mut app = AppTemplate::new(Default::default());
    app.run(args, move || thread_async(body))
}

fn run_all(cases: &[FaultCase]) -> i32 {
    let exe = std::env::current_exe().expect("this program");
    let only = std::env::args().nth(1);
    let mut failures = Vec::new();
    for c in cases {
        if only.as_deref().is_some_and(|o| !c.name.contains(o)) {
            continue;
        }
        let start = Instant::now();
        let mut child =
            Command::new(&exe).env(CASE_VAR, c.name).spawn().expect("the case's process");
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
        let code = status.and_then(|s| s.code());
        println!(
            "case {}: exit {:?} (expected one of {:?}) in {:?}",
            c.name,
            code,
            c.exit_codes,
            start.elapsed()
        );
        if !code.is_some_and(|code| c.exit_codes.contains(&code)) {
            failures.push(c.name);
        }
    }
    if failures.is_empty() {
        0
    } else {
        eprintln!("cases with an unexpected exit: {failures:?}");
        1
    }
}

/// The program's `main`: the parent runs every case (or those whose name contains the first
/// argument), a child the case it is named.
pub fn main(cases: Vec<FaultCase>) {
    let code = match std::env::var(CASE_VAR) {
        Ok(name) => run_case(&cases, &name),
        Err(_) => run_all(&cases),
    };
    std::process::exit(code);
}
