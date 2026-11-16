//! Small printing helpers: joining strings, printing time points with microsecond resolution,
//! and a timestamped log line to standard output.
//!
//! Pointers need no helper: Rust formats them with `{:p}` on any writer.

use std::fmt;
use std::io::Write as _;

/// Joins `items` with `sep` between consecutive items; an empty input gives an empty string.
#[deprecated(note = "use slice::join instead")]
pub fn format_separated<I>(items: I, sep: &str) -> String
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let mut ret = String::new();
    let mut it = items.into_iter();
    let Some(first) = it.next() else {
        return ret;
    };
    ret += first.as_ref();
    for x in it {
        ret += sep;
        ret += x.as_ref();
    }
    ret
}

/// Prints a time point as seconds and microseconds since its epoch
/// (`"{seconds:>12}.{micros:06}"`).
#[derive(Debug, Clone, Copy)]
pub struct UsecfmtWrapper<T> {
    /// The time point.
    pub val: T,
}

/// Wraps `tp` for printing with microsecond resolution.
#[inline]
pub fn usecfmt<T>(tp: T) -> UsecfmtWrapper<T> {
    UsecfmtWrapper { val: tp }
}

impl fmt::Display for UsecfmtWrapper<std::time::Duration> {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        let usec = self.val.as_micros();
        write!(os, "{:>12}.{:06}", usec / 1000000, usec % 1000000)
    }
}

impl fmt::Display for UsecfmtWrapper<std::time::Instant> {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `Instant` has no epoch, so the time since the first call in this process is printed.
        static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let epoch = *EPOCH.get_or_init(std::time::Instant::now);
        fmt::Display::fmt(&usecfmt(self.val.saturating_duration_since(epoch)), os)
    }
}

/// Prints the current time followed by `args` to standard output. Write errors are ignored.
pub fn log(args: fmt::Arguments<'_>) {
    let mut out = std::io::stdout().lock();
    // Output errors are ignored: this is best-effort diagnostic output.
    let _ = write!(out, "{} ", usecfmt(std::time::Instant::now()));
    let _ = out.write_fmt(args);
}

/// Writes `args` to `os` and returns it, ignoring write errors. Kept for old callers; prefer
/// `write!` or `format!`.
#[deprecated(note = "use std::format() or fmt::print()")]
pub fn fmt_print<'a, W: std::io::Write>(os: &'a mut W, args: fmt::Arguments<'_>) -> &'a mut W {
    // Output errors are ignored: the writer is returned for chaining.
    let _ = os.write_fmt(args);
    os
}
