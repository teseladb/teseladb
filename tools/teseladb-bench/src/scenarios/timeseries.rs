//! `timeseries` scenario (015 FR-012, SC-003, SC-004): a time series against `--target` or a
//! local cluster of three nodes. Measurements of [`SENSORS`] sensors, one a second each, go in
//! batches of [`BATCH`] from `--concurrency` clients that take the next batch in turn (the rate of
//! measurements a second is the ingest metric, at least [`INGEST_RATE`] for SC-003); then one hour of
//! one sensor is read by its meta value and time (under 10 ms at p50); last, the same
//! measurements in a collection that is not a time series give the space it takes against the
//! series' (`collStats.size` of both).

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use teseladb_bson::{RawDoc, RawValue, Value};

use super::wire::each;
use crate::cli::Args;
use crate::client::{Client, Target, doc};
use crate::report::{Report, Target as Goal};

/// Database and collections.
const DB: &str = "bench";
const SERIES: &str = "series";
const PLAIN: &str = "series_plain";
/// Sensors, and measurements of one insert command.
const SENSORS: i64 = 10;
const BATCH: i64 = 1000;
/// Start of the measurements (2026-01-01T00:00:00Z) and an hour, in ms.
const T0: i64 = 1_767_225_600_000;
const HOUR: i64 = 3_600_000;
/// SC-003: measurements a second the ingest reaches on three nodes.
const INGEST_RATE: f64 = 1_000_000.0;
/// SC-003: one hour of one sensor read under 10 ms at p50.
const HOUR_P50_NS: f64 = 10_000_000.0;
/// Hours of measurements the space comparison writes into both collections.
const SPACE_HOURS: i64 = 2;

/// Measurement `i` (in order: every sensor at one second, then the next second).
fn measurement(i: i64) -> Value {
    let t = T0 + (i / SENSORS) * 1000;
    Value::Document(doc(vec![
        ("t", Value::DateTime(t)),
        ("sensor", Value::Int64(i % SENSORS)),
        ("temp", Value::Double(20.0 + (i % 97) as f64 / 10.0)),
        ("ok", Value::Boolean(i % 13 != 0)),
    ]))
}

/// Inserts measurements `from..to` into `coll`, `BATCH` at a time.
fn ingest(c: &mut Client, coll: &str, from: i64, to: i64) -> Result<(), String> {
    let mut i = from;
    while i < to {
        let end = (i + BATCH).min(to);
        let docs: Vec<Value> = (i..end).map(measurement).collect();
        c.command(
            DB,
            doc(vec![("insert", Value::String(coll.into())), ("documents", Value::Array(docs))]),
        )?;
        i = end;
    }
    Ok(())
}

/// Ingests into the series from `clients` connections, each taking the next batch, until `duration`
/// has passed and every sensor has at least an hour; returns the measurements written, which are
/// always the first ones (every batch taken is written before its client stops).
fn ingest_for(
    addr: std::net::SocketAddr,
    clients: usize,
    duration: Duration,
) -> Result<i64, String> {
    let next = Arc::new(AtomicI64::new(0));
    let start = Instant::now();
    let workers: Vec<_> = (0..clients.max(1))
        .map(|_| {
            let next = next.clone();
            thread::spawn(move || -> Result<(), String> {
                let mut c = Client::connect(addr)?;
                loop {
                    let from = next.fetch_add(BATCH, Ordering::Relaxed);
                    if from >= SENSORS * 3600 && start.elapsed() >= duration {
                        return Ok(());
                    }
                    ingest(&mut c, SERIES, from, from + BATCH)?;
                }
            })
        })
        .collect();
    for w in workers {
        w.join().map_err(|_| "an ingest client panicked")??;
    }
    // Each client took one batch past the end and wrote nothing for it.
    Ok(next.load(Ordering::Relaxed) - BATCH * clients.max(1) as i64)
}

/// `collStats.size` of `coll`.
fn size(c: &mut Client, coll: &str) -> Result<f64, String> {
    let reply = c.command(DB, doc(vec![("collStats", Value::String(coll.into()))]))?;
    let raw = RawDoc::from_bytes(&reply).map_err(|e| format!("{e:?}"))?;
    match raw.get("size") {
        Some(RawValue::Int32(n)) => Ok(f64::from(n)),
        Some(RawValue::Int64(n)) => Ok(n as f64),
        Some(RawValue::Double(n)) => Ok(n),
        other => Err(format!("collStats without size: {other:?}")),
    }
}

/// Runs the scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let mut c = Client::connect(target.addr()?)?;
    for coll in [SERIES, PLAIN] {
        // The collections may not exist yet.
        let _ = c.command(DB, doc(vec![("drop", Value::String(coll.into()))]));
    }
    let options = doc(vec![
        ("timeField", Value::String("t".into())),
        ("metaField", Value::String("sensor".into())),
    ]);
    c.command(
        DB,
        doc(vec![
            ("create", Value::String(SERIES.into())),
            ("timeseries", Value::Document(options)),
        ]),
    )?;
    // Ingest for the run's duration (at least one batch of each sensor's hour).
    let start = Instant::now();
    let written = ingest_for(target.addr()?, args.concurrency, args.duration)?;
    let rate = written as f64 / start.elapsed().as_secs_f64();
    let hours = (written / SENSORS / 3600).max(1);
    let mut n = 0i64;
    let hour = each(args.warmup, args.duration, || {
        n += 1;
        let from = T0 + (n % hours) * HOUR;
        let range =
            doc(vec![("$gte", Value::DateTime(from)), ("$lt", Value::DateTime(from + HOUR))]);
        let filter =
            doc(vec![("sensor", Value::Int64(n % SENSORS)), ("t", Value::Document(range))]);
        c.command(
            DB,
            doc(vec![
                ("find", Value::String(SERIES.into())),
                ("filter", Value::Document(filter)),
                ("batchSize", Value::Int32(4000)),
            ]),
        )
        .map(|_| ())
    })?;
    let space_end = SENSORS * 3600 * SPACE_HOURS;
    let _ = c.command(DB, doc(vec![("drop", Value::String(SERIES.into()))]));
    c.command(
        DB,
        doc(vec![
            ("create", Value::String(SERIES.into())),
            (
                "timeseries",
                Value::Document(doc(vec![
                    ("timeField", Value::String("t".into())),
                    ("metaField", Value::String("sensor".into())),
                ])),
            ),
        ]),
    )?;
    ingest(&mut c, SERIES, 0, space_end)?;
    ingest(&mut c, PLAIN, 0, space_end)?;
    let ratio = size(&mut c, SERIES)? / size(&mut c, PLAIN)?;
    let mut report = Report::new("timeseries");
    report.push("ingest", "measurements/s", rate, Some(Goal::AtLeast(INGEST_RATE)));
    report.latency("hour_of_one_sensor", &hour, Some(HOUR_P50_NS), None);
    report.push("space_ratio", "series/plain", ratio, Some(Goal::AtMost(1.0)));
    report.notes.push(format!(
        "{SENSORS} sensors, one measurement a second each, batches of {BATCH}, {} ingest connections, target {}",
        args.concurrency,
        args.target.as_deref().unwrap_or("local cluster of three nodes")
    ));
    Ok(report)
}
