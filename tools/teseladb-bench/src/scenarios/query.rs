//! `aggregate` and `sort` scenarios (010 FR-021, SC-004): queries over the wire protocol against
//! `--target` or a local cluster of three nodes, one connection, each command timed alone, over
//! `--records` documents `{_id, g, n, pad}` where each value of `g` has 10 documents and `g` has
//! an index. `aggregate` times the query by that index of equality that gives 10 documents
//! (under 500 µs at p50 with three nodes), a `$group` of the whole collection and a `$match`
//! with its `$group`; `sort` times the first 10 by the index, the first 10 sorted in memory and
//! a pipeline's `$sort` with its `$limit`.

use teseladb_bson::{Document, Value};

use super::wire::each;
use crate::cli::Args;
use crate::client::{Client, Target, check, doc};
use crate::report::Report;

/// Database and collection of the documents.
const DB: &str = "bench";
const COLLECTION: &str = "query";
/// Documents of each value of `g`.
const PER_GROUP: i64 = 10;
/// SC-004: the query by an index of equality that gives 10 documents under 500 µs at p50.
const EQUALITY_P50_NS: f64 = 500_000.0;
/// Documents written by each `insert`.
const BATCH: i64 = 500;
/// The documents' padding.
const PAD: usize = 200;

fn coll() -> Value {
    Value::String(COLLECTION.into())
}

fn run_on(c: &mut Client, command: Document) -> Result<(), String> {
    check(&c.command(DB, command)?)
}

/// Writes `records` documents and the index of `g`; the number of values of `g`.
fn load(c: &mut Client, records: u64) -> Result<i64, String> {
    // The collection may not exist yet.
    let _ = c.command(DB, doc(vec![("drop", coll())]));
    let records = i64::try_from(records).unwrap_or(i64::MAX).max(PER_GROUP);
    let pad = "x".repeat(PAD);
    let mut next = 0;
    while next < records {
        let docs: Vec<Value> = (next..(next + BATCH).min(records))
            .map(|i| {
                Value::Document(doc(vec![
                    ("_id", Value::Int64(i)),
                    ("g", Value::Int64(i / PER_GROUP)),
                    ("n", Value::Int64((i * 7919) % records)),
                    ("pad", Value::String(pad.clone())),
                ]))
            })
            .collect();
        next += BATCH;
        run_on(c, doc(vec![("insert", coll()), ("documents", Value::Array(docs))]))?;
    }
    let index = doc(vec![
        ("key", Value::Document(doc(vec![("g", Value::Int32(1))]))),
        ("name", Value::String("g_1".into())),
    ]);
    run_on(
        c,
        doc(vec![
            ("createIndexes", coll()),
            ("indexes", Value::Array(vec![Value::Document(index)])),
        ]),
    )?;
    Ok(records / PER_GROUP)
}

fn aggregate(pipeline: Vec<Document>) -> Document {
    doc(vec![
        ("aggregate", coll()),
        ("pipeline", Value::Array(pipeline.into_iter().map(Value::Document).collect())),
        ("cursor", Value::Document(Document::new())),
    ])
}

fn stage(name: &str, spec: Vec<(&str, Value)>) -> Document {
    doc(vec![(name, Value::Document(doc(spec)))])
}

fn note(args: &Args) -> String {
    format!(
        "{} documents, 10 per value of g, one connection, target {}",
        args.records,
        args.target.as_deref().unwrap_or("local cluster of three nodes")
    )
}

/// The `aggregate` scenario.
pub fn run_aggregate(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let mut c = Client::connect(target.addr()?)?;
    let groups = load(&mut c, args.records)?;
    let mut k = 0i64;
    let equality = each(args.warmup, args.duration, || {
        k = (k + 1) % groups;
        let filter = Value::Document(doc(vec![("g", Value::Int64(k))]));
        run_on(&mut c, doc(vec![("find", coll()), ("filter", filter)]))
    })?;
    let sum = || Value::Document(doc(vec![("$sum", Value::String("$n".into()))]));
    let group = each(args.warmup, args.duration, || {
        let g = stage("$group", vec![("_id", Value::String("$g".into())), ("s", sum())]);
        run_on(&mut c, aggregate(vec![g]))
    })?;
    let mut m = 0i64;
    let match_group = each(args.warmup, args.duration, || {
        m = (m + 1) % groups;
        let lt = Value::Document(doc(vec![("$lt", Value::Int64(m + 10))]));
        let filter = stage("$match", vec![("g", lt)]);
        let g = stage("$group", vec![("_id", Value::Null), ("s", sum())]);
        run_on(&mut c, aggregate(vec![filter, g]))
    })?;
    let mut report = Report::new("aggregate");
    report.latency("equality_10", &equality, Some(EQUALITY_P50_NS), None);
    report.latency("group", &group, None, None);
    report.latency("match_group", &match_group, None, None);
    report.notes.push(note(args));
    Ok(report)
}

/// The `sort` scenario.
pub fn run_sort(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let mut c = Client::connect(target.addr()?)?;
    load(&mut c, args.records)?;
    let top = |field: &str, dir: i32| {
        doc(vec![
            ("find", coll()),
            ("sort", Value::Document(doc(vec![(field, Value::Int32(dir))]))),
            ("limit", Value::Int32(10)),
        ])
    };
    let indexed = each(args.warmup, args.duration, || run_on(&mut c, top("g", -1)))?;
    let memory = each(args.warmup, args.duration, || run_on(&mut c, top("n", 1)))?;
    let pipeline = each(args.warmup, args.duration, || {
        let sort = stage("$sort", vec![("n", Value::Int32(-1))]);
        let limit = doc(vec![("$limit", Value::Int32(10))]);
        run_on(&mut c, aggregate(vec![sort, limit]))
    })?;
    let mut report = Report::new("sort");
    report.latency("indexed_top10", &indexed, None, None);
    report.latency("memory_top10", &memory, None, None);
    report.latency("aggregate_sort", &pipeline, None, None);
    report.notes.push(note(args));
    Ok(report)
}

/// SC-003: what a node keeps resident per core during a `$group` of high cardinality, at most
/// 256 MB.
const RESIDENT_PER_CORE_MB: f64 = 256.0;
/// The padding of a document of the `group-large` scenario: 1 KiB with its fields.
const LARGE_PAD: usize = 1000;

/// A number of a reply at a dotted path.
pub(crate) fn number_at(reply: &[u8], path: &str) -> Option<f64> {
    let mut doc = Document::from(teseladb_bson::RawDoc::from_bytes(reply).ok()?);
    let parts: Vec<&str> = path.split('.').collect();
    for (i, p) in parts.iter().enumerate() {
        let v = doc.get(p)?.clone();
        if i + 1 == parts.len() {
            return match v {
                Value::Int32(n) => Some(f64::from(n)),
                Value::Int64(n) => Some(n as f64),
                Value::Double(n) => Some(n),
                _ => None,
            };
        }
        let Value::Document(next) = v else { return None };
        doc = next;
    }
    None
}

/// The `group-large` scenario (010 SC-003, T027): `--records` documents of 1 KiB, each with a
/// value of `g` of its own (50 GB with 52 428 800 records), then one `$group` by `g` with
/// `allowDiskUse`; the groups must be as many as the documents, and the node's resident memory
/// per core (`serverStatus.mem.resident` over `hostInfo.system.numCores`) stays under 256 MB.
pub fn run_group_large(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let mut c = Client::connect(target.addr()?)?;
    let _ = c.command(DB, doc(vec![("drop", coll())]));
    let records = i64::try_from(args.records).unwrap_or(i64::MAX);
    let pad = "x".repeat(LARGE_PAD);
    let loading = std::time::Instant::now();
    let mut next = 0;
    while next < records {
        let docs: Vec<Value> = (next..(next + BATCH).min(records))
            .map(|i| {
                Value::Document(doc(vec![
                    ("_id", Value::Int64(i)),
                    ("g", Value::Int64(i)),
                    ("pad", Value::String(pad.clone())),
                ]))
            })
            .collect();
        next += BATCH;
        run_on(&mut c, doc(vec![("insert", coll()), ("documents", Value::Array(docs))]))?;
    }
    let loaded = loading.elapsed();
    let group = stage("$group", vec![("_id", Value::String("$g".into()))]);
    let count = doc(vec![("$count", Value::String("n".into()))]);
    let mut command = aggregate(vec![group, count]);
    command.push("allowDiskUse", true);
    let start = std::time::Instant::now();
    let reply = c.command(DB, command)?;
    let took = start.elapsed();
    let groups = first_batch_n(&reply);
    if groups != Some(records as f64) {
        return Err(format!("the $group gave {groups:?} groups of {records} documents"));
    }
    let status = c.command("admin", doc(vec![("serverStatus", Value::Int32(1))]))?;
    let host = c.command("admin", doc(vec![("hostInfo", Value::Int32(1))]))?;
    let resident = number_at(&status, "mem.resident").unwrap_or(0.0);
    let cores = number_at(&host, "system.numCores").unwrap_or(1.0).max(1.0);
    let mut report = Report::new("group-large");
    report.push("load_seconds", "s", loaded.as_secs_f64(), None);
    report.push("group_seconds", "s", took.as_secs_f64(), None);
    report.push(
        "resident_per_core",
        "MB",
        resident / cores,
        Some(crate::report::Target::AtMost(RESIDENT_PER_CORE_MB)),
    );
    report.notes.push(format!(
        "{records} documents of 1 KiB, one group each, target {}",
        args.target.as_deref().unwrap_or("local cluster of three nodes")
    ));
    Ok(report)
}

/// `n` of the first document of a reply's first batch.
fn first_batch_n(reply: &[u8]) -> Option<f64> {
    let doc = Document::from(teseladb_bson::RawDoc::from_bytes(reply).ok()?);
    let Some(Value::Document(cursor)) = doc.get("cursor") else { return None };
    let Some(Value::Array(batch)) = cursor.get("firstBatch") else { return None };
    let Some(Value::Document(first)) = batch.first() else { return None };
    match first.get("n") {
        Some(Value::Int32(n)) => Some(f64::from(*n)),
        Some(Value::Int64(n)) => Some(*n as f64),
        _ => None,
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn bytes(d: Document) -> Vec<u8> {
        d.to_bytes().expect("bson")
    }

    #[test]
    fn numbers_are_read_at_their_dotted_path() {
        let mem = doc(vec![("resident", Value::Int32(12)), ("virtual", Value::Int64(40))]);
        let status = bytes(doc(vec![
            ("mem", Value::Document(mem)),
            ("uptime", Value::Double(1.5)),
            ("host", Value::String("h".into())),
        ]));
        assert_eq!(number_at(&status, "mem.resident"), Some(12.0));
        assert_eq!(number_at(&status, "mem.virtual"), Some(40.0));
        assert_eq!(number_at(&status, "uptime"), Some(1.5));
        assert_eq!(number_at(&status, "host"), None, "not a number");
        assert_eq!(number_at(&status, "uptime.x"), None, "not a document on the way");
        assert_eq!(number_at(&status, "mem.nope"), None);
        assert_eq!(number_at(&status, ""), None);
        assert_eq!(number_at(&[1, 2], "mem"), None);
    }

    #[test]
    fn the_count_of_a_group_is_n_of_the_first_document() {
        let batch = |docs: Vec<Value>| {
            let cursor = doc(vec![("firstBatch", Value::Array(docs))]);
            bytes(doc(vec![("cursor", Value::Document(cursor))]))
        };
        let n = |v: Value| Value::Document(doc(vec![("n", v)]));
        assert_eq!(first_batch_n(&batch(vec![n(Value::Int32(3))])), Some(3.0));
        assert_eq!(first_batch_n(&batch(vec![n(Value::Int64(4))])), Some(4.0));
        assert_eq!(first_batch_n(&batch(vec![n(Value::Double(5.0))])), None);
        assert_eq!(first_batch_n(&batch(vec![Value::Int32(1)])), None);
        assert_eq!(first_batch_n(&batch(vec![])), None);
        let no_batch = bytes(doc(vec![("cursor", Value::Document(Document::new()))]));
        assert_eq!(first_batch_n(&no_batch), None);
        assert_eq!(first_batch_n(&bytes(doc(vec![("ok", Value::Int32(1))]))), None);
        assert_eq!(first_batch_n(&[1, 2]), None);
    }
}
