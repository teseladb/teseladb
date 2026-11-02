//! `reference` (002 FR-020, SC-005): the fixed scenario of the hot path that the 10 % rule of
//! the baselines applies to, over the wire protocol against `--target` or a local cluster. It
//! measures a write confirmed by the majority with the journal (`insert`, `w: "majority"`,
//! `j: true`) and a read by `_id` served by the leader's lease (`find`, `readConcern: "local"`),
//! and a query by a secondary index (009 T029: `find` by equality on the indexed field `v`, which
//! gives the document just written), and then a `$search` term query for the value it wrote (011
//! T050) over a `default` search index built before the load, one after the other on each worker,
//! on documents of 100 bytes.

use std::sync::atomic::{AtomicU64, Ordering};

use teseladb_bson::Value;

use super::ycsb_gen::{Rng, field, key};
use crate::cli::Args;
use crate::client::{Client, Target, doc};
use crate::load::{Load, drive};
use crate::report::Report;

const DB: &str = "reference";
const COLLECTION: &str = "docs";
/// Seed of the workers' sources (worker `w` uses `SEED + w`).
const SEED: u64 = 0x5245_4645;
/// Kinds of operation in the report.
const WRITE: &str = "write";
const READ: &str = "read";
const QUERY: &str = "query";
const SEARCH: &str = "search";
/// Longest wait for the search index to be `READY` before the load.
const INDEX_READY: std::time::Duration = std::time::Duration::from_secs(120);
/// The indexed field of the documents.
const INDEXED: &str = "v";

fn insert(c: &mut Client, id: String, value: String) -> Result<(), String> {
    let document = doc(vec![("_id", Value::String(id)), (INDEXED, Value::String(value))]);
    let wc = doc(vec![("w", Value::String("majority".into())), ("j", Value::Boolean(true))]);
    c.command(
        DB,
        doc(vec![
            ("insert", Value::String(COLLECTION.into())),
            ("documents", Value::Array(vec![Value::Document(document)])),
            ("writeConcern", Value::Document(wc)),
        ]),
    )
    .map(drop)
}

fn read(c: &mut Client, id: String) -> Result<(), String> {
    let filter = doc(vec![("_id", Value::String(id))]);
    let rc = doc(vec![("level", Value::String("local".into()))]);
    c.command(
        DB,
        doc(vec![
            ("find", Value::String(COLLECTION.into())),
            ("filter", Value::Document(filter)),
            ("limit", Value::Int32(1)),
            ("readConcern", Value::Document(rc)),
        ]),
    )
    .map(drop)
}

fn query(c: &mut Client, value: String) -> Result<(), String> {
    let filter = doc(vec![(INDEXED, Value::String(value))]);
    c.command(
        DB,
        doc(vec![
            ("find", Value::String(COLLECTION.into())),
            ("filter", Value::Document(filter)),
            ("limit", Value::Int32(1)),
        ]),
    )
    .map(drop)
}

/// The `$search` term query for `value` on `v`, first result only.
fn search(c: &mut Client, value: String) -> Result<(), String> {
    let text = doc(vec![("path", Value::String(INDEXED.into())), ("query", Value::String(value))]);
    let stage = doc(vec![("$search", Value::Document(doc(vec![("text", Value::Document(text))])))]);
    let limit = doc(vec![("$limit", Value::Int32(1))]);
    c.command(
        DB,
        doc(vec![
            ("aggregate", Value::String(COLLECTION.into())),
            ("pipeline", Value::Array(vec![Value::Document(stage), Value::Document(limit)])),
            ("cursor", Value::Document(doc(vec![]))),
        ]),
    )
    .map(drop)
}

/// Creates the `default` search index of the collection and waits until it is `READY`.
fn search_index(c: &mut Client) -> Result<(), String> {
    let definition =
        doc(vec![("mappings", Value::Document(doc(vec![("dynamic", Value::Boolean(true))])))]);
    let index = doc(vec![
        ("name", Value::String("default".into())),
        ("definition", Value::Document(definition)),
    ]);
    c.command(
        DB,
        doc(vec![
            ("createSearchIndexes", Value::String(COLLECTION.into())),
            ("indexes", Value::Array(vec![Value::Document(index)])),
        ]),
    )?;
    let start = std::time::Instant::now();
    while start.elapsed() < INDEX_READY {
        let list = doc(vec![("listSearchIndexes", Value::String(COLLECTION.into()))]);
        let reply = c.command(DB, list)?;
        if reply.windows(5).any(|w| w == b"READY") {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    Err("the search index of the reference scenario was not READY in time".to_owned())
}

/// The index of `v`.
fn create_index(c: &mut Client) -> Result<(), String> {
    let index = doc(vec![
        ("key", Value::Document(doc(vec![(INDEXED, Value::Int32(1))]))),
        ("name", Value::String(format!("{INDEXED}_1"))),
    ]);
    c.until_ready(
        DB,
        doc(vec![
            ("createIndexes", Value::String(COLLECTION.into())),
            ("indexes", Value::Array(vec![Value::Document(index)])),
        ]),
    )
}

/// What a worker does next: write a document, read it by `_id`, query it by its index, then
/// search its value.
enum Next {
    Write,
    Read(String, String),
    Query(String),
    Search(String),
}

/// Runs the scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let addr = target.addr()?;
    let mut setup = Client::connect(addr)?;
    // The collection may not exist yet.
    let _ = setup.command(DB, doc(vec![("drop", Value::String(COLLECTION.into()))]));
    create_index(&mut setup)?;
    // A first document, not timed, waits for the cluster's leaders.
    let probe =
        doc(vec![("_id", Value::String("ready".into())), (INDEXED, Value::String(String::new()))]);
    setup.until_ready(
        DB,
        doc(vec![
            ("insert", Value::String(COLLECTION.into())),
            ("documents", Value::Array(vec![Value::Document(probe)])),
        ]),
    )?;
    search_index(&mut setup)?;
    let next = AtomicU64::new(0);
    let load = Load {
        warmup: args.warmup,
        duration: args.duration,
        concurrency: args.concurrency,
        rate: args.rate,
    };
    // Each worker cycles: it writes a document, reads it by `_id`, queries it by its index, then
    // searches its value.
    let out = drive(
        &load,
        |w| Ok((Client::connect(addr)?, Rng::new(SEED + w as u64), Next::Write)),
        |(c, rng, next_op)| match std::mem::replace(next_op, Next::Write) {
            Next::Write => {
                let id = key(next.fetch_add(1, Ordering::Relaxed));
                let value = field(rng);
                insert(c, id.clone(), value.clone())?;
                *next_op = Next::Read(id, value);
                Ok(WRITE)
            }
            Next::Read(id, value) => {
                read(c, id)?;
                *next_op = Next::Query(value);
                Ok(READ)
            }
            Next::Query(value) => {
                query(c, value.clone())?;
                *next_op = Next::Search(value);
                Ok(QUERY)
            }
            Next::Search(value) => {
                search(c, value)?;
                Ok(SEARCH)
            }
        },
    )?;
    let mut report = Report::new("reference");
    for (kind, h) in &out.latencies {
        report.latency(kind, h, None, None);
    }
    report.push("throughput", "op/s", out.throughput(), None);
    report.push("errors", "op", out.errors as f64, None);
    if let Some(e) = &out.first_error {
        report.notes.push(format!("first error: {e}"));
    }
    report.notes.push(format!(
        "{} workers, {}, target {}",
        args.concurrency.max(1),
        args.rate.map_or("closed loop".to_owned(), |r| format!("{r} op/s")),
        args.target.as_deref().unwrap_or("local cluster of three nodes")
    ));
    Ok(report)
}
