//! `wire` scenario (006 FR-029, SC-004): the latency of the commands that do no work, over the
//! wire protocol against `--target` or a local cluster of three nodes: `hello`, `ping` (under
//! 30 µs at p50 on loopback) and `whatsmyuri`, one connection, each operation timed alone.

use std::time::{Duration, Instant};

use teseladb_bson::Value;
use teseladb_types::Histogram;

use super::warm_up;
use crate::cli::Args;
use crate::client::{Client, Target, check, doc};
use crate::report::Report;

/// SC-004: `ping` under 30 µs at p50 on loopback.
const PING_P50_NS: f64 = 30_000.0;

/// Times each run of `op` for about `duration`, after `warmup` untimed, at least `min` runs.
pub(crate) fn each(
    warmup: Duration,
    duration: Duration,
    mut op: impl FnMut() -> Result<(), String>,
) -> Result<Histogram, String> {
    const MIN_RUNS: u64 = 100;
    warm_up(warmup, &mut op)?;
    let mut h = Histogram::new();
    let start = Instant::now();
    let mut runs = 0;
    while super::keep_going(runs, MIN_RUNS, start.elapsed() >= duration) {
        let t0 = Instant::now();
        op()?;
        h.record(t0.elapsed().as_nanos() as u64);
        runs += 1;
    }
    Ok(h)
}

/// One command on `db` whose reply must be `ok: 1`.
pub(crate) fn command(c: &mut Client, db: &str, name: &str) -> Result<(), String> {
    check(&c.command(db, doc(vec![(name, Value::Int32(1))]))?)
}

/// Runs the scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let mut c = Client::connect(target.addr()?)?;
    let mut report = Report::new("wire");
    for (name, p50) in [("hello", None), ("ping", Some(PING_P50_NS)), ("whatsmyuri", None)] {
        let h = each(args.warmup, args.duration, || command(&mut c, "admin", name))?;
        report.latency(name, &h, p50, None);
    }
    report.notes.push(format!(
        "one connection, target {}",
        args.target.as_deref().unwrap_or("local cluster of three nodes")
    ));
    Ok(report)
}
