//! `ingest` scenario (002 FR-034, SC-015): bulk ingestion over the wire protocol against
//! `--target` (TeselaDB or RefDb, with the same client) or a local cluster. `--concurrency`
//! connections each send unordered `insert`s of [`BATCH`] telemetry documents of about 120 bytes
//! (`{_id, sensor, ts, temperature, humidity, status}`) in a closed loop; the report gives the
//! documents per second and the latency of each batch.

use std::sync::atomic::{AtomicU64, Ordering};

use teseladb_bson::{Document, Value};

use crate::cli::Args;
use crate::client::{Client, Target, doc};
use crate::load::{Load, drive};
use crate::report::Report;

const DB: &str = "bench";
const COLLECTION: &str = "ingest";
/// Documents per `insert`.
pub const BATCH: u64 = 1000;
/// Distinct sensors the documents name.
const SENSORS: u64 = 10_000;

/// The `i`-th document: its `_id` is unique across the run's connections.
fn telemetry(i: u64) -> Value {
    Value::Document(doc(vec![
        ("_id", Value::Int64(i64::try_from(i).unwrap_or(i64::MAX))),
        ("sensor", Value::String(format!("sensor-{:05}", i % SENSORS))),
        ("ts", Value::Int64(1_700_000_000_000 + i64::try_from(i).unwrap_or(0))),
        ("temperature", Value::Double((i % 400) as f64 / 10.0)),
        ("humidity", Value::Double((i % 1000) as f64 / 10.0)),
        ("status", Value::String(if i.is_multiple_of(97) { "alarm" } else { "ok" }.to_owned())),
    ]))
}

/// The unordered insert of documents `from .. from + BATCH`.
fn batch(from: u64) -> Document {
    let docs = (from..from + BATCH).map(telemetry).collect();
    doc(vec![
        ("insert", Value::String(COLLECTION.into())),
        ("documents", Value::Array(docs)),
        ("ordered", Value::Boolean(false)),
    ])
}

pub fn run(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let addr = target.addr()?;
    let mut setup = Client::connect(addr)?;
    // The collection may not exist yet.
    let _ = setup.command(DB, doc(vec![("drop", Value::String(COLLECTION.into()))]));
    setup.until_ready(DB, doc(vec![("create", Value::String(COLLECTION.into()))]))?;
    let next = AtomicU64::new(0);
    let load = Load {
        warmup: args.warmup,
        duration: args.duration,
        concurrency: args.concurrency,
        rate: args.rate,
    };
    let out = drive(
        &load,
        |_| Client::connect(addr),
        |c| {
            let from = next.fetch_add(BATCH, Ordering::Relaxed);
            c.command(DB, batch(from))?;
            Ok("batch")
        },
    )?;
    let mut report = Report::new("ingest");
    for (kind, h) in &out.latencies {
        report.latency(kind, h, None, None);
    }
    report.push("documents", "doc/s", out.throughput() * BATCH as f64, None);
    report.push("errors", "op", out.errors as f64, None);
    if let Some(e) = &out.first_error {
        report.notes.push(format!("first error: {e}"));
    }
    report.notes.push(format!(
        "{} connections, unordered inserts of {BATCH} documents of about 120 bytes, target {}",
        args.concurrency.max(1),
        args.target.as_deref().unwrap_or("local cluster of three nodes")
    ));
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_batch_holds_unordered_unique_documents() {
        let b = batch(2000);
        let Some(Value::Array(docs)) = b.get("documents") else { panic!("documents") };
        assert_eq!(docs.len() as u64, BATCH);
        assert_eq!(b.get("ordered"), Some(&Value::Boolean(false)));
        let first = match &docs[0] {
            Value::Document(d) => d.get("_id").cloned(),
            _ => None,
        };
        assert_eq!(first, Some(Value::Int64(2000)));
    }
}
