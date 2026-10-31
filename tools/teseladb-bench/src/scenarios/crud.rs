//! `crud` scenario (006 FR-029, SC-004): the CRUD commands over the wire protocol against
//! `--target` or a local cluster of three nodes, one connection, each operation timed alone:
//! inserting a 1 KiB document (under 350 µs at p50 with three nodes), reading it by `_id`,
//! updating it and deleting it.

use teseladb_bson::Value;

use super::wire::each;
use crate::cli::Args;
use crate::client::{Client, Target, check, doc};
use crate::report::Report;

/// Database and collection of the documents.
const DB: &str = "bench";
const COLLECTION: &str = "crud";
/// SC-004: inserting a 1 KiB document under 350 µs at p50 with three nodes.
const INSERT_P50_NS: f64 = 350_000.0;
/// The document's padding: with its `_id` and field names the document takes 1 KiB.
const PAD: usize = 1000;

fn run_on(c: &mut Client, command: Vec<(&str, Value)>) -> Result<(), String> {
    check(&c.command(DB, doc(command))?)
}

/// Runs the scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let mut c = Client::connect(target.addr()?)?;
    // The collection may not exist yet.
    let _ = c.command(DB, doc(vec![("drop", Value::String(COLLECTION.into()))]));
    let coll = || Value::String(COLLECTION.into());
    let mut next = 0i64;
    let pad = "x".repeat(PAD);
    let insert = each(args.warmup, args.duration, || {
        next += 1;
        let d = doc(vec![("_id", Value::Int64(next)), ("pad", Value::String(pad.clone()))]);
        run_on(
            &mut c,
            vec![("insert", coll()), ("documents", Value::Array(vec![Value::Document(d)]))],
        )
    })?;
    let written = next.max(1);
    let mut k = 0i64;
    let mut key = || {
        k = k % written + 1;
        Value::Document(doc(vec![("_id", Value::Int64(k))]))
    };
    let find = each(args.warmup, args.duration, || {
        run_on(&mut c, vec![("find", coll()), ("filter", key()), ("limit", Value::Int32(1))])
    })?;
    let update = each(args.warmup, args.duration, || {
        let set = Value::Document(doc(vec![(
            "$inc",
            Value::Document(doc(vec![("n", Value::Int32(1))])),
        )]));
        let u = Value::Document(doc(vec![("q", key()), ("u", set)]));
        run_on(&mut c, vec![("update", coll()), ("updates", Value::Array(vec![u]))])
    })?;
    let delete = each(args.warmup, args.duration, || {
        let d = Value::Document(doc(vec![("q", key()), ("limit", Value::Int32(1))]));
        run_on(&mut c, vec![("delete", coll()), ("deletes", Value::Array(vec![d]))])
    })?;
    let mut report = Report::new("crud");
    report.latency("insert", &insert, Some(INSERT_P50_NS), None);
    report.latency("find", &find, None, None);
    report.latency("update", &update, None, None);
    report.latency("delete", &delete, None, None);
    report.notes.push(format!(
        "documents of 1 KiB, one connection, target {}",
        args.target.as_deref().unwrap_or("local cluster of three nodes")
    ));
    Ok(report)
}
