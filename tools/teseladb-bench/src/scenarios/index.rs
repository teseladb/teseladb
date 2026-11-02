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

use teseladb_bson::{Document, Value};

use super::wire::each;
use crate::cli::Args;
use crate::client::{Client, Target, check, doc};
use crate::report::Report;

/// Database and collections of the scenarios.
const DB: &str = "bench";
const WRITES: &str = "index_write";
const READS: &str = "index_read";
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
