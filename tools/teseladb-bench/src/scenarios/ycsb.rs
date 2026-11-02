//! `ycsb-a` to `ycsb-f` (002 FR-018, FR-019): the YCSB workloads over the wire protocol against
//! `--target` (TeselaDB or RefDb, with the same client) or against a local cluster. The load
//! phase inserts `--records` records of ten fields of 100 bytes; the timed phase runs the
//! workload's mix with `--concurrency` connections, in a closed loop or at `--rate`.

use std::sync::atomic::{AtomicU64, Ordering};

use teseladb_bson::{Document, Value};

use super::ycsb_gen::{FIELDS, KeyChooser, MAX_SCAN, Op, Rng, Workload, field, key};
use crate::cli::Args;
use crate::client::{Client, Target, doc};
use crate::load::{Load, drive};
use crate::report::Report;

/// Database and collection of the records.
const DB: &str = "ycsb";
const COLLECTION: &str = "usertable";
/// Records per insert during the load phase.
const LOAD_BATCH: u64 = 1000;
/// Seed of the workers' sources (worker `w` uses `SEED + w`).
const SEED: u64 = 0x5943_5342;

pub(crate) fn record(i: u64, rng: &mut Rng) -> Value {
    Value::Document(record_doc(i, rng))
}

/// Record `i`: its key and ten fields of 100 bytes.
pub(crate) fn record_doc(i: u64, rng: &mut Rng) -> Document {
    let mut pairs = vec![("_id", Value::String(key(i)))];
    let names: Vec<String> = (0..FIELDS).map(|f| format!("field{f}")).collect();
    for n in &names {
        pairs.push((n.as_str(), Value::String(field(rng))));
    }
    doc(pairs)
}

fn insert(c: &mut Client, docs: Vec<Value>) -> Result<(), String> {
    c.command(DB, insert_command(docs)).map(drop)
}

/// The insert of `docs` into the YCSB collection.
fn insert_command(docs: Vec<Value>) -> Document {
    doc(vec![("insert", Value::String(COLLECTION.into())), ("documents", Value::Array(docs))])
}

/// Inserts `docs` into the YCSB collection of a node that may still be electing its leaders.
pub(crate) fn insert_when_ready(c: &mut Client, docs: Vec<Value>) -> Result<(), String> {
    c.until_ready(DB, insert_command(docs))
}

fn find(c: &mut Client, filter: Value, limit: u64) -> Result<(), String> {
    let mut pairs = vec![("find", Value::String(COLLECTION.into())), ("filter", filter)];
    pairs.push(("sort", Value::Document(doc(vec![("_id", Value::Int32(1))]))));
    pairs.push(("limit", Value::Int64(limit as i64)));
    c.command(DB, doc(pairs)).map(drop)
}

fn by_id(k: String) -> Value {
    Value::Document(doc(vec![("_id", Value::String(k))]))
}

fn update(c: &mut Client, k: String, rng: &mut Rng) -> Result<(), String> {
    let name = format!("field{}", rng.below(FIELDS as u64));
    let set = doc(vec![(name.as_str(), Value::String(field(rng)))]);
    let u = doc(vec![("$set", Value::Document(set))]);
    let one = doc(vec![("q", by_id(k)), ("u", Value::Document(u))]);
    c.command(
        DB,
        doc(vec![
            ("update", Value::String(COLLECTION.into())),
            ("updates", Value::Array(vec![Value::Document(one)])),
        ]),
    )
    .map(drop)
}

/// Runs workload `letter`.
pub fn run(args: &Args, letter: char) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let (out, notes) = run_on(args, letter, &target)?;
    let mut report = Report::new(&format!("ycsb-{letter}"));
    for (kind, h) in &out.latencies {
        report.latency(kind, h, None, None);
    }
    report.push("throughput", "op/s", out.throughput(), None);
    report.push("errors", "op", out.errors as f64, None);
    report.notes.extend(notes);
    report.notes.push(format!(
        "target {}",
        args.target.as_deref().unwrap_or("local cluster of three nodes")
    ));
    Ok(report)
}

/// Loads the records and runs workload `letter` against `target`: what was measured and the
/// notes that describe it.
pub fn run_on(
    args: &Args,
    letter: char,
    target: &Target,
) -> Result<(crate::load::Outcome, Vec<String>), String> {
    let records = args.records;
    let workload =
        Workload::new(letter, records).ok_or(format!("unknown YCSB workload {letter}"))?;
    let addr = target.addr()?;
    let mut setup = Client::connect(addr)?;
    // The collection may not exist yet.
    let _ = setup.command(DB, doc(vec![("drop", Value::String(COLLECTION.into()))]));
    let mut rng = Rng::new(SEED);
    let mut i = 0;
    while i < records {
        let n = LOAD_BATCH.min(records - i);
        insert(&mut setup, (i..i + n).map(|j| record(j, &mut rng)).collect())?;
        i += n;
    }
    let inserted = AtomicU64::new(records);
    let load = Load {
        warmup: args.warmup,
        duration: args.duration,
        concurrency: args.concurrency,
        rate: args.rate,
    };
    let out = drive(
        &load,
        |w| Ok((Client::connect(addr)?, Rng::new(SEED + 1 + w as u64))),
        |(c, rng)| {
            let op = workload.next_op(rng);
            let n = inserted.load(Ordering::Relaxed);
            let k = key(workload.keys.next(rng, n));
            match op {
                Op::Read => find(c, by_id(k), 1)?,
                Op::Update => update(c, k, rng)?,
                Op::Insert => {
                    let next = inserted.fetch_add(1, Ordering::Relaxed);
                    insert(c, vec![record(next, rng)])?;
                }
                Op::Scan => {
                    let from = Value::Document(doc(vec![("$gte", Value::String(k))]));
                    let filter = Value::Document(doc(vec![("_id", from)]));
                    find(c, filter, 1 + rng.below(MAX_SCAN))?;
                }
                Op::ReadModifyWrite => {
                    find(c, by_id(k.clone()), 1)?;
                    update(c, k, rng)?;
                }
            }
            Ok(op.name())
        },
    )?;
    let mut notes = Vec::new();
    if let Some(e) = &out.first_error {
        notes.push(format!("first error: {e}"));
    }
    let dist = match workload.keys {
        KeyChooser::ScrambledZipfian(_) => "scrambled zipfian",
        KeyChooser::Latest(_) => "latest",
    };
    notes.push(format!(
        "{records} records, {} workers, {}, keys {dist}",
        args.concurrency.max(1),
        args.rate.map_or("closed loop".to_owned(), |r| format!("{r} op/s")),
    ));
    Ok((out, notes))
}
