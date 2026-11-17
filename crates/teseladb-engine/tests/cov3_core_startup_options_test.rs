//! Starts the engine with each group of logging and core options in a process of its own and
//! checks the exit code (as the original's app_template parses them): every log level,
//! timestamp style and output stream by name, logging to syslog, the memory diagnostics kinds,
//! and the values that must stop the start (unknown enum names, a malformed cpuset, an unknown
//! reactor backend, a hugepages directory that does not exist). A program with its own `main`
//! (`harness = false`): one engine per process, so each case runs this program again with
//! `COV3_STARTUP_CASE` naming it.
#![cfg(target_os = "linux")]

use std::process::Command;
use std::time::{Duration, Instant};

use teseladb::engine::core::app_template::AppTemplate;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::thread::async_ as thread_async;

const CASE_VAR: &str = "COV3_STARTUP_CASE";
const CASE_TIMEOUT: Duration = Duration::from_secs(120);

const OK: &[i32] = &[0];
const INVALID: &[i32] = &[1];

/// The cases: name, options, accepted exit codes.
fn cases() -> Vec<(&'static str, Vec<&'static str>, &'static [i32])> {
    vec![
        (
            "log_levels_and_styles",
            vec![
                "--default-log-level=trace",
                "--logger-log-level=engine=debug:io=warn",
                "--logger-stdout-timestamps=boot",
                "--logger-ostream-type=stdout",
                "--dump-memory-diagnostics-on-alloc-failure-kind=none",
            ],
            OK,
        ),
        (
            "quiet_logs_to_syslog",
            vec![
                "--default-log-level=error",
                "--logger-stdout-timestamps=none",
                "--logger-ostream-type=none",
                "--log-to-syslog=1",
                "--dump-memory-diagnostics-on-alloc-failure-kind=all",
            ],
            OK,
        ),
        (
            "warn_level",
            vec![
                "--default-log-level=warn",
                "--logger-stdout-timestamps=real",
            ],
            OK,
        ),
        (
            "unknown_log_level",
            vec!["--default-log-level=loud"],
            INVALID,
        ),
        (
            "unknown_timestamp_style",
            vec!["--logger-stdout-timestamps=sundial"],
            INVALID,
        ),
        (
            "unknown_ostream_type",
            vec!["--logger-ostream-type=printer"],
            INVALID,
        ),
        (
            "unknown_diagnostics_kind",
            vec!["--dump-memory-diagnostics-on-alloc-failure-kind=some"],
            INVALID,
        ),
        ("malformed_cpuset", vec!["--cpuset=zero-one"], INVALID),
        (
            "unknown_reactor_backend",
            vec!["--reactor-backend=carrier-pigeon"],
            INVALID,
        ),
        (
            "missing_hugepages_directory",
            vec!["--hugepages=/nonexistent/cov3"],
            &[1, 134],
        ),
        ("help_loggers", vec!["--help-loggers"], OK),
    ]
}

/// The application of every case that starts: a sleep on every shard's reactor.
fn workload() -> Result<(), ExceptionPtr> {
    sleep(Duration::from_millis(20)).get()?;
    Ok(())
}

fn run_case(name: &str) -> i32 {
    let Some((_, opts, _)) = cases().into_iter().find(|c| c.0 == name) else {
        eprintln!("no case {name}");
        return 100;
    };
    let mut args = vec!["cov3_core_startup_options_test".to_string()];
    args.extend(opts.iter().map(|s| s.to_string()));
    let mut app = AppTemplate::new(Default::default());
    app.run(args, || thread_async(workload))
}

fn run_all() -> i32 {
    let exe = std::env::current_exe().expect("this program");
    let mut failures = Vec::new();
    for (name, _, codes) in cases() {
        let start = Instant::now();
        let mut child = Command::new(&exe)
            .env(CASE_VAR, name)
            .spawn()
            .expect("the case's process");
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
        println!("case {name}: exit {code:?} (expected one of {codes:?})");
        if !code.is_some_and(|c| codes.contains(&c)) {
            failures.push(name);
        }
    }
    // The engine's own loggers are listed before anything logged (the original's are
    // registered before main).
    let out = Command::new(&exe)
        .env(CASE_VAR, "help_loggers")
        .output()
        .expect("help");
    let listed = String::from_utf8_lossy(&out.stdout).into_owned();
    for name in [
        "engine",
        "engine_memory",
        "io",
        "scollectd",
        "httpd",
        "http",
    ] {
        if !listed.lines().any(|l| l.trim() == name) {
            eprintln!("--help-loggers does not list {name}:\n{listed}");
            failures.push("help_loggers lists the engine's loggers");
        }
    }
    if failures.is_empty() {
        0
    } else {
        eprintln!("cases with an unexpected exit: {failures:?}");
        1
    }
}

pub(crate) fn main() {
    let code = match std::env::var(CASE_VAR) {
        Ok(name) => run_case(&name),
        Err(_) => run_all(),
    };
    std::process::exit(code);
}
