//! Logging.
//!
//! A [`Logger`] is a named, process-wide log source with its own level (atomic, so any
//! shard may change it). Loggers register in the global [`LoggerRegistry`] so that levels
//! can be set by name from the command line ([`log_cli`]) or at run time. Messages are
//! formatted lazily: the level check is inlined at the call site and nothing is formatted
//! for a disabled level. An enabled message is formatted into a per-thread 8 KiB buffer
//! ([`LogBuf`]) with a prefix of level,
//! timestamp, `[shard N]` and logger name, and written to standard output/error and/or
//! syslog.
//!
//! Output format: `LEVEL  %Y-%m-%d %T,%03d [shard N] logger - message`.
//!
//! A formatting failure never propagates: it is counted in `LOGGING_FAILURES` and
//! reported as a separate message. [`RateLimit`] limits a message to one per interval.

use std::cell::{Cell, UnsafeCell};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{c_char, c_int};
use std::fmt;
use std::io::Write as IoWrite;
use std::marker::PhantomPinned;
use std::panic::Location;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::engine::core::future::ExceptionPtr;
use crate::engine::core::lowres_clock::LowresClock;
use crate::engine::core::sstring::Sstring;
use crate::engine::util::log_impl::internal::{self, InserterIterator, LogBuf};
use crate::engine::util::program_options;

/// The level of a log message, and the threshold of a [`Logger`].
///
/// Levels are in increasing order of verbosity: a logger at `Debug` (3) also shows
/// `Error` (0), `Warn` (1) and `Info` (2).
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogLevel {
    #[default]
    /// Errors.
    Error,
    /// Warnings.
    Warn,
    /// Informational messages (the level of a new logger).
    Info,
    /// Debugging detail.
    Debug,
    /// Very verbose tracing.
    Trace,
}

impl LogLevel {
    fn from_u8(v: u8) -> LogLevel {
        match v {
            0 => LogLevel::Error,
            1 => LogLevel::Warn,
            2 => LogLevel::Info,
            3 => LogLevel::Debug,
            _ => LogLevel::Trace,
        }
    }
}

struct WrappedLogLevel {
    level: LogLevel,
}

static LOG_LEVEL_NAMES: LazyLock<BTreeMap<LogLevel, &'static str>> = LazyLock::new(|| {
    BTreeMap::from([
        (LogLevel::Trace, "trace"),
        (LogLevel::Debug, "debug"),
        (LogLevel::Info, "info"),
        (LogLevel::Warn, "warn"),
        (LogLevel::Error, "error"),
    ])
});

/// The number of log levels.
const NR_LEVELS: usize = LogLevel::Trace as usize + 1;
static WRAPPED_LOG_LEVEL_COLORED: AtomicBool = AtomicBool::new(true);

impl fmt::Display for WrappedLogLevel {
    // format specifier not supported
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        static TEXT: [&str; NR_LEVELS] = {
            let mut t = [""; NR_LEVELS];
            t[LogLevel::Debug as usize] = "DEBUG";
            t[LogLevel::Info as usize] = "INFO ";
            t[LogLevel::Trace as usize] = "TRACE";
            t[LogLevel::Warn as usize] = "WARN ";
            t[LogLevel::Error as usize] = "ERROR";
            t
        };
        let index = self.level as usize;
        let name = TEXT[index];
        // Foreground colors as ANSI escape codes.
        static STYLE: [&str; NR_LEVELS] = {
            let mut s = [""; NR_LEVELS];
            s[LogLevel::Debug as usize] = "\x1b[32m"; // green
            s[LogLevel::Info as usize] = "\x1b[37m"; // white
            s[LogLevel::Trace as usize] = "\x1b[34m"; // blue
            s[LogLevel::Warn as usize] = "\x1b[33m"; // yellow
            s[LogLevel::Error as usize] = "\x1b[31m"; // red
            s
        };
        if WRAPPED_LOG_LEVEL_COLORED.load(Ordering::Relaxed) {
            return write!(f, "{}{}\x1b[0m", STYLE[index], name);
        }
        write!(f, "{}", name)
    }
}

/// Prints the level name (`error`, `warn`, `info`, `debug`, `trace`).
impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(LOG_LEVEL_NAMES[self])
    }
}

/// Parses a level name as printed by `Display`.
impl std::str::FromStr for LogLevel {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for (k, v) in LOG_LEVEL_NAMES.iter() {
            if s == *v {
                return Ok(*k);
            }
        }
        Err(())
    }
}

/// The failure path of [`engine_assert!`](crate::engine_assert).
pub mod internal_assert {
    /// Prints the location and the failed expression to standard output, as
    /// `FILE:LINE: FUNC: Assertion ` followed by the quoted `msg` and ` failed.`, and
    /// aborts the process.
    pub fn assert_fail(msg: &str, file: &str, line: i32, func: &str) -> ! {
        println!("{}:{}: {}: Assertion `{}` failed.", file, line as u32, func, msg);
        std::process::abort();
    }
}
pub use internal_assert::assert_fail;

thread_local! {
    /// The number of log messages on this thread that failed to format.
    pub static LOGGING_FAILURES: Cell<u64> = const { Cell::new(0) };
}

/// The style of the timestamp in log messages written to the output stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LoggerTimestampStyle {
    #[default]
    /// No timestamp (the default value of the type).
    None,
    /// Time since the process started.
    Boot,
    /// Wall-clock date and time.
    Real,
}

/// The output stream log messages are written to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LoggerOstreamType {
    #[default]
    /// No output stream (the default value of the type).
    None = 0,
    /// Standard output.
    Cout = 1,
    /// Standard error.
    Cerr = 2,
}

/// Parses the value of a `--logger-stdout-timestamps` option (`none`, `boot` or `real`).
pub fn validate_logger_timestamp_style(
    v: &mut Option<LoggerTimestampStyle>,
    values: &[String],
) -> Result<(), program_options::ValidationError> {
    program_options::validators::check_first_occurrence(v.is_some())?;
    let s = program_options::validators::get_single_string(values)?;
    if s == "none" {
        *v = Some(LoggerTimestampStyle::None);
        return Ok(());
    } else if s == "boot" {
        *v = Some(LoggerTimestampStyle::Boot);
        return Ok(());
    } else if s == "real" {
        *v = Some(LoggerTimestampStyle::Real);
        return Ok(());
    }
    Err(program_options::ValidationError::InvalidOptionValue(s.to_string()))
}

impl fmt::Display for LoggerTimestampStyle {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoggerTimestampStyle::None => os.write_str("none"),
            LoggerTimestampStyle::Boot => os.write_str("boot"),
            LoggerTimestampStyle::Real => os.write_str("real"),
        }
    }
}

/// Parses the value of a `--logger-ostream-type` option (`none`, `stdout` or `stderr`).
pub fn validate_logger_ostream_type(
    v: &mut Option<LoggerOstreamType>,
    values: &[String],
) -> Result<(), program_options::ValidationError> {
    program_options::validators::check_first_occurrence(v.is_some())?;
    let s = program_options::validators::get_single_string(values)?;
    if s == "none" {
        *v = Some(LoggerOstreamType::None);
        return Ok(());
    } else if s == "stdout" {
        *v = Some(LoggerOstreamType::Cout);
        return Ok(());
    } else if s == "stderr" {
        *v = Some(LoggerOstreamType::Cerr);
        return Ok(());
    }
    Err(program_options::ValidationError::InvalidOptionValue(s.to_string()))
}

impl fmt::Display for LoggerOstreamType {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoggerOstreamType::None => os.write_str("none"),
            LoggerOstreamType::Cout => os.write_str("stdout"),
            LoggerOstreamType::Cerr => os.write_str("stderr"),
        }
    }
}

fn print_no_timestamp(it: InserterIterator) -> InserterIterator {
    it
}

fn print_boot_timestamp(it: InserterIterator) -> InserterIterator {
    let n = crate::engine::core::lowres_clock::steady_clock_now_ns() as u64 / 1000;
    internal::format_to(it, format_args!("{:10}.{:06}", n / 1000000, n % 1000000))
}

// `struct tm`, declared once for the crate's `strftime`.
use crate::engine::json::formatter::DateTime as Tm;

type TimeT = i64;

unsafe extern "C" {
    fn localtime_r(t: *const TimeT, result: *mut Tm) -> *mut Tm;
    fn strftime(s: *mut c_char, max: usize, format: *const c_char, tm: *const Tm) -> usize;
    fn syslog(priority: c_int, format: *const c_char, ...);
    fn isatty(fd: c_int) -> c_int;
}

fn print_real_timestamp(it: InserterIterator) -> InserterIterator {
    struct ASecond {
        t: TimeT,
        static_buf: [u8; 32], // big enough to hold '2023-01-14 15:06:33'
        len: usize,
    }
    thread_local! {
        static THIS_SECOND: UnsafeCell<ASecond> =
            const { UnsafeCell::new(ASecond { t: 0, static_buf: [0; 32], len: 0 }) };
    }
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let t = n.as_secs() as TimeT;
    THIS_SECOND.with(|cell| {
        // SAFETY: thread-local, not re-entered.
        let this_second = unsafe { &mut *cell.get() };
        if this_second.t != t {
            this_second.t = t;
            // SAFETY: Tm is plain data; localtime_r and strftime write into our buffers.
            unsafe {
                let mut tm: Tm = std::mem::zeroed();
                localtime_r(&t, &mut tm);
                this_second.len = strftime(
                    this_second.static_buf.as_mut_ptr() as *mut c_char,
                    this_second.static_buf.len(),
                    c"%Y-%m-%d %T".as_ptr(),
                    &tm,
                );
            }
        }
        let ms = n.subsec_millis();
        let view = std::str::from_utf8(&this_second.static_buf[..this_second.len]).unwrap_or("");
        internal::format_to(it, format_args!("{},{:03}", view, ms))
    })
}

static PRINT_TIMESTAMP: AtomicU8 = AtomicU8::new(0);

fn print_timestamp(it: InserterIterator) -> InserterIterator {
    match PRINT_TIMESTAMP.load(Ordering::Relaxed) {
        0 => print_no_timestamp(it),
        1 => print_boot_timestamp(it),
        _ => print_real_timestamp(it),
    }
}

/// The output target of the loggers.
pub enum Ostream {
    /// Standard output.
    Cout,
    /// Standard error.
    Cerr,
    /// Any other writer.
    Custom(Box<dyn IoWrite + Send>),
}

impl Ostream {
    fn write_and_flush(&mut self, s: &[u8]) {
        match self {
            Ostream::Cout => {
                let mut o = std::io::stdout().lock();
                let _ = o.write_all(s);
                let _ = o.flush();
            }
            Ostream::Cerr => {
                let mut o = std::io::stderr().lock();
                let _ = o.write_all(s);
                let _ = o.flush();
            }
            Ostream::Custom(w) => {
                let _ = w.write_all(s);
                let _ = w.flush();
            }
        }
    }
}

static LOGGER_OUT: LazyLock<Mutex<Ostream>> = LazyLock::new(|| Mutex::new(Ostream::Cerr));
static LOGGER_OSTREAM: AtomicBool = AtomicBool::new(true);
static LOGGER_SYSLOG: AtomicBool = AtomicBool::new(false);
static LOGGER_SHARD_FIELD_WIDTH: AtomicU32 = AtomicU32::new(1);

thread_local! {
    static SILENT: Cell<bool> = const { Cell::new(false) };
}

/// Writes the body of a log message directly into the log buffer.
pub trait LogWriter {
    /// Writes through `it` and returns the cursor.
    fn call<'a>(&mut self, it: InserterIterator<'a>) -> InserterIterator<'a>;
}

/// A [`LogWriter`] made from a closure.
pub struct LambdaLogWriter<F: FnMut(InserterIterator) -> InserterIterator> {
    _func: F,
}

impl<F: FnMut(InserterIterator) -> InserterIterator> LambdaLogWriter<F> {
    /// Wraps `func`.
    pub fn new(func: F) -> Self {
        LambdaLogWriter { _func: func }
    }
}

impl<F: FnMut(InserterIterator) -> InserterIterator> LogWriter for LambdaLogWriter<F> {
    fn call<'a>(&mut self, it: InserterIterator<'a>) -> InserterIterator<'a> {
        (self._func)(it)
    }
}

/// The format string of a log call and the caller's location, used to report a message
/// that failed to format.
#[derive(Clone, Copy)]
pub struct FormatInfo {
    /// The format string.
    pub format: &'static str,
    /// Where the log call is.
    pub loc: &'static Location<'static>,
}

/// The error of a malformed format string read at run time.
#[derive(Debug, Clone, Copy)]
pub struct FormatError(pub &'static str);

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for FormatError {}

/// One piece of a runtime format string: text to copy, or the index of an argument.
enum RuntimeFormatPiece<'a> {
    Literal(&'a str),
    Argument(usize),
}

/// Walks a format string read at run time (`{}`, `{N}`, `{{`, `}}`), folding `acc`
/// through `f` for each piece, and stops at the first error with its message.
fn parse_runtime_format<'a, A>(
    format: &'a str,
    nargs: usize,
    mut acc: A,
    mut f: impl FnMut(A, RuntimeFormatPiece<'a>) -> A,
) -> (A, Result<(), &'static str>) {
    let bytes = format.as_bytes();
    let (mut i, mut start) = (0usize, 0usize);
    let mut next_auto = 0usize;
    let (mut auto, mut manual) = (false, false);
    while i < bytes.len() {
        match bytes[i] {
            b'{' if bytes.get(i + 1) == Some(&b'{') => {
                acc = f(acc, RuntimeFormatPiece::Literal(&format[start..=i]));
                i += 2;
                start = i;
            }
            b'{' => {
                acc = f(acc, RuntimeFormatPiece::Literal(&format[start..i]));
                let Some(len) = bytes[i + 1..].iter().position(|&b| b == b'}') else {
                    return (acc, Err("invalid format string"));
                };
                let field = &format[i + 1..i + 1 + len];
                let id = field.split_once(':').map_or(field, |(id, _)| id);
                if field.len() != id.len() && field.len() != id.len() + 1 {
                    return (acc, Err("unsupported format specifier"));
                }
                let index = if id.is_empty() {
                    if manual {
                        return (
                            acc,
                            Err("cannot switch from manual to automatic argument indexing"),
                        );
                    }
                    auto = true;
                    next_auto += 1;
                    next_auto - 1
                } else {
                    if auto {
                        return (
                            acc,
                            Err("cannot switch from automatic to manual argument indexing"),
                        );
                    }
                    manual = true;
                    match id.parse::<usize>() {
                        Ok(n) => n,
                        Err(_) => return (acc, Err("invalid format string")),
                    }
                };
                if index >= nargs {
                    return (acc, Err("argument not found"));
                }
                acc = f(acc, RuntimeFormatPiece::Argument(index));
                i += len + 2;
                start = i;
            }
            b'}' if bytes.get(i + 1) == Some(&b'}') => {
                acc = f(acc, RuntimeFormatPiece::Literal(&format[start..=i]));
                i += 2;
                start = i;
            }
            b'}' => return (acc, Err("unmatched '}' in format string")),
            _ => i += 1,
        }
    }
    acc = f(acc, RuntimeFormatPiece::Literal(&format[start..]));
    (acc, Ok(()))
}

impl FormatInfo {
    /// The format string `format` at the caller's location.
    #[track_caller]
    pub fn new(format: &'static str) -> Self {
        FormatInfo { format, loc: Location::caller() }
    }

    /// No format string, at the caller's location.
    #[track_caller]
    pub fn empty() -> Self {
        FormatInfo { format: "", loc: Location::caller() }
    }
}

impl Default for FormatInfo {
    #[track_caller]
    fn default() -> Self {
        Self::empty()
    }
}

/// Alias of [`FormatInfo`].
pub type FormatInfoT = FormatInfo;

/// Apply a rate limit to log message(s)
///
/// Pass this to [`Logger::log_rate_limited`] to apply a rate limit to the message. The
/// limit applies to all the calls this same `RateLimit` is passed to, so it is usually a
/// thread-local created once with the interval (for example 10 seconds) and passed to the
/// log call of a request handler that should log at most once per interval.
///
/// The rate limit ensures that at most one message per interval will be
/// logged. If there were messages dropped due to rate-limiting the
/// following snippet will be prepended to the first non-dropped log
/// messages:
///
/// `(rate limiting dropped N similar messages)`
///
/// where N is the number of messages dropped.
pub struct RateLimit {
    _interval: Duration,
    _next: crate::engine::core::lowres_clock::LowresTimePoint,
    _dropped_messages: u64,
}

impl RateLimit {
    fn check(&mut self) -> bool {
        let now = LowresClock::now();
        if now < self._next {
            self._dropped_messages += 1;
            return false;
        }
        self._next = now + self._interval;
        true
    }
    fn has_dropped_messages(&self) -> bool {
        self._dropped_messages != 0
    }
    fn get_and_reset_dropped_messages(&mut self) -> u64 {
        std::mem::replace(&mut self._dropped_messages, 0)
    }
    /// A limit of one message per `interval`.
    pub fn new(interval: Duration) -> Self {
        RateLimit { _interval: interval, _next: LowresClock::now(), _dropped_messages: 0 }
    }
}

/// A named log source writing to the output stream and/or syslog.
///
/// Typically one static logger per module, e.g. `Logger::new("io")`, used as
/// `log.info(FormatInfo::new("..."), format_args!(...))` or through the `log_*!` macros.
/// The output format is (depending on level):
/// `DEBUG  %Y-%m-%d %T,%03d [shard 0] - "your msg"`.
///
/// Messages can be rate-limited, see [`RateLimit`].
///
/// The registry keeps a pointer to each logger, so a logger is built
/// pinned on the heap ([`Logger::new`] returns a `Pin<Box<Logger>>`): its address is
/// fixed before it registers, and moving the box moves no logger.
pub struct Logger {
    _name: Sstring,
    _level: AtomicU8,
    _pin: PhantomPinned,
}

// SAFETY: a logger is a global shared by every shard: its level is atomic and
// its name is written only while the logger is owned exclusively (construction, the failed
// registration in new() and moved_from(), through `Pin<Box>` with no other reference), and
// only read afterwards; the sstring's heap bytes are never mutated through a shared
// reference, so concurrent reads are sound.
unsafe impl Sync for Logger {}
// SAFETY: as above; the name's heap block is owned by the logger and freed by whichever
// thread drops it.
unsafe impl Send for Logger {}

// Mainly this protects us from re-entrance via malloc()'s
// oversized allocation warnings and failed allocation errors
struct Silencer;
impl Silencer {
    fn new() -> Self {
        SILENT.with(|s| s.set(true));
        Silencer
    }
}
impl Drop for Silencer {
    fn drop(&mut self) {
        SILENT.with(|s| s.set(false));
    }
}

thread_local! {
    static STATIC_LOG_BUF: UnsafeCell<[u8; 8192]> = const { UnsafeCell::new([0; 8192]) };
}

impl Logger {
    /// A logger named `name`, registered in the global registry.
    ///
    /// # Errors
    ///
    /// Fails if a logger with the same name is already registered.
    pub fn new(name: impl Into<Sstring>) -> Result<Pin<Box<Self>>, ExceptionPtr> {
        let mut l = Box::pin(Logger {
            _name: name.into(),
            _level: AtomicU8::new(LogLevel::Info as u8),
            _pin: PhantomPinned,
        });
        if let Err(e) = global_logger_registry().register_logger(&l) {
            // The name belongs to the logger already registered: forget it, so that this
            // one's destructor does not unregister the other.
            // SAFETY: the name is not structurally pinned.
            unsafe { l.as_mut().get_unchecked_mut()._name = Sstring::new() };
            return Err(e);
        }
        Ok(l)
    }

    /// A new logger that takes the place of `x` in the registry (`x` is consumed).
    pub fn moved_from(mut x: Pin<Box<Logger>>) -> Pin<Box<Self>> {
        let to = Box::pin(Logger {
            _name: x._name.clone(),
            _level: AtomicU8::new(x._level.load(Ordering::Relaxed)),
            _pin: PhantomPinned,
        });
        global_logger_registry().moved(&x, &to);
        // The moved-from logger has an empty name and does not unregister.
        // SAFETY: the name is not structurally pinned.
        unsafe { x.as_mut().get_unchecked_mut()._name = Sstring::new() };
        to
    }

    /// Whether this is shard 0 (`info0` logs only there).
    #[inline]
    pub fn is_shard_zero(&self) -> bool {
        crate::engine::core::smp::this_shard_id() == 0
    }

    /// Returns whether messages at `level` are enabled for this logger. Hot path: one
    /// relaxed atomic load.
    #[inline]
    pub fn is_enabled(&self, level: LogLevel) -> bool {
        (level as u8 <= self._level.load(Ordering::Relaxed)) && !SILENT.with(|s| s.get())
    }

    // A `dyn` writer rather than a boxed closure, which could allocate.
    fn do_log(&self, level: LogLevel, writer: &mut dyn LogWriter) {
        let is_ostream_enabled = LOGGER_OSTREAM.load(Ordering::Relaxed);
        let is_syslog_enabled = LOGGER_SYSLOG.load(Ordering::Relaxed);
        if !is_ostream_enabled && !is_syslog_enabled {
            return;
        }
        let name = &self._name;
        // A function rather than a closure, so that it is generic over the lifetime of the
        // buffer each call writes into.
        fn print_once<'a>(
            name: &Sstring,
            writer: &mut dyn LogWriter,
            mut it: InserterIterator<'a>,
        ) -> InserterIterator<'a> {
            if !crate::engine::core::reactor::local_engine().is_null() {
                it = internal::format_to(
                    it,
                    format_args!(
                        " [shard {:>width$}:{}]",
                        crate::engine::core::smp::this_shard_id(),
                        crate::engine::core::scheduling::current_scheduling_group().short_name(),
                        width = LOGGER_SHARD_FIELD_WIDTH.load(Ordering::Relaxed) as usize
                    ),
                );
            }
            it = internal::format_to(it, format_args!(" {} - ", name));
            writer.call(it)
        }
        // Mainly this protects us from re-entrance via malloc()'s
        // oversized allocation warnings and failed allocation errors
        let _be_silent = Silencer::new();

        STATIC_LOG_BUF.with(|static_log_buf| {
            let p = static_log_buf.get() as *mut u8;
            if is_ostream_enabled {
                // SAFETY: the thread-local buffer outlives `buf`; 8192 bytes.
                let mut buf = unsafe { LogBuf::with_external(p, 8192) };
                let mut it = buf.back_insert_begin();
                it = internal::format_to(it, format_args!("{} ", WrappedLogLevel { level }));
                it = print_timestamp(it);
                it = print_once(name, writer, it);
                it.push(b'\n');
                if let Ok(mut out) = LOGGER_OUT.lock() {
                    out.write_and_flush(buf.view());
                }
            }
            if is_syslog_enabled {
                // SAFETY: as above.
                let mut buf = unsafe { LogBuf::with_external(p, 8192) };
                let mut it = buf.back_insert_begin();
                it = print_once(name, writer, it);
                it.assign(b'\0');
                const LOG_ERR: c_int = 3;
                const LOG_WARNING: c_int = 4;
                const LOG_INFO: c_int = 6;
                const LOG_DEBUG: c_int = 7;
                let level_map: [c_int; 20] = {
                    let mut m = [0; 20];
                    m[LogLevel::Debug as usize] = LOG_DEBUG;
                    m[LogLevel::Info as usize] = LOG_INFO;
                    m[LogLevel::Trace as usize] = LOG_DEBUG; // no LOG_TRACE
                    m[LogLevel::Warn as usize] = LOG_WARNING;
                    m[LogLevel::Error as usize] = LOG_ERR;
                    m
                };
                // NOTE: syslog() can block, which will stall the reactor thread.
                //       this should be rare (will have to fill the pipe buffer
                //       before syslogd can clear it) but can happen.  If it does,
                //       we'll have to implement some internal buffering (which
                //       still means the problem can happen, just less frequently).
                // syslog() interprets % characters, so send msg as a parameter
                // SAFETY: buf.data() is NUL terminated just above.
                unsafe {
                    syslog(level_map[level as usize], c"%s".as_ptr(), buf.data() as *const c_char);
                }
            }
        });
    }

    fn failed_to_log(&self, ex: ExceptionPtr, fmt: &str, loc: &'static Location<'static>) {
        let mut writer = LambdaLogWriter::new(|mut it: InserterIterator| {
            it = internal::format_to(
                it,
                format_args!("{}:{} @{}: failed to log message", loc.file(), loc.line(), "?"),
            );
            if !fmt.is_empty() {
                it = internal::format_to(it, format_args!(": fmt='{}'", fmt));
            }
            internal::format_to(it, format_args!(": {}", DisplayExceptionPtr(Some(&ex))))
        });
        // A failure to format the failure message is counted in logging_failures.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.do_log(LogLevel::Error, &mut writer)
        }))
        .is_err()
        {
            LOGGING_FAILURES.with(|f| f.set(f.get() + 1));
        }
    }

    /// Runs the formatting of a log message; a panic while formatting is reported through
    /// failed_to_log.
    fn guarded(&self, fmt: FormatInfo, f: impl FnOnce()) {
        if let Err(p) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
            self.failed_to_log(ExceptionPtr::from_panic(p), fmt.format, fmt.loc);
        }
    }

    /// Logs `args` at `level` if the level is enabled; otherwise does nothing.
    ///
    /// `fmt` carries the format string and the caller's location, used to report a
    /// message that fails to format; `args` is the message (`format_args!`).
    ///
    /// The level check is inlined into the caller; only an enabled message calls into the
    /// formatting.
    #[inline]
    pub fn log(&self, level: LogLevel, fmt: FormatInfo, args: fmt::Arguments<'_>) {
        if self.is_enabled(level) {
            self.log_enabled(level, fmt, args);
        }
    }

    /// The body of [`log`](Self::log) once the level is known to be enabled.
    #[inline(never)]
    fn log_enabled(&self, level: LogLevel, fmt: FormatInfo, args: fmt::Arguments<'_>) {
        self.guarded(fmt, || {
            let mut writer =
                LambdaLogWriter::new(|it: InserterIterator| internal::format_to(it, args));
            self.do_log(level, &mut writer);
        });
    }

    /// Logs `args` at `level` if the level is enabled and `rl` allows it; otherwise does
    /// nothing.
    ///
    /// If there were messages dropped due to rate-limiting the following snippet
    /// will be prepended to the first non-dropped log messages:
    ///
    /// `(rate limiting dropped N similar messages)`
    ///
    /// where N is the number of messages dropped.
    ///
    /// `rl` is the rate limit to apply; `fmt` and `args` are as in [`log`](Self::log).
    #[inline]
    pub fn log_rate_limited(
        &self,
        level: LogLevel,
        rl: &mut RateLimit,
        fmt: FormatInfo,
        args: fmt::Arguments<'_>,
    ) {
        if self.is_enabled(level) && rl.check() {
            self.guarded(fmt, || {
                let mut writer = LambdaLogWriter::new(|mut it: InserterIterator| {
                    if rl.has_dropped_messages() {
                        it = internal::format_to(
                            it,
                            format_args!(
                                "(rate limiting dropped {} similar messages) ",
                                rl.get_and_reset_dropped_messages()
                            ),
                        );
                    }
                    internal::format_to(it, args)
                });
                self.do_log(level, &mut writer);
            });
        }
    }

    /// Logs at `level`, if enabled and allowed by `rl`, the body produced by `writer`.
    ///
    /// A low-level method for cases where avoiding allocations matters: `writer` gets an
    /// [`InserterIterator`] that writes into the log buffer directly, with no intermediate
    /// buffer. `fmt` only supplies the caller's location. See [`RateLimit`].
    #[inline]
    pub fn log_writer(
        &self,
        level: LogLevel,
        rl: &mut RateLimit,
        writer: &mut dyn LogWriter,
        fmt: FormatInfo,
    ) {
        if self.is_enabled(level) && rl.check() {
            let fmt0 = FormatInfo { format: "", loc: fmt.loc };
            self.guarded(fmt0, || {
                let mut writer_wrapper = LambdaLogWriter::new(|mut it: InserterIterator| {
                    if rl.has_dropped_messages() {
                        it = internal::format_to(
                            it,
                            format_args!(
                                "(rate limiting dropped {} similar messages) ",
                                rl.get_and_reset_dropped_messages()
                            ),
                        );
                    }
                    writer.call(it)
                });
                self.do_log(level, &mut writer_wrapper);
            });
        }
    }
    /// Logs at `level` with a format string read at run time, so a
    /// malformed one is reported through `failed_to_log` instead of being rejected at compile
    /// time. It takes `{}` and `{N}` replacement fields without a format spec, and `{{` / `}}`.
    pub fn log_runtime(&self, level: LogLevel, fmt: FormatInfo, args: &[&dyn fmt::Display]) {
        if !self.is_enabled(level) {
            return;
        }
        // A malformed format string is rejected before anything reaches the log buffer.
        if let ((), Err(what)) = parse_runtime_format(fmt.format, args.len(), (), |(), _| ()) {
            self.failed_to_log(ExceptionPtr::new(FormatError(what)), fmt.format, fmt.loc);
            return;
        }
        self.guarded(fmt, || {
            let mut writer = LambdaLogWriter::new(|it: InserterIterator| {
                // Checked above: every field has its argument, so the whole string is written.
                parse_runtime_format(fmt.format, args.len(), it, |it, piece| match piece {
                    RuntimeFormatPiece::Literal(s) => {
                        internal::format_to(it, format_args!("{}", s))
                    }
                    RuntimeFormatPiece::Argument(i) => {
                        internal::format_to(it, format_args!("{}", args[i]))
                    }
                })
                .0
            });
            self.do_log(level, &mut writer);
        });
    }

    /// Log with error tag:
    /// ERROR  %Y-%m-%d %T,%03d [shard 0] - "your msg" \n
    ///
    /// `fmt` and `args` are as in [`log`](Self::log).
    #[inline]
    pub fn error(&self, fmt: FormatInfo, args: fmt::Arguments<'_>) {
        self.log(LogLevel::Error, fmt, args);
    }

    /// [`Logger::error`] with a format string read at run time and
    /// no arguments (see [`Logger::log_runtime`]).
    pub fn error_runtime(&self, fmt: FormatInfo) {
        self.log_runtime(LogLevel::Error, fmt, &[]);
    }

    /// Log with warning tag:
    /// WARN  %Y-%m-%d %T,%03d [shard 0] - "your msg" \n
    ///
    /// `fmt` and `args` are as in [`log`](Self::log).
    #[inline]
    pub fn warn(&self, fmt: FormatInfo, args: fmt::Arguments<'_>) {
        self.log(LogLevel::Warn, fmt, args);
    }

    /// Log with info tag:
    /// INFO  %Y-%m-%d %T,%03d [shard 0] - "your msg" \n
    ///
    /// `fmt` and `args` are as in [`log`](Self::log).
    #[inline]
    pub fn info(&self, fmt: FormatInfo, args: fmt::Arguments<'_>) {
        self.log(LogLevel::Info, fmt, args);
    }

    /// Log with info tag on shard zero only:
    /// INFO  %Y-%m-%d %T,%03d [shard 0] - "your msg" \n
    ///
    /// `fmt` and `args` are as in [`log`](Self::log).
    #[inline]
    pub fn info0(&self, fmt: FormatInfo, args: fmt::Arguments<'_>) {
        if self.is_shard_zero() {
            self.log(LogLevel::Info, fmt, args);
        }
    }

    /// Log with debug tag:
    /// DEBUG  %Y-%m-%d %T,%03d [shard 0] - "your msg" \n
    ///
    /// `fmt` and `args` are as in [`log`](Self::log).
    #[inline]
    pub fn debug(&self, fmt: FormatInfo, args: fmt::Arguments<'_>) {
        self.log(LogLevel::Debug, fmt, args);
    }

    /// Log with trace tag:
    /// TRACE  %Y-%m-%d %T,%03d [shard 0] - "your msg" \n
    ///
    /// `fmt` and `args` are as in [`log`](Self::log).
    #[inline]
    pub fn trace(&self, fmt: FormatInfo, args: fmt::Arguments<'_>) {
        self.log(LogLevel::Trace, fmt, args);
    }

    /// The name of the logger (usually one logger per module).
    pub fn name(&self) -> &Sstring {
        &self._name
    }

    /// The current level of this logger.
    pub fn level(&self) -> LogLevel {
        LogLevel::from_u8(self._level.load(Ordering::Relaxed))
    }

    /// Sets the level of this logger to `level`.
    pub fn set_level(&self, level: LogLevel) {
        self._level.store(level as u8, Ordering::Relaxed);
    }

    /// Sets the output stream of all loggers; the default is standard error.
    pub fn set_ostream(out: Ostream) {
        if let Ok(mut o) = LOGGER_OUT.lock() {
            *o = out;
        }
    }

    /// Enables or disables output to the output stream (default: enabled).
    pub fn set_ostream_enabled(enabled: bool) {
        LOGGER_OSTREAM.store(enabled, Ordering::Relaxed);
    }

    /// Enables or disables output to syslog (default: disabled).
    ///
    /// NOTE: syslog() can block, which will stall the reactor thread.
    ///       this should be rare (will have to fill the pipe buffer
    ///       before syslogd can clear it) but can happen.
    pub fn set_syslog_enabled(enabled: bool) {
        LOGGER_SYSLOG.store(enabled, Ordering::Relaxed);
    }

    /// Set the width of shard id field in log messages
    ///
    /// The current shard id is printed as part of the prefix of log messages, like
    /// `[shard 42]`; `width` is the minimal width of the number.
    pub fn set_shard_field_width(width: u32) {
        LOGGER_SHARD_FIELD_WIDTH.store(width, Ordering::Relaxed);
    }

    /// Enables or disables the colored level tag in the output stream.
    pub fn set_with_color(enabled: bool) {
        WRAPPED_LOG_LEVEL_COLORED.store(enabled, Ordering::Relaxed);
    }
}

impl Drop for Logger {
    fn drop(&mut self) {
        if !self._name.empty() {
            global_logger_registry().unregister_logger(self);
        }
    }
}

/// A registered logger: pinned, so the pointer stays valid until it unregisters.
#[derive(Clone, Copy)]
struct LoggerPtr(*const Logger);

// SAFETY: the loggers are pinned and unregister before they are destroyed; the registry
// only reads their name and stores their level, which is atomic, under its mutex.
unsafe impl Send for LoggerPtr {}

impl LoggerPtr {
    #[inline]
    fn get(&self) -> &Logger {
        // SAFETY: as in the Send impl.
        unsafe { &*self.0 }
    }
}

/// Logs at a level with a literal format string and arguments, capturing the caller's
/// location: `engine_log!(logger, level, "fmt", args...)`.
#[macro_export]
macro_rules! engine_log {
    ($logger:expr, $level:expr, $fmt:literal $(, $args:expr)* $(,)?) => {
        $logger.log($level, $crate::engine::util::log::FormatInfo::new($fmt), format_args!($fmt $(, $args)*))
    };
}
/// [`engine_log!`](crate::engine_log) at the error level.
#[macro_export]
macro_rules! log_error { ($l:expr, $($t:tt)*) => { $crate::engine_log!($l, $crate::engine::util::log::LogLevel::Error, $($t)*) }; }
/// [`engine_log!`](crate::engine_log) at the warn level.
#[macro_export]
macro_rules! log_warn { ($l:expr, $($t:tt)*) => { $crate::engine_log!($l, $crate::engine::util::log::LogLevel::Warn, $($t)*) }; }
/// [`engine_log!`](crate::engine_log) at the info level.
#[macro_export]
macro_rules! log_info { ($l:expr, $($t:tt)*) => { $crate::engine_log!($l, $crate::engine::util::log::LogLevel::Info, $($t)*) }; }
/// [`engine_log!`](crate::engine_log) at the debug level.
#[macro_export]
macro_rules! log_debug { ($l:expr, $($t:tt)*) => { $crate::engine_log!($l, $crate::engine::util::log::LogLevel::Debug, $($t)*) }; }
/// [`engine_log!`](crate::engine_log) at the trace level.
#[macro_export]
macro_rules! log_trace { ($l:expr, $($t:tt)*) => { $crate::engine_log!($l, $crate::engine::util::log::LogLevel::Trace, $($t)*) }; }

/// The process-wide registry of loggers, by name.
///
/// Loggers are usually statics, one per module; they register on creation and unregister
/// on drop. All methods take the registry's mutex.
pub struct LoggerRegistry {
    _loggers: Mutex<HashMap<Sstring, LoggerPtr>>,
}

impl LoggerRegistry {
    fn new() -> Self {
        LoggerRegistry { _loggers: Mutex::new(HashMap::new()) }
    }

    /// loops through all registered loggers and sets the log level
    /// Note: this method locks
    ///
    /// `level` is the new level.
    pub fn set_all_loggers_level(&self, level: LogLevel) {
        let g = self._loggers.lock().unwrap_or_else(|e| e.into_inner());
        for l in g.values() {
            l.get()._level.store(level as u8, Ordering::Relaxed);
        }
    }

    /// Given a name for a logger returns the log_level enum
    /// Note: this method locks
    ///
    /// Returns `None` if no logger has that name.
    pub fn get_logger_level(&self, name: &Sstring) -> Option<LogLevel> {
        let g = self._loggers.lock().unwrap_or_else(|e| e.into_inner());
        g.get(name).map(|l| LogLevel::from_u8(l.get()._level.load(Ordering::Relaxed)))
    }

    /// Sets the log level for a given logger
    /// Note: this method locks
    ///
    /// # Errors
    ///
    /// Fails with an out-of-range error if no logger is named `name`.
    pub fn set_logger_level(&self, name: &Sstring, level: LogLevel) -> Result<(), ExceptionPtr> {
        let g = self._loggers.lock().unwrap_or_else(|e| e.into_inner());
        match g.get(name) {
            Some(l) => {
                l.get()._level.store(level as u8, Ordering::Relaxed);
                Ok(())
            }
            None => Err(ExceptionPtr::new(crate::engine::core::future::OutOfRange::new(
                "_Map_base::at",
            ))),
        }
    }

    /// Returns a list of registered loggers
    /// Note: this method locks
    ///
    /// The names of all registered loggers.
    pub fn get_all_logger_names(&self) -> Vec<Sstring> {
        let g = self._loggers.lock().unwrap_or_else(|e| e.into_inner());
        g.keys().cloned().collect()
    }

    /// Registers a logger with the static map
    /// Note: this method locks
    ///
    pub fn register_logger(&self, l: &Logger) -> Result<(), ExceptionPtr> {
        let mut g = self._loggers.lock().unwrap_or_else(|e| e.into_inner());
        if g.contains_key(l.name()) {
            return Err(ExceptionPtr::new(crate::engine::core::future::RuntimeError(format!(
                "Logger '{}' registered twice",
                l.name()
            ))));
        }
        g.insert(l.name().clone(), LoggerPtr(l));
        Ok(())
    }

    /// Unregisters a logger with the static map
    /// Note: this method locks
    ///
    pub fn unregister_logger(&self, l: &Logger) {
        let mut g = self._loggers.lock().unwrap_or_else(|e| e.into_inner());
        g.remove(l.name());
    }

    /// Swaps the logger given the from->name() in the static map
    /// Note: this method locks
    ///
    pub fn moved(&self, from: &Logger, to: &Logger) {
        let mut g = self._loggers.lock().unwrap_or_else(|e| e.into_inner());
        g.insert(from.name().clone(), LoggerPtr(to));
    }
}

/// The process-wide logger registry.
pub fn global_logger_registry() -> &'static LoggerRegistry {
    static G_REGISTRY: LazyLock<LoggerRegistry> = LazyLock::new(LoggerRegistry::new);
    &G_REGISTRY
}

/// The logging configuration applied by [`apply_logging_settings`].
pub struct LoggingSettings {
    /// Per-logger levels, overriding `default_level`.
    pub logger_levels: HashMap<Sstring, LogLevel>,
    /// The level of every logger not in `logger_levels`.
    pub default_level: LogLevel,
    /// Whether to write to the output stream.
    pub stdout_enabled: bool,
    /// Whether to write to syslog.
    pub syslog_enabled: bool,
    /// Whether to color the level tag.
    pub with_color: bool,
    /// The timestamp style of the output stream.
    pub stdout_timestamp_style: LoggerTimestampStyle,
    /// Which output stream to use.
    pub logger_ostream: LoggerOstreamType,
}

/// Shortcut for configuring the logging system all at once.
///
pub fn apply_logging_settings(s: &LoggingSettings) -> Result<(), ExceptionPtr> {
    global_logger_registry().set_all_loggers_level(s.default_level);
    for (k, v) in &s.logger_levels {
        if global_logger_registry().set_logger_level(k, *v).is_err() {
            return Err(ExceptionPtr::new(crate::engine::core::future::RuntimeError(format!(
                "Unknown logger '{}'. Use --help-loggers to list available loggers.",
                k
            ))));
        }
    }
    let logger_ostream = if s.stdout_enabled { s.logger_ostream } else { LoggerOstreamType::None };
    match logger_ostream {
        LoggerOstreamType::None => {
            Logger::set_ostream_enabled(false);
        }
        LoggerOstreamType::Cout => {
            Logger::set_ostream(Ostream::Cout);
            Logger::set_ostream_enabled(true);
        }
        LoggerOstreamType::Cerr => {
            Logger::set_ostream(Ostream::Cerr);
            Logger::set_ostream_enabled(true);
        }
    }
    Logger::set_syslog_enabled(s.syslog_enabled);
    Logger::set_with_color(s.with_color);

    match s.stdout_timestamp_style {
        LoggerTimestampStyle::None => PRINT_TIMESTAMP.store(0, Ordering::Relaxed),
        LoggerTimestampStyle::Boot => PRINT_TIMESTAMP.store(1, Ordering::Relaxed),
        LoggerTimestampStyle::Real => PRINT_TIMESTAMP.store(2, Ordering::Relaxed),
    }
    Ok(())
}

/// The name of the type `T`, as `std::any::type_name` gives it.
pub fn pretty_type_name<T: ?Sized>() -> Sstring {
    Sstring::from(std::any::type_name::<T>())
}

/// The name of `level` (`error`, `warn`, ...).
pub fn level_name(level: LogLevel) -> Sstring {
    Sstring::from(LOG_LEVEL_NAMES[&level])
}

/// A logger named after the type `T`.
pub struct LoggerFor<T: ?Sized> {
    /// The logger.
    pub logger: Pin<Box<Logger>>,
    _t: std::marker::PhantomData<T>,
}

impl<T: ?Sized> LoggerFor<T> {
    /// Creates and registers the logger.
    ///
    /// # Errors
    ///
    /// Fails if a logger with that name is already registered.
    pub fn new() -> Result<Self, ExceptionPtr> {
        Ok(LoggerFor {
            logger: Logger::new(pretty_type_name::<T>())?,
            _t: std::marker::PhantomData,
        })
    }
}

impl<T: ?Sized> std::ops::Deref for LoggerFor<T> {
    type Target = Logger;
    fn deref(&self) -> &Logger {
        &self.logger
    }
}
/// Command-line configuration of logging (see [`util::log_cli`](crate::engine::util::log_cli)).
pub mod log_cli {
    use super::*;
    use crate::engine::util::log_cli::{LogLevelMap, Options};

    /// Parses a level name.
    ///
    /// # Errors
    ///
    /// Fails with `Unknown log level '<s>'` for an unknown name.
    pub fn parse_log_level(s: &Sstring) -> Result<LogLevel, ExceptionPtr> {
        s.as_str().parse::<LogLevel>().map_err(|_| {
            ExceptionPtr::new(crate::engine::core::future::RuntimeError(format!(
                "Unknown log level '{}'",
                s
            )))
        })
    }

    /// Splits `v` on `:` into `KEY=VALUE` pairs and calls `consume_key_value` for each.
    ///
    /// # Errors
    ///
    /// Fails with a validation error for a pair without `=`.
    pub fn parse_map_associations(
        v: &str,
        mut consume_key_value: impl FnMut(String, String),
    ) -> Result<(), program_options::ValidationError> {
        // Split on ':'.
        // An empty input yields no tokens.
        if v.is_empty() {
            return Ok(());
        }
        for p in v.split(':') {
            let i = match p.find('=') {
                Some(i) => i,
                None => {
                    return Err(program_options::ValidationError::InvalidOptionValue(
                        p.to_string(),
                    ));
                }
            };
            let k = p[..i].to_string();
            let v = p[i + 1..].to_string();
            consume_key_value(k, v);
        }
        Ok(())
    }

    /// The description of the logging options, for `--help`.
    pub fn get_options_description() -> program_options::OptionsDescription {
        let mut descriptor =
            crate::engine::core::program_options::OptionsDescriptionBuildingVisitor::new();
        let mut opts = std::pin::pin!(Options::new());
        opts.as_mut().link(None);
        opts.describe(&mut descriptor);
        descriptor.get_options_description()
    }

    impl Default for Options {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Options {
        /// The logging options with their defaults, not yet linked;
        /// [`Options::link`] links them in place under a parent group.
        pub fn new() -> Self {
            let group = program_options::OptionGroup::new("Logging options");
            let default_log_level = program_options::Value::new(
                "default-log-level",
                Some(LogLevel::Info),
                "Default log level for log messages. Valid values are trace, debug, info, warn, error.",
            );
            let logger_log_level = program_options::Value::new(
                "logger-log-level",
                Some(LogLevelMap::new()),
                "Map of logger name to log level. The format is \"NAME0=LEVEL0[:NAME1=LEVEL1:...]\". \
                 Valid logger names can be queried with --help-loggers. \
                 Valid values for levels are trace, debug, info, warn, error. \
                 This option can be specified multiple times.",
            );
            let logger_stdout_timestamps = program_options::Value::new(
                "logger-stdout-timestamps",
                Some(LoggerTimestampStyle::Real),
                "Select timestamp style for stdout logs: none|boot|real",
            );
            let log_to_stdout = program_options::Value::new(
                "log-to-stdout",
                Some(true),
                "Send log output to output stream, as selected by --logger-ostream-type",
            );
            let logger_ostream_type = program_options::Value::new(
                "logger-ostream-type",
                Some(LoggerOstreamType::Cerr),
                "Send log output to: none|stdout|stderr",
            );
            let log_to_syslog = program_options::Value::new(
                "log-to-syslog",
                Some(false),
                "Send log output to syslog.",
            );
            // SAFETY: isatty only inspects the descriptor.
            let tty = unsafe { isatty(1) } != 0;
            let log_with_color = program_options::Value::new(
                "log-with-color",
                Some(tty),
                "Print colored tag prefix in log message written to ostream",
            );
            Options {
                group,
                default_log_level,
                logger_log_level,
                logger_stdout_timestamps,
                log_to_stdout,
                logger_ostream_type,
                log_to_syslog,
                log_with_color,
            }
        }

        /// Links the options, at their final place, under `parent_group`.
        pub fn link(
            self: Pin<&mut Self>,
            parent_group: Option<Pin<&mut program_options::OptionGroup>>,
        ) {
            crate::link_option_group!(
                self,
                parent_group,
                group,
                default_log_level,
                logger_log_level,
                logger_stdout_timestamps,
                log_to_stdout,
                logger_ostream_type,
                log_to_syslog,
                log_with_color,
            );
        }
    }

    /// Writes the sorted names of the registered loggers to `os`.
    pub fn print_available_loggers(os: &mut dyn fmt::Write) -> fmt::Result {
        let mut names = global_logger_registry().get_all_logger_names();
        // For quick searching by humans.
        names.sort();
        os.write_str("Available loggers:\n")?;
        for name in &names {
            writeln!(os, "    {}", name)?;
        }
        Ok(())
    }

    /// Builds the logging settings from parsed command-line variables.
    ///
    /// # Errors
    ///
    /// Fails if a value does not parse.
    pub fn extract_settings_from_vars(
        vars: &program_options::VariablesMap,
    ) -> Result<LoggingSettings, ExceptionPtr> {
        let mut opts = std::pin::pin!(Options::new());
        opts.as_mut().link(None);
        let mut visitor =
            crate::engine::core::program_options::VariablesMapExtractingVisitor::new(vars);
        // SAFETY: mutating the options in place does not move them.
        unsafe { opts.as_mut().get_unchecked_mut() }.mutate(&mut visitor)?;
        Ok(extract_settings(&opts))
    }

    /// Builds the logging settings from the option values.
    pub fn extract_settings(opts: &Options) -> LoggingSettings {
        LoggingSettings {
            logger_levels: opts.logger_log_level.get_value().clone(),
            default_level: *opts.default_log_level.get_value(),
            stdout_enabled: *opts.log_to_stdout.get_value(),
            syslog_enabled: *opts.log_to_syslog.get_value(),
            with_color: *opts.log_with_color.get_value(),
            stdout_timestamp_style: *opts.logger_stdout_timestamps.get_value(),
            logger_ostream: *opts.logger_ostream_type.get_value(),
        }
    }
}

/// Displays an error for logging: its type and message, plus details for nested and
/// system errors; `<no exception>` when empty.
pub struct DisplayExceptionPtr<'a>(pub Option<&'a ExceptionPtr>);

impl fmt::Display for DisplayExceptionPtr<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let eptr = match self.0 {
            None => {
                return out.write_str("<no exception>");
            }
            Some(e) => e,
        };
        let exc = eptr.get();
        match exc {
            Some(e) => out.write_str(e.type_name())?,
            // This case shouldn't happen...
            None => out.write_str("<unknown exception>")?,
        }
        // Print more information on some familiar exception types
        if let Some(ne) = eptr.downcast_ref::<crate::engine::core::future::NestedException>() {
            write!(
                out,
                ": {} (while cleaning up after {})",
                DisplayExceptionPtr(Some(&ne.inner)),
                DisplayExceptionPtr(Some(&ne.outer))
            )?;
        } else if let Some(e) = eptr.downcast_ref::<crate::engine::core::future::SystemError>() {
            // An error code prints as `category:value`.
            write!(out, " (error {}:{}, {})", e.category().name(), e.code(), e)?;
        } else if let Some(e) = eptr.downcast_ref::<std::io::Error>() {
            match e.raw_os_error() {
                Some(code) => write!(out, " (error system:{}, {})", code, e)?,
                None => write!(out, " ({})", e)?,
            }
        } else if let Some(e) = exc {
            write!(out, " ({})", e)?;
        } else {
            // no extra info
        }
        if let Some(nested) = eptr.nested_ptr() {
            write!(out, ": {}", DisplayExceptionPtr(Some(nested)))?;
        }
        Ok(())
    }
}

/// Displays the error as [`DisplayExceptionPtr`] does: a null pointer prints as
/// `<no exception>`.
impl fmt::Display for ExceptionPtr {
    #[inline]
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let eptr = if self.is_null() { None } else { Some(self) };
        fmt::Display::fmt(&DisplayExceptionPtr(eptr), out)
    }
}

/// Writes an error for logging: its type name and message.
pub fn format_exception(out: &mut dyn fmt::Write, type_name: &str, what: &str) -> fmt::Result {
    write!(out, "{} ({})", type_name, what)
}

/// Writes a system error for logging: its type, error code and message.
pub fn format_system_error(
    out: &mut dyn fmt::Write,
    type_name: &str,
    code: &dyn fmt::Display,
    what: &str,
) -> fmt::Result {
    write!(out, "{} (error {}, {})", type_name, code, what)
}
