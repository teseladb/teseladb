//! Our own logger: one line per event, as text or JSON, with a level filter that is
//! checked before formatting anything (a disabled level allocates nothing).

use std::collections::VecDeque;
use std::fmt::{self, Display, Write as _};
use std::io::Write;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

/// Lines `getLog` keeps of each log, as RefDb's in-memory logs do.
pub const RAM_LOG_LINES: usize = 1024;

/// The last lines of a log and how many it wrote.
#[derive(Debug, Default)]
struct RamLog {
    total: i64,
    lines: VecDeque<String>,
}

impl RamLog {
    fn push(&mut self, line: &str) {
        self.total += 1;
        if self.lines.len() == RAM_LOG_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(line.trim_end_matches('\n').to_owned());
    }

    fn read(&self) -> (i64, Vec<String>) {
        (self.total, self.lines.iter().cloned().collect())
    }
}

/// The process's log (`getLog: "global"`).
static RECENT: Mutex<RamLog> = Mutex::new(RamLog { total: 0, lines: VecDeque::new() });
/// The warnings written before the node was ready (`getLog: "startupWarnings"`).
static STARTUP: Mutex<RamLog> = Mutex::new(RamLog { total: 0, lines: VecDeque::new() });
/// Whether the node finished starting.
static STARTED: AtomicBool = AtomicBool::new(false);

fn remember(level: Level, line: &str) {
    RECENT.lock().unwrap_or_else(PoisonError::into_inner).push(line);
    if level == Level::Warn && !STARTED.load(Ordering::Acquire) {
        STARTUP.lock().unwrap_or_else(PoisonError::into_inner).push(line);
    }
}

/// The lines the process logged, the last [`RAM_LOG_LINES`], and how many it ever logged.
pub fn recent() -> (i64, Vec<String>) {
    RECENT.lock().unwrap_or_else(PoisonError::into_inner).read()
}

/// The warnings logged while the node was starting.
pub fn startup_warnings() -> (i64, Vec<String>) {
    STARTUP.lock().unwrap_or_else(PoisonError::into_inner).read()
}

/// A warning a command made where no logger is at hand: RefDb's identifier, its message and
/// its attributes as relaxed Extended JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    /// RefDb's identifier of the line (`id`).
    pub id: i32,
    /// The message.
    pub msg: String,
    /// The attributes (`attr`).
    pub attr: String,
}

/// Lines waiting for the node's loop to write them, with their severity.
static PENDING: Mutex<Vec<(Level, Pending)>> = Mutex::new(Vec::new());

/// Keeps a warning for [`drain`].
pub fn defer(line: Pending) {
    defer_at(Level::Warn, line);
}

/// Keeps a line of severity `level` for [`drain`].
pub fn defer_at(level: Level, line: Pending) {
    PENDING.lock().unwrap_or_else(PoisonError::into_inner).push((level, line));
}

/// Writes the deferred lines with `log`, as `id` and `attr` fields.
pub fn drain(log: &Logger) {
    let lines = std::mem::take(&mut *PENDING.lock().unwrap_or_else(PoisonError::into_inner));
    for (level, p) in lines {
        log.log(level, &p.msg, &[("id", &p.id), ("attr", &p.attr)]);
    }
}

/// The node is ready: later warnings are no longer startup warnings.
pub fn mark_started() {
    STARTED.store(true, Ordering::Release);
}

/// Severity, from the most to the least important.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Something failed and was not handled.
    Error,
    /// Something unusual that was handled.
    Warn,
    /// Normal life events (start, stop, joins).
    Info,
    /// Detail for diagnosing.
    Debug,
    /// Everything.
    Trace,
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
            Level::Trace => "trace",
        }
    }
}

impl FromStr for Level {
    type Err = String;

    fn from_str(s: &str) -> Result<Level, String> {
        match s {
            "error" => Ok(Level::Error),
            "warn" => Ok(Level::Warn),
            "info" => Ok(Level::Info),
            "debug" => Ok(Level::Debug),
            "trace" => Ok(Level::Trace),
            _ => Err(format!("unknown log level `{s}` (error, warn, info, debug, trace)")),
        }
    }
}

/// Line format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// `2026-10-25T10:00:00.123Z info message key=value`.
    Text,
    /// One JSON object per line.
    Json,
}

impl FromStr for Format {
    type Err = String;

    fn from_str(s: &str) -> Result<Format, String> {
        match s {
            "text" => Ok(Format::Text),
            "json" => Ok(Format::Json),
            _ => Err(format!("unknown log format `{s}` (text, json)")),
        }
    }
}

/// Destination of the lines, shared by every thread.
pub struct Logger {
    level: Level,
    format: Format,
    out: Mutex<Box<dyn Write + Send>>,
}

impl fmt::Debug for Logger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Logger").field("level", &self.level).field("format", &self.format).finish()
    }
}

impl Logger {
    /// Logger that writes to `out`.
    pub fn new(level: Level, format: Format, out: Box<dyn Write + Send>) -> Logger {
        Logger { level, format, out: Mutex::new(out) }
    }

    /// Whether lines of `level` are written.
    pub fn enabled(&self, level: Level) -> bool {
        level <= self.level
    }

    /// Writes an event if its level is enabled. A write error is ignored: losing a log line must
    /// not stop the node.
    pub fn log(&self, level: Level, msg: &str, fields: &[(&str, &dyn Display)]) {
        if !self.enabled(level) {
            return;
        }
        let line = self.format_line(now_ms(), level, msg, fields);
        remember(level, &line);
        let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = out.write_all(line.as_bytes()).and_then(|()| out.flush());
    }

    /// The line `log` would write at `ts_ms` (milliseconds since the Unix epoch).
    pub fn format_line(
        &self,
        ts_ms: u64,
        level: Level,
        msg: &str,
        fields: &[(&str, &dyn Display)],
    ) -> String {
        let mut s = String::with_capacity(64 + msg.len());
        let ts = timestamp(ts_ms);
        match self.format {
            Format::Text => {
                let _ = write!(s, "{ts} {} {msg}", level.name());
                for (k, v) in fields {
                    let v = v.to_string();
                    // Quoted and escaped whenever the value could break or forge the line.
                    let plain = !v.is_empty()
                        && !v
                            .chars()
                            .any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '=');
                    if !plain {
                        let _ = write!(s, " {k}=");
                        json_string(&mut s, &v);
                    } else {
                        let _ = write!(s, " {k}={v}");
                    }
                }
            }
            Format::Json => {
                let _ = write!(s, "{{\"ts\":\"{ts}\",\"level\":\"{}\",\"msg\":", level.name());
                json_string(&mut s, msg);
                for (k, v) in fields {
                    s.push(',');
                    json_string(&mut s, k);
                    s.push(':');
                    json_string(&mut s, &v.to_string());
                }
                s.push('}');
            }
        }
        s.push('\n');
        s
    }

    /// `error` line.
    pub fn error(&self, msg: &str, fields: &[(&str, &dyn Display)]) {
        self.log(Level::Error, msg, fields);
    }

    /// `warn` line.
    pub fn warn(&self, msg: &str, fields: &[(&str, &dyn Display)]) {
        self.log(Level::Warn, msg, fields);
    }

    /// `info` line.
    pub fn info(&self, msg: &str, fields: &[(&str, &dyn Display)]) {
        self.log(Level::Info, msg, fields);
    }

    /// `debug` line.
    pub fn debug(&self, msg: &str, fields: &[(&str, &dyn Display)]) {
        self.log(Level::Debug, msg, fields);
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Appends `v` as a JSON string (RFC 8259 escapes).
pub fn json_string(out: &mut String, v: &str) {
    out.push('"');
    for c in v.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// RFC 3339 UTC timestamp with milliseconds.
pub fn timestamp(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        ms % 1000
    )
}

/// Days since 1970-01-01 to (year, month, day) in the proleptic Gregorian calendar.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}
