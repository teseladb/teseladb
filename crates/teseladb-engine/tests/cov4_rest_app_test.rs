#![allow(unsafe_code)]
//! The application template beyond its plain start (the original's app_template.cc): the
//! `--help` text with the application's description, a positional option and a custom
//! configuration reader, the `io.conf` file of `$HOME/.config/engine`, a required option that
//! is missing (exit 1), a logging setting naming an unknown logger (exit 1), a program whose
//! future fails and an invalid collectd address (exit 1, the error printed). One engine per
//! process, so each case runs this program again with `COV4_APP_CASE` naming it
//! (`harness = false`).
#![cfg(target_os = "linux")]

use std::cell::Cell;
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant};

use teseladb::engine::core::app_template::{AppTemplate, Config, PositionalOption};
use teseladb::engine::core::future::{ExceptionPtr, RuntimeError, make_exception_future};
use teseladb::engine::core::program_options::bpo;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_ as thread_async;

const CASE_VAR: &str = "COV4_APP_CASE";
const CASE_TIMEOUT: Duration = Duration::from_secs(120);
const DESCRIPTION: &str = "cov4 application under test";

fn workload() -> Result<(), ExceptionPtr> {
    sleep(Duration::from_millis(5)).get()?;
    Ok(())
}

fn app_with_description() -> Box<AppTemplate> {
    AppTemplate::new(Config {
        name: Sstring::from("cov4_app"),
        description: Sstring::from(DESCRIPTION),
        ..Config::default()
    })
}

fn args(rest: &[&str]) -> Vec<String> {
    std::iter::once("cov4_rest_app_test").chain(rest.iter().copied()).map(String::from).collect()
}

/// Runs case `name` in this process and returns its exit code.
fn run_case(name: &str) -> i32 {
    match name {
        "help" => app_with_description().run(args(&["--help"]), || thread_async(workload)),
        "positional_and_reader" => {
            let mut app = app_with_description();
            app.add_positional_options(vec![PositionalOption {
                name: "input",
                value_semantic: Box::new(bpo::value::<Sstring>()),
                help: "an input name",
                max_count: 1,
            }]);
            app.get_options_description().add_options().flag("cov4-flag", "a flag");
            app.get_conf_file_options_description().add_options().opt(
                "cov4-conf",
                bpo::value::<u32>(),
                "a configuration value",
            );
            let read = Rc::new(Cell::new(false));
            let r = read.clone();
            app.set_configuration_reader(Box::new(move |_conf: &mut bpo::VariablesMap| {
                r.set(true);
                Ok(())
            }));
            let _ = app.options();
            let app_ptr: *mut AppTemplate = &mut *app;
            let code = app.run(args(&["--cov4-flag", "first.dat"]), move || {
                // SAFETY: the template outlives its run; only the parsed map is read.
                let conf = unsafe { (*app_ptr).configuration() };
                let ok = conf.count("cov4-flag") == 1
                    && conf.get::<Sstring>("input").is_some_and(|s| s.as_str() == "first.dat");
                thread_async(move || {
                    if !ok {
                        return Err(ExceptionPtr::new(RuntimeError::new("options not parsed")));
                    }
                    workload()
                })
            });
            if !read.get() {
                eprintln!("the configuration reader did not run");
                return 3;
            }
            code
        }
        "io_conf" => AppTemplate::new(Default::default()).run(args(&[]), || thread_async(workload)),
        "missing_required" => {
            let mut app = app_with_description();
            app.add_options().opt("cov4-must", bpo::value::<u32>().required(), "must be given");
            app.run(args(&[]), || thread_async(workload))
        }
        "unknown_logger" => AppTemplate::new(Default::default())
            .run(args(&["--logger-log-level=cov4_no_such_logger=debug"]), || {
                thread_async(workload)
            }),
        // The future of `run`'s function fails: the reactor exits on the unhandled error.
        "failing_future" => AppTemplate::new(Default::default()).run(args(&[]), || {
            make_exception_future::<()>(ExceptionPtr::new(RuntimeError::new("cov4 boom")))
        }),
        // An invalid collectd address fails the start after the reactors run: the error is
        // printed and the exit code is 1.
        "bad_collectd_address" => AppTemplate::new(Default::default())
            .run(args(&["--collectd=1", "--collectd-address=not-an-address:25826"]), || {
                thread_async(workload)
            }),
        _ => {
            eprintln!("no case {name}");
            100
        }
    }
}

/// A `$HOME` with the engine configuration of the current one (the backend the coverage runs
/// choose) and an `io.conf` with one more setting.
fn home_with_io_conf() -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!("cov4_rest_app_home_{}", std::process::id()));
    let conf = home.join(".config/engine");
    std::fs::create_dir_all(&conf).unwrap();
    if let Some(h) = std::env::var_os("HOME") {
        let engine_conf = std::path::Path::new(&h).join(".config/engine/engine.conf");
        if let Ok(s) = std::fs::read_to_string(engine_conf) {
            std::fs::write(conf.join("engine.conf"), s).unwrap();
        }
    }
    std::fs::write(conf.join("io.conf"), "blocked-reactor-notify-ms = 50\n").unwrap();
    home
}

fn run_all() -> i32 {
    let exe = std::env::current_exe().expect("this program");
    let home = home_with_io_conf();
    let cases: [(&str, &[i32], Option<&str>); 7] = [
        ("help", &[0], Some(DESCRIPTION)),
        ("positional_and_reader", &[0], None),
        ("io_conf", &[0], None),
        ("missing_required", &[1], Some("cov4-must")),
        ("unknown_logger", &[1], Some("logging configuration error")),
        ("failing_future", &[1], Some("Exiting on unhandled exception")),
        ("bad_collectd_address", &[1], Some("program failed with uncaught exception")),
    ];
    let mut failures = Vec::new();
    for (name, codes, text) in cases {
        let mut cmd = Command::new(&exe);
        cmd.env(CASE_VAR, name);
        if name == "io_conf" {
            cmd.env("HOME", &home);
        }
        let start = Instant::now();
        let mut child = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("the case's process");
        let status = loop {
            if let Some(status) = child.try_wait().expect("the case's status") {
                break Some(status);
            }
            if start.elapsed() > CASE_TIMEOUT {
                let _ = child.kill();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let out = child.wait_with_output().expect("output");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        let code = status.and_then(|s| s.code());
        println!("case {name}: exit {code:?} (expected one of {codes:?})");
        if !code.is_some_and(|c| codes.contains(&c)) {
            failures.push(format!("{name}: exit {code:?}"));
        }
        if let Some(t) = text
            && !stdout.contains(t)
        {
            failures.push(format!("{name}: {t:?} not in {stdout:?}"));
        }
    }
    let _ = std::fs::remove_dir_all(&home);
    if failures.is_empty() {
        0
    } else {
        eprintln!("cases with an unexpected result: {failures:?}");
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
