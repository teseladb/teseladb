//! The runner of the microbenchmarks: command-line configuration, the run loop
//! (`do_run()`, `run()`, `run_all()`), the result printers and `main()`. Each bench
//! includes this file with `#[path]` and calls [`main`] after registering its tests.

#![allow(dead_code)]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::File as StdFile;
use std::io::Write as _;
use std::sync::atomic::Ordering;
use std::time::Duration;

use teseladb::engine::core::app_template::AppTemplate;
use teseladb::engine::core::future::{ExceptionPtr, SystemError};
use teseladb::engine::core::program_options::bpo;
use teseladb::engine::core::reactor::yield_;
use teseladb::engine::core::relabel_config::regex::Regex;
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_;
use teseladb::engine::json::formatter::ToJson;
use teseladb::engine::testing::perf_tests::perf_tests::internal::{
    PerformanceTest, RunResult, all_tests,
};
use teseladb::engine::testing::random::testing::seed_local_random_engine;

// ---------------------------------------------------------------------------------------
// signal_timer
// ---------------------------------------------------------------------------------------

const SIGALRM: i32 = 14;
const SIGEV_SIGNAL: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const SA_SIGINFO: i32 = 4;

#[repr(C)]
#[derive(Default)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[repr(C)]
#[derive(Default)]
struct Itimerspec {
    it_interval: Timespec,
    it_value: Timespec,
}

/// `struct sigevent` of glibc (64 bytes).
#[repr(C)]
struct Sigevent {
    sigev_value: *mut c_void,
    sigev_signo: i32,
    sigev_notify: i32,
    _pad: [i32; 12],
}

/// The head of `siginfo_t` of glibc on x86-64 and aarch64, up to `si_value`.
#[repr(C)]
struct SiginfoHead {
    si_signo: i32,
    si_errno: i32,
    si_code: i32,
    _pad0: i32,
    _si_pid_or_tid: i32,
    _si_uid_or_overrun: i32,
    si_value: *mut c_void,
}

/// `struct sigaction` of glibc.
#[repr(C)]
struct Sigaction {
    sa_sigaction: usize,
    sa_mask: [u64; 16],
    sa_flags: i32,
    sa_restorer: usize,
}

unsafe extern "C" {
    fn timer_create(clockid: i32, sevp: *mut Sigevent, timerid: *mut *mut c_void) -> i32;
    fn timer_delete(timerid: *mut c_void) -> i32;
    fn timer_settime(
        timerid: *mut c_void,
        flags: i32,
        new_value: *const Itimerspec,
        old_value: *mut Itimerspec,
    ) -> i32;
    fn sigaction(signum: i32, act: *const Sigaction, oldact: *mut Sigaction) -> i32;
}

fn system_error(ret: i32) -> ExceptionPtr {
    ExceptionPtr::new(SystemError::from_errno(ret))
}

// We need to use signal-based timer instead of engine ones so that
// tests that do not suspend can be interrupted.
// This causes no overhead though since the timer is used only in a dry run.
struct SignalTimer {
    fn_: Box<dyn Fn()>,
    timer: *mut c_void,
}

impl SignalTimer {
    /// Boxed, because the signal handler finds the timer by its address.
    fn new(fn_: Box<dyn Fn()>) -> Result<Box<Self>, ExceptionPtr> {
        let mut t = Box::new(SignalTimer { fn_, timer: std::ptr::null_mut() });
        let mut se = Sigevent {
            sigev_value: std::ptr::null_mut(),
            sigev_signo: SIGALRM,
            sigev_notify: SIGEV_SIGNAL,
            _pad: [0; 12],
        };
        se.sigev_value = &mut *t as *mut SignalTimer as *mut c_void;
        // SAFETY: valid pointers to a sigevent and a timer id.
        let ret = unsafe { timer_create(CLOCK_MONOTONIC, &mut se, &mut t.timer) };
        if ret != 0 {
            return Err(system_error(ret));
        }
        Ok(t)
    }

    fn arm(&self, dt: Duration) -> Result<(), ExceptionPtr> {
        let sec = dt.as_secs() as i64;
        let nsec = i64::from(dt.subsec_nanos());
        let ts = Itimerspec {
            it_interval: Timespec::default(),
            it_value: Timespec { tv_sec: sec, tv_nsec: nsec },
        };
        // SAFETY: `timer` was created by timer_create.
        let ret = unsafe { timer_settime(self.timer, 0, &ts, std::ptr::null_mut()) };
        if ret != 0 {
            return Err(system_error(ret));
        }
        Ok(())
    }

    fn cancel(&self) -> Result<(), ExceptionPtr> {
        let ts = Itimerspec::default();
        // SAFETY: as above.
        let ret = unsafe { timer_settime(self.timer, 0, &ts, std::ptr::null_mut()) };
        if ret != 0 {
            return Err(system_error(ret));
        }
        Ok(())
    }

    fn init() -> Result<(), ExceptionPtr> {
        let sa = Sigaction {
            sa_sigaction: Self::signal_handler as extern "C" fn(i32, *mut SiginfoHead, *mut c_void)
                as usize,
            sa_mask: [0; 16],
            sa_flags: SA_SIGINFO,
            sa_restorer: 0,
        };
        // SAFETY: a valid sigaction for SIGALRM.
        let ret = unsafe { sigaction(SIGALRM, &sa, std::ptr::null_mut()) };
        if ret != 0 {
            return Err(system_error(ret));
        }
        Ok(())
    }

    extern "C" fn signal_handler(_: i32, si: *mut SiginfoHead, _: *mut c_void) {
        // SAFETY: the kernel passes the sigev_value set in new(), the address of a live timer.
        let t = unsafe { &*((*si).si_value as *const SignalTimer) };
        (t.fn_)();
    }
}

impl Drop for SignalTimer {
    fn drop(&mut self) {
        // SAFETY: `timer` was created by timer_create.
        unsafe { timer_delete(self.timer) };
    }
}

// ---------------------------------------------------------------------------------------
// config, result, printers
// ---------------------------------------------------------------------------------------

trait ResultPrinter {
    fn print_configuration(&mut self, c: &ConfigData);
    fn print_result(&mut self, r: &PerfResult);
    fn name_column_length_mut(&mut self) -> &mut usize;
    fn name_column_length(&self) -> usize;
    fn update_name_column_length(&mut self, length: usize) {
        *self.name_column_length_mut() = length.max(1);
    }
}

const DEFAULT_NAME_COLUMN_LENGTH: usize = 40;

/// The fields of `config` but the printers (so printers can be handed the rest).
struct ConfigData {
    single_run_iterations: u64,
    single_run_duration: Duration,
    number_of_runs: u32,
    random_seed: u32,
}

struct Config {
    data: ConfigData,
    printers: Vec<Box<dyn ResultPrinter>>,
}

#[derive(Default, Clone)]
struct PerfResult {
    test_name: String,
    total_iterations: u64,
    runs: u32,
    median: f64,
    mad: f64,
    min: f64,
    max: f64,
    allocs: f64,
    tasks: f64,
    inst: f64,
    cycles: f64,
}

/// Formats a duration in nanoseconds with a unit suited to its size (`ns`, `us`, ...).
fn fmt_duration(value: f64) -> String {
    if value < 1_000.0 {
        format!("{:.3}ns", value)
    } else if value < 1_000_000.0 {
        // `us` rather than the micro sign, so the output stays ASCII.
        format!("{:.3}us", value / 1_000.0)
    } else if value < 1_000_000_000.0 {
        format!("{:.3}ms", value / 1_000_000.0)
    } else {
        format!("{:.3}s", value / 1_000_000_000.0)
    }
}

/// What a column's function returns: an integer, a double or a duration.
#[derive(Clone, Copy)]
enum ColumnValue {
    Integral(u64),
    Double(f64),
    Duration(f64),
}

/// A column object encapsulates the logic needed to print one
/// type of result value, usually as a column (or a json attribute).
///
/// This allows all printers to share a common view of the available
/// columns.
#[derive(Clone)]
struct Column {
    // column header
    header: &'static str,
    // width for the column in text output
    width: usize,
    // precision in case of double
    prec: usize,
    fn_: fn(&PerfResult) -> ColumnValue,
}

impl Column {
    const DEFAULT_WIDTH: usize = 11;

    const fn new(header: &'static str, prec: usize, fn_: fn(&PerfResult) -> ColumnValue) -> Self {
        Column { header, width: Self::DEFAULT_WIDTH, prec, fn_ }
    }

    /// used by stdout and md formats to print as text
    fn print_text(&self, out: &mut dyn std::io::Write, r: &PerfResult) {
        let _ = match (self.fn_)(r) {
            ColumnValue::Double(v) => write!(out, "{:>w$.p$}", v, w = self.width, p = self.prec),
            ColumnValue::Integral(v) => write!(out, "{:>w$}", v, w = self.width),
            ColumnValue::Duration(v) => write!(out, "{:>w$}", fmt_duration(v), w = self.width),
        };
    }

    /// used by json format to extract double result
    fn to_double(&self, r: &PerfResult) -> f64 {
        match (self.fn_)(r) {
            ColumnValue::Duration(v) | ColumnValue::Double(v) => v,
            ColumnValue::Integral(v) => v as f64,
        }
    }

    fn print_header(&self, out: &mut dyn std::io::Write, str: Option<&str>) {
        let _ = write!(out, "{:>w$}", str.unwrap_or(self.header), w = self.width);
    }
}

fn print_result_columns(
    out: &mut dyn std::io::Write,
    cols: &[Column],
    name_length: usize,
    r: &PerfResult,
    start_delim: &str,
    middle_delim: &str,
    end_delim: &str,
) {
    let _ = write!(out, "{}{:<w$}", start_delim, r.test_name, w = name_length);
    for c in cols {
        let _ = write!(out, "{}", middle_delim);
        c.print_text(out, r);
    }
    let _ = writeln!(out, "{}", end_delim);
}

// columns for json ouput
const JSON_COLUMNS: [Column; 8] = [
    Column::new("median", 0, |r| ColumnValue::Duration(r.median)),
    Column::new("mad", 0, |r| ColumnValue::Duration(r.mad)),
    Column::new("min", 0, |r| ColumnValue::Duration(r.min)),
    Column::new("max", 0, |r| ColumnValue::Duration(r.max)),
    Column::new("allocs", 3, |r| ColumnValue::Double(r.allocs)),
    Column::new("tasks", 3, |r| ColumnValue::Double(r.tasks)),
    Column::new("inst", 1, |r| ColumnValue::Double(r.inst)),
    Column::new("cycles", 1, |r| ColumnValue::Double(r.cycles)),
];

// columns for text output
fn text_columns() -> Vec<Column> {
    let mut ret = vec![Column::new("iterations", 0, |r| {
        ColumnValue::Integral(r.total_iterations / u64::from(r.runs))
    })];
    ret.extend(JSON_COLUMNS.iter().cloned());
    ret
}

struct StdoutPrinter {
    name_column_length: usize,
}

impl ResultPrinter for StdoutPrinter {
    fn print_configuration(&mut self, c: &ConfigData) {
        print!(
            "{:<25} {}\n{:<25} {}\n{:<25} {}\n{:<25} {}\n{:<25} {}\n\n",
            "single run iterations:",
            c.single_run_iterations,
            "single run duration:",
            fmt_duration(c.single_run_duration.as_nanos() as f64),
            "number of runs:",
            c.number_of_runs,
            "number of cores:",
            Smp::count(),
            "random seed:",
            c.random_seed
        );
        print!("{:<w$}", "test", w = self.name_column_length());
        let mut out = std::io::stdout();
        for c in text_columns() {
            // a middle delimiter between each column
            let _ = write!(out, " ");
            c.print_header(&mut out, None);
        }
        // end of line
        let _ = writeln!(out);
    }

    fn print_result(&mut self, r: &PerfResult) {
        print_result_columns(
            &mut std::io::stdout(),
            &text_columns(),
            self.name_column_length(),
            r,
            "",
            " ",
            "",
        );
    }

    fn name_column_length_mut(&mut self) -> &mut usize {
        &mut self.name_column_length
    }

    fn name_column_length(&self) -> usize {
        self.name_column_length
    }
}

struct JsonPrinter {
    name_column_length: usize,
    output_file: String,
    root: HashMap<Sstring, HashMap<Sstring, HashMap<Sstring, f64>>>,
}

impl JsonPrinter {
    fn new(file: &str) -> Self {
        JsonPrinter {
            name_column_length: DEFAULT_NAME_COLUMN_LENGTH,
            output_file: file.to_string(),
            root: HashMap::new(),
        }
    }
}

impl Drop for JsonPrinter {
    fn drop(&mut self) {
        if let Ok(mut out) = StdFile::create(&self.output_file)
            && let Ok(j) = self.root.to_json()
        {
            let _ = out.write_all(j.as_bytes());
        }
    }
}

impl ResultPrinter for JsonPrinter {
    fn print_configuration(&mut self, _: &ConfigData) {}

    fn print_result(&mut self, r: &PerfResult) {
        let result = self
            .root
            .entry(Sstring::from("results"))
            .or_default()
            .entry(Sstring::from(r.test_name.as_str()))
            .or_default();
        result.insert(Sstring::from("runs"), f64::from(r.runs));
        result.insert(Sstring::from("total_iterations"), r.total_iterations as f64);
        for c in &JSON_COLUMNS {
            result.insert(Sstring::from(c.header), c.to_double(r));
        }
    }

    fn name_column_length_mut(&mut self) -> &mut usize {
        &mut self.name_column_length
    }

    fn name_column_length(&self) -> usize {
        self.name_column_length
    }
}

struct MarkdownPrinter {
    name_column_length: usize,
    output: Box<dyn std::io::Write>,
}

impl MarkdownPrinter {
    fn new(filename: &str) -> Result<Self, ExceptionPtr> {
        let output: Box<dyn std::io::Write> = if filename == "-" {
            Box::new(std::io::stdout())
        } else {
            match StdFile::create(filename) {
                Ok(f) => Box::new(f),
                Err(_) => {
                    return Err(ExceptionPtr::new(
                        teseladb::engine::core::future::InvalidArgument(format!(
                            "unable to write to {}",
                            filename
                        )),
                    ));
                }
            }
        };
        Ok(MarkdownPrinter { name_column_length: DEFAULT_NAME_COLUMN_LENGTH, output })
    }

    fn print_header_row(&mut self, head_text: &str, body_text: Option<&str>) {
        // start delimeter, and the top-left cell
        let _ = write!(self.output, "| {:<w$}", head_text, w = self.name_column_length);
        // the header cells
        for c in text_columns() {
            // middle delimeter
            let _ = write!(self.output, " | ");
            // right align the text in result cells
            c.print_header(&mut self.output, body_text);
        }
        // end delimeter
        let _ = writeln!(self.output, " |");
    }
}

impl ResultPrinter for MarkdownPrinter {
    fn print_configuration(&mut self, _: &ConfigData) {
        // print the header row
        self.print_header_row("test", None);
        // then the divider row of all -
        self.print_header_row("-", Some("-:"));
    }

    fn print_result(&mut self, r: &PerfResult) {
        let n = self.name_column_length;
        print_result_columns(&mut self.output, &text_columns(), n, r, "| ", " | ", " |");
    }

    fn name_column_length_mut(&mut self) -> &mut usize {
        &mut self.name_column_length
    }

    fn name_column_length(&self) -> usize {
        self.name_column_length
    }
}

// ---------------------------------------------------------------------------------------
// performance_test::do_run, run; run_all
// ---------------------------------------------------------------------------------------

fn do_run(
    test: &mut (dyn PerformanceTest + 'static),
    conf: &mut Config,
) -> Result<(), ExceptionPtr> {
    let base = test.base_mut();
    base._max_single_run_iterations.store(conf.data.single_run_iterations, Ordering::Relaxed);
    if base._max_single_run_iterations.load(Ordering::Relaxed) == 0 {
        base._max_single_run_iterations.store(u64::MAX, Ordering::Relaxed);
    }

    let maxp: *const std::sync::atomic::AtomicU64 = &base._max_single_run_iterations;
    let tmr = SignalTimer::new(Box::new(move || {
        // SAFETY: the test outlives the timer, which is dropped at the end of do_run.
        unsafe { (*maxp).store(0, Ordering::Relaxed) };
    }))?;

    let testp: *mut dyn PerformanceTest = test;

    // dry run, estimate the number of iterations
    if !conf.data.single_run_duration.is_zero() {
        let dur = conf.data.single_run_duration;
        let tmrp: *const SignalTimer = &*tmr;
        // switch out of engine thread
        yield_()
            .then(move |()| {
                // SAFETY: the test and the timer live until this future is waited for below.
                unsafe {
                    if let Err(e) = (*tmrp).arm(dur) {
                        return teseladb::engine::core::future::make_exception_future(e);
                    }
                    (*testp).do_single_run().finally(move || {
                        let _ = (*tmrp).cancel();
                        let b = (*testp).base_mut();
                        b._max_single_run_iterations
                            .store(b._single_run_iterations, Ordering::Relaxed);
                    })
                }
            })
            .discard_result()
            .get()?;
    }

    let mut r = PerfResult::default();
    let mut results = vec![0f64; conf.data.number_of_runs as usize];
    let mut total_iterations: u64 = 0;
    // An index loop, which fills `results[i]`.
    #[allow(clippy::needless_range_loop)]
    for i in 0..conf.data.number_of_runs as usize {
        // switch out of engine thread
        let rr: RunResult = yield_()
            .then(move |()| {
                // SAFETY: as above.
                unsafe {
                    (*testp).base_mut()._single_run_iterations = 0;
                    (*testp).do_single_run()
                }
            })
            .get()?;
        // SAFETY: as above.
        let iters = unsafe { (*testp).base()._single_run_iterations };
        let dt = rr.duration;
        let ns = dt.as_nanos() as f64;
        results[i] = ns / iters as f64;

        total_iterations += iters;

        r.allocs += rr.stats.allocations as f64 / iters as f64;
        r.tasks += rr.stats.tasks_executed as f64 / iters as f64;
        r.inst += rr.stats.instructions_retired as f64 / iters as f64;
        r.cycles += rr.stats.cpu_cycles_retired as f64 / iters as f64;
    }
    drop(tmr);

    // SAFETY: as above.
    r.test_name = unsafe { (*testp).base().name() };
    r.total_iterations = total_iterations;
    r.runs = conf.data.number_of_runs;

    let mid = (conf.data.number_of_runs / 2) as usize;

    results.sort_by(f64::total_cmp);
    r.median = results[mid];

    let mut diffs: Vec<f64> = results.iter().map(|x| (x - r.median).abs()).collect();
    diffs.sort_by(f64::total_cmp);
    r.mad = diffs[mid];

    r.min = results[0];
    r.max = results[results.len() - 1];

    let runs = f64::from(conf.data.number_of_runs);
    r.allocs /= runs;
    r.tasks /= runs;
    r.inst /= runs;
    r.cycles /= runs;

    for rp in &mut conf.printers {
        rp.print_result(&r);
    }
    Ok(())
}

fn run(test: &mut (dyn PerformanceTest + 'static), conf: &mut Config) -> Result<(), ExceptionPtr> {
    test.set_up();
    let res = do_run(test, conf);
    test.tear_down();
    res
}

fn run_all(tests: &[String], conf: &mut Config) -> Result<(), ExceptionPtr> {
    let regexes: Vec<Regex> = tests.iter().map(|t| Regex::new(t)).collect::<Result<_, _>>()?;
    let can_run =
        |name: &str| regexes.is_empty() || regexes.iter().any(|re| re.is_full_match(name));

    let mut all = all_tests();
    let mut max_name_column_length = 0usize;
    for test in all.iter().filter(|t| can_run(&t.base().name())) {
        max_name_column_length = max_name_column_length.max(test.base().name().len());
    }

    for rp in &mut conf.printers {
        rp.update_name_column_length(max_name_column_length);
        rp.print_configuration(&conf.data);
    }

    for test in all.iter_mut() {
        if can_run(&test.base().name()) {
            run(&mut **test, conf)?;
        }
    }
    Ok(())
}

/// The entry point of a bench binary: parses the options and runs the registered tests.
pub fn main() {
    let mut app = AppTemplate::new(Default::default());
    app.add_options()
        .opt(
            "iterations,i",
            bpo::value::<usize>().default_value(0),
            "number of iterations in a single run",
        )
        .opt(
            "duration,d",
            bpo::value::<f64>().default_value(1.0),
            "duration of a single run in seconds",
        )
        .opt("runs,r", bpo::value::<usize>().default_value(5), "number of runs")
        .opt("test,t", bpo::vector_value::<String>(), "tests to execute")
        .opt("random-seed,S", bpo::value::<u32>().default_value(0), "random number generator seed")
        .flag("no-stdout", "do not print to stdout")
        .opt("json-output", bpo::value::<String>(), "output json file")
        .opt("md-output", bpo::value::<String>(), "output markdown file")
        .flag("list", "list available tests");

    let appp: *mut AppTemplate = &mut *app;
    let args: Vec<String> = std::env::args().collect();
    let status = app.run(args, move || {
        async_(move || -> Result<(), ExceptionPtr> {
            // SAFETY: the app outlives its run().
            let cfg = unsafe { (*appp).configuration() };
            SignalTimer::init()?;
            let mut conf = Config {
                data: ConfigData {
                    single_run_iterations: *cfg.get::<usize>("iterations").expect("default") as u64,
                    single_run_duration: Duration::from_secs_f64(
                        *cfg.get::<f64>("duration").expect("default"),
                    ),
                    number_of_runs: *cfg.get::<usize>("runs").expect("default") as u32,
                    random_seed: *cfg.get::<u32>("random-seed").expect("default"),
                },
                printers: Vec::new(),
            };

            let mut tests_to_run: Vec<String> = Vec::new();
            if let Some(t) = cfg.get::<Vec<String>>("test") {
                tests_to_run = t.clone();
            }

            if cfg.find("list").is_some() {
                println!("available tests:");
                for t in all_tests().iter() {
                    println!("\t{}", t.base().name());
                }
                return Ok(());
            }

            if cfg.find("no-stdout").is_none() {
                conf.printers.push(Box::new(StdoutPrinter {
                    name_column_length: DEFAULT_NAME_COLUMN_LENGTH,
                }));
            }

            if let Some(f) = cfg.get::<String>("json-output") {
                conf.printers.push(Box::new(JsonPrinter::new(f)));
            }

            if let Some(f) = cfg.get::<String>("md-output") {
                conf.printers.push(Box::new(MarkdownPrinter::new(f)?));
            }

            if conf.data.random_seed == 0 {
                conf.data.random_seed = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos() ^ d.as_secs() as u32)
                    .unwrap_or(1);
            }
            let seed = conf.data.random_seed;
            Smp::invoke_on_all(move || {
                let local_seed = seed.wrapping_add(this_shard_id());
                seed_local_random_engine(local_seed);
            })
            .get()?;

            run_all(&tests_to_run, &mut conf)
        })
    });
    std::process::exit(status);
}
