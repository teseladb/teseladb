//! `reference` (002 FR-020, SC-005): the fixed scenario of the hot path that the 10 % rule of
//! the baselines applies to, over the wire protocol against `--target` or a local cluster. It
//! measures a write confirmed by the majority with the journal (`insert`, `w: "majority"`,
//! `j: true`) and a read by `_id` served by the leader's lease (`find`, `readConcern: "local"`),
//! one after the other on each worker, on documents of 100 bytes. The query by index (009) and the
//! search (011) join it when the server answers `createIndexes` and `$search`; until then their
//! metrics are not in the report, so a baseline saved now stays valid.

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

fn insert(c: &mut Client, id: String, rng: &mut Rng) -> Result<(), String> {
    let document = doc(vec![("_id", Value::String(id)), ("v", Value::String(field(rng)))]);
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

/// Runs the scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let addr = target.addr()?;
    let mut setup = Client::connect(addr)?;
    // The collection may not exist yet.
    let _ = setup.command(DB, doc(vec![("drop", Value::String(COLLECTION.into()))]));
    let next = AtomicU64::new(0);
    let load = Load {
        warmup: args.warmup,
        duration: args.duration,
        concurrency: args.concurrency,
        rate: args.rate,
    };
    // Each call alternates: an odd call reads the document its worker wrote in the call before.
    let out = drive(
        &load,
        |w| Ok((Client::connect(addr)?, Rng::new(SEED + w as u64), None::<String>)),
        |(c, rng, written)| match written.take() {
            Some(id) => {
                read(c, id)?;
                Ok(READ)
            }
            None => {
                let id = key(next.fetch_add(1, Ordering::Relaxed));
                insert(c, id.clone(), rng)?;
                *written = Some(id);
                Ok(WRITE)
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
