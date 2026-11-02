//! `index-write` and `index-read` (009 FR-018, SC-002) over the wire protocol, against `--target`
//! or a local cluster of three nodes, one connection, each operation timed alone:
//! - `index-write`: inserting a document into a collection with three secondary indexes, first
//!   with the index entries in the document's range (`same_range`, under 350 µs at p50), then,
//!   on a local cluster, once the data range was split between the documents and their index
//!   entries (`other_range`, under 1 ms at p50). The entries of index `n` follow the documents
//!   in the key space, so a split at an `_id` above every document's leaves them all on the other
//!   side.
//! - `index-read`: with `--records` documents loaded, a `find` by equality on an indexed field
//!   that gives one document.
//! - `index-build-load` (SC-003): with `--records` documents of about 1 KiB loaded (ten million
//!   are 10 GB), the p99 of single inserts before a build and while `createIndexes` builds an
//!   index over them on another connection; the second must stay under twice the first.

use teseladb_bson::{Document, Value};

use super::wire::each;
use crate::cli::Args;
use crate::client::{Client, Target, check, doc};
use crate::report::Report;

/// Database and collections of the scenarios.
const DB: &str = "bench";
const WRITES: &str = "index_write";
const READS: &str = "index_read";
const BUILDS: &str = "index_build";
/// The padding of each document of `index-build-load`, so that ten million make 10 GB.
const PAD_BYTES: usize = 1_000;
/// Documents per `insert` while `index-build-load` loads its collection.
const LOAD_BATCH: usize = 1_000;
/// SC-003: the p99 of the writes during a build, at most this times the p99 before it.
const BUILD_P99_FACTOR: f64 = 2.0;
/// The fewest writes timed during a build, however fast it ends.
const MIN_BUILD_WRITES: u64 = 100;
/// The indexed fields of `index-write`.
const FIELDS: [&str; 3] = ["a", "b", "c"];
/// SC-002: a write with three indexes in the same range, and in other ranges.
const SAME_RANGE_P50_NS: f64 = 350_000.0;
const OTHER_RANGE_P50_NS: f64 = 1_000_000.0;
/// An `_id` above every integer `_id` (strings sort after numbers).
const ABOVE_EVERY_ID: &str = "~";

fn run_on(c: &mut Client, command: Vec<(&str, Value)>) -> Result<(), String> {
    check(&c.command(DB, doc(command))?)
}

/// Drops `coll` and creates it with an ascending index on each of `fields`.
fn indexed(c: &mut Client, coll: &str, fields: &[&str]) -> Result<(), String> {
    // The collection may not exist yet.
    let _ = c.command(DB, doc(vec![("drop", Value::String(coll.into()))]));
    let indexes = fields
        .iter()
        .map(|f| {
            Value::Document(doc(vec![
                ("key", Value::Document(doc(vec![(f, Value::Int32(1))]))),
                ("name", Value::String(format!("{f}_1"))),
            ]))
        })
        .collect();
    let create = doc(vec![
        ("createIndexes", Value::String(coll.into())),
        ("indexes", Value::Array(indexes)),
    ]);
    c.until_ready(DB, create)
}

/// The `insert` of document `id` of `index-write`, with a value in each indexed field.
fn insert_command(coll: &str, id: i64) -> Document {
    let mut pairs = vec![("_id", Value::Int64(id))];
    pairs.extend(FIELDS.iter().map(|f| (*f, Value::Int64(id))));
    let d = Value::Document(doc(pairs));
    doc(vec![("insert", Value::String(coll.into())), ("documents", Value::Array(vec![d]))])
}

fn insert(c: &mut Client, coll: &str, id: i64) -> Result<(), String> {
    check(&c.command(DB, insert_command(coll, id))?)
}

/// `index-write`.
pub fn run_write(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let mut c = Client::connect(target.addr()?)?;
    indexed(&mut c, WRITES, &FIELDS)?;
    // The first document waits for the cluster's leaders; it is not timed.
    c.until_ready(DB, insert_command(WRITES, 0))?;
    let mut next = 0i64;
    let same = each(args.warmup, args.duration, || {
        next += 1;
        insert(&mut c, WRITES, next)
    })?;
    let mut report = Report::new("index-write");
    report.latency("same_range", &same, Some(SAME_RANGE_P50_NS), None);
    // Only a local cluster lets the scenario split its range.
    if let (Some(admin_at), Some(range)) =
        (target.admin_addr(), target.ranges().and_then(|r| r.first().copied()))
    {
        let at = format!("{{\"ns\": \"{DB}.{WRITES}\", \"_id\": \"{ABOVE_EVERY_ID}\"}}");
        super::cluster::admin(admin_at, &["range", "split", &range.to_string(), "--at", &at])?;
        target.wait_ranges(2)?;
        let other = each(args.warmup, args.duration, || {
            next += 1;
            insert(&mut c, WRITES, next)
        })?;
        report.latency("other_range", &other, Some(OTHER_RANGE_P50_NS), None);
    } else {
        report
            .notes
            .push("other_range needs a local cluster: the scenario splits its range".into());
    }
    report.notes.push(format!("three indexes, target {}", target_name(args)));
    Ok(report)
}

/// `index-read`.
pub fn run_read(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let mut c = Client::connect(target.addr()?)?;
    indexed(&mut c, READS, &["k"])?;
    let records = i64::try_from(args.records.max(1)).unwrap_or(i64::MAX);
    for id in 1..=records {
        let d = Value::Document(doc(vec![("_id", Value::Int64(id)), ("k", Value::Int64(id))]));
        // The load is not timed: each insert waits for the cluster's leaders.
        let insert = doc(vec![
            ("insert", Value::String(READS.into())),
            ("documents", Value::Array(vec![d])),
        ]);
        c.until_ready(DB, insert)?;
    }
    // A first read, not timed, waits for the cluster's leaders.
    let first = doc(vec![
        ("find", Value::String(READS.into())),
        ("filter", Value::Document(doc(vec![("k", Value::Int64(1))]))),
    ]);
    c.until_ready(DB, first)?;
    let mut k = 0i64;
    let read = each(args.warmup, args.duration, || {
        k = k % records + 1;
        let filter = Value::Document(doc(vec![("k", Value::Int64(k))]));
        run_on(
            &mut c,
            vec![
                ("find", Value::String(READS.into())),
                ("filter", filter),
                ("limit", Value::Int32(1)),
            ],
        )
    })?;
    let mut report = Report::new("index-read");
    report.latency("index_read", &read, None, None);
    report.notes.push(format!("{records} documents, target {}", target_name(args)));
    Ok(report)
}

fn target_name(args: &Args) -> &str {
    args.target.as_deref().unwrap_or("local cluster of three nodes")
}

/// A document of `index-build-load`.
fn padded(id: i64) -> Value {
    Value::Document(doc(vec![
        ("_id", Value::Int64(id)),
        ("k", Value::Int64(id)),
        ("pad", Value::String("x".repeat(PAD_BYTES))),
    ]))
}

fn insert_padded(c: &mut Client, id: i64) -> Result<(), String> {
    let insert = doc(vec![
        ("insert", Value::String(BUILDS.into())),
        ("documents", Value::Array(vec![padded(id)])),
    ]);
    check(&c.command(DB, insert)?)
}

/// `index-build-load`.
pub fn run_build_load(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let addr = target.addr()?;
    let mut c = Client::connect(addr)?;
    // The collection may not exist yet.
    let _ = c.command(DB, doc(vec![("drop", Value::String(BUILDS.into()))]));
    let records = i64::try_from(args.records.max(1)).unwrap_or(i64::MAX);
    let mut id = 0i64;
    while id < records {
        let end = records.min(id + LOAD_BATCH as i64);
        let batch = ((id + 1)..=end).map(padded).collect();
        // The load is not timed: each batch waits for the cluster's leaders.
        let insert =
            doc(vec![("insert", Value::String(BUILDS.into())), ("documents", Value::Array(batch))]);
        c.until_ready(DB, insert)?;
        id = end;
    }
    let before = each(args.warmup, args.duration, || {
        id += 1;
        insert_padded(&mut c, id)
    })?;
    let (during, build_ms) = std::thread::scope(|s| -> Result<_, String> {
        let builder = s.spawn(move || -> Result<u128, String> {
            let mut b = Client::connect(addr)?;
            let spec = doc(vec![
                ("key", Value::Document(doc(vec![("k", Value::Int32(1))]))),
                ("name", Value::String("k_1".into())),
            ]);
            let started = std::time::Instant::now();
            run_on(
                &mut b,
                vec![
                    ("createIndexes", Value::String(BUILDS.into())),
                    ("indexes", Value::Array(vec![Value::Document(spec)])),
                ],
            )?;
            Ok(started.elapsed().as_millis())
        });
        let mut h = teseladb_types::Histogram::new();
        let mut runs = 0u64;
        while runs < MIN_BUILD_WRITES || !builder.is_finished() {
            id += 1;
            let t0 = std::time::Instant::now();
            insert_padded(&mut c, id)?;
            h.record(t0.elapsed().as_nanos() as u64);
            runs += 1;
        }
        let ms = builder.join().map_err(|_| "the index build panicked".to_owned())??;
        Ok((h, ms))
    })?;
    let mut report = Report::new("index-build-load");
    report.latency("write_before", &before, None, None);
    let limit = before.percentile(99.0) as f64 * BUILD_P99_FACTOR;
    report.latency("write_during_build", &during, None, Some(limit));
    report.push("build_ms", "ms", build_ms as f64, None);
    let bytes = u64::try_from(records).unwrap_or(u64::MAX).saturating_mul(PAD_BYTES as u64);
    report.notes.push(format!(
        "{records} documents (about {} MB), index on k built while writing, target {}",
        bytes / 1_000_000,
        target_name(args)
    ));
    Ok(report)
}
