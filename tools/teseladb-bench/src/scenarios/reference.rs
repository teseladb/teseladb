//! `reference` (002 FR-020, SC-005): the fixed scenario of the hot path that the 10 % rule of
//! the baselines applies to, over the wire protocol against `--target` or a local cluster. It
//! measures a write confirmed by the majority with the journal (`insert`, `w: "majority"`,
//! `j: true`) and a read by `_id` served by the leader's lease (`find`, `readConcern: "local"`),
//! and a query by a secondary index (009 T029: `find` by equality on the indexed field `v`, which
//! gives the document just written), and then a `$search` term query for the value it wrote (011
//! T050) over a `default` search index built before the load, one after the other on each worker,
//! on documents of 100 bytes. The read and the query have to give the document just written (one
//! that does not is an error); the search has to find it too (011 SC-003), and the share of
//! searches that did is a result with its target of one.

use std::sync::atomic::{AtomicU64, Ordering};

use teseladb_bson::{RawDoc, RawValue, Value};

use super::ycsb_gen::{Rng, field, key};
use crate::cli::Args;
use crate::client::{Client, Target, debug, doc};
use crate::load::{Load, drive};
use crate::report::{Report, Target as Goal};

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

/// The `_id` of the first document of a cursor's first batch, `None` when it is empty.
fn first_id(reply: &[u8]) -> Result<Option<String>, String> {
    let d = RawDoc::from_bytes(reply).map_err(debug)?;
    let Some(RawValue::Document(cursor)) = d.get("cursor") else {
        return Err("a reply without a cursor".into());
    };
    let Some(RawValue::Array(batch)) = cursor.get("firstBatch") else {
        return Err("a cursor without a first batch".into());
    };
    let Some(RawValue::Document(first)) = batch.iter().next() else {
        return Ok(None);
    };
    match first.get("_id") {
        Some(RawValue::String(id)) => Ok(Some(id.to_owned())),
        other => Err(format!("a document whose _id is {other:?}")),
    }
}

/// An error unless `reply` gives document `id` first: `what` did not find what was written.
fn expect(what: &str, reply: &[u8], id: &str) -> Result<(), String> {
    match first_id(reply)? {
        Some(found) if found == id => Ok(()),
        other => Err(format!("the {what} of document {id} gave {other:?}")),
    }
}

fn read(c: &mut Client, id: String) -> Result<Vec<u8>, String> {
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
}

fn query(c: &mut Client, value: String) -> Result<Vec<u8>, String> {
    let filter = doc(vec![(INDEXED, Value::String(value))]);
    c.command(
        DB,
        doc(vec![
            ("find", Value::String(COLLECTION.into())),
            ("filter", Value::Document(filter)),
            ("limit", Value::Int32(1)),
        ]),
    )
}

/// The `$search` term query for `value` on `v`, first result only.
fn search(c: &mut Client, value: String) -> Result<Vec<u8>, String> {
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
    Query(String, String),
    Search(String, String),
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
    super::search::default_index(&mut setup, DB, COLLECTION, INDEX_READY)?;
    let next = AtomicU64::new(0);
    let (searched, found) = (AtomicU64::new(0), AtomicU64::new(0));
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
                expect(READ, &read(c, id.clone())?, &id)?;
                *next_op = Next::Query(id, value);
                Ok(READ)
            }
            Next::Query(id, value) => {
                expect(QUERY, &query(c, value.clone())?, &id)?;
                *next_op = Next::Search(id, value);
                Ok(QUERY)
            }
            Next::Search(id, value) => {
                let hit = first_id(&search(c, value)?)?.is_some_and(|f| f == id);
                searched.fetch_add(1, Ordering::Relaxed);
                found.fetch_add(u64::from(hit), Ordering::Relaxed);
                Ok(SEARCH)
            }
        },
    )?;
    let mut report = Report::new("reference");
    for (kind, h) in &out.latencies {
        report.latency(kind, h, None, None);
    }
    report.push("throughput", "op/s", out.throughput(), None);
    // The searches of the warm-up count too: a search that misses is a miss whenever it runs.
    let ratio = found.into_inner() as f64 / searched.into_inner().max(1) as f64;
    report.push("search.found", "ratio", ratio, Some(Goal::AtLeast(1.0)));
    report.push("errors", "op", out.errors as f64, Some(Goal::AtMost(0.0)));
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

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use teseladb_bson::Document;

    use super::*;

    fn batch(docs: Vec<Value>) -> Vec<u8> {
        let cursor = doc(vec![("firstBatch", Value::Array(docs))]);
        doc(vec![("cursor", Value::Document(cursor))]).to_bytes().expect("bson")
    }

    fn with_id(id: Value) -> Value {
        Value::Document(doc(vec![("_id", id)]))
    }

    #[test]
    fn the_steps_must_give_the_document_just_written() {
        let hit = batch(vec![with_id(Value::String("user1".into()))]);
        assert_eq!(first_id(&hit), Ok(Some("user1".into())));
        assert_eq!(expect(READ, &hit, "user1"), Ok(()));
        let other = expect(QUERY, &hit, "user2").unwrap_err();
        assert!(other.contains("query of document user2"), "{other}");
        assert_eq!(first_id(&batch(vec![])), Ok(None));
        assert!(expect(READ, &batch(vec![]), "user1").unwrap_err().contains("None"));
        assert!(first_id(&batch(vec![with_id(Value::Int32(1))])).is_err());
        let no_batch = doc(vec![("cursor", Value::Document(Document::new()))]);
        assert!(first_id(&no_batch.to_bytes().expect("bson")).is_err());
        let no_cursor = doc(vec![("ok", Value::Int32(1))]);
        assert!(first_id(&no_cursor.to_bytes().expect("bson")).is_err());
        assert!(first_id(&[1, 2]).is_err());
    }
}
