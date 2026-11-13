//! Open-loop scenarios (002 FR-035): requests at `--rate` per second from `--concurrency`
//! connections over a few threads of the generator of `open_loop`, with their latency counted from
//! the instant each was due. `open-read` reads by `_id` the records YCSB's load phase leaves
//! (`--records`, loaded unless `--skip-load`); `open-insert` inserts one telemetry document per
//! request with `{w: "majority", j: true}`. The report says whether the client kept the rate: a
//! measure where it did not is not valid.

use teseladb_bson::{Document, RawDoc, RawValue, Value};

use super::ycsb_gen::key;
use crate::cli::Args;
use crate::client::{Client, Target, doc, durable};
use crate::open_loop::{self, Plan};
use crate::report::{Report, Target as Goal};

/// Threads of the generator: each drives its share of the connections.
const THREADS: usize = 4;

fn body(mut command: Document, db: &str) -> Vec<u8> {
    command.insert("$db", Value::String(db.to_owned()));
    command.to_bytes().unwrap_or_default()
}

/// Whether a reply says `ok: 1`.
fn ok(reply: &[u8]) -> bool {
    RawDoc::from_bytes(reply).ok().and_then(|d| d.get("ok")).is_some_and(|v| match v {
        RawValue::Double(n) => n == 1.0,
        RawValue::Int32(n) => n == 1,
        RawValue::Int64(n) => n == 1,
        _ => false,
    })
}

fn plan(args: &Args) -> Result<Plan, String> {
    let rate = args.rate.ok_or("an open-loop scenario needs --rate")?;
    Ok(Plan {
        rate,
        threads: THREADS.min(args.concurrency.max(1)),
        connections: args.concurrency.max(1),
        warmup: args.warmup,
        duration: args.duration,
    })
}

fn report(name: &str, args: &Args, m: &open_loop::Measured) -> Report {
    let mut r = Report::new(name);
    r.latency("op", &m.latency, None, None);
    r.push(
        "sent_rate",
        "op/s",
        m.sent_rate,
        Some(Goal::AtLeast(args.rate.unwrap_or(0.0) * open_loop::KEPT_RATE)),
    );
    r.push("answered", "op", m.answered as f64, None);
    r.push("failed", "op", m.failed as f64, None);
    if !m.kept_rate {
        r.notes.push("the client did not keep the rate: this measure is not valid".into());
    }
    r.notes.push(format!(
        "open loop at {} op/s over {} connections, target {}",
        args.rate.unwrap_or(0.0),
        args.concurrency.max(1),
        args.target.as_deref().unwrap_or("three local nodes")
    ));
    r
}

/// `open-read`.
pub fn read(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let addr = target.addr()?;
    if !args.skip_load {
        let load_only = Args {
            duration: std::time::Duration::ZERO,
            warmup: std::time::Duration::ZERO,
            ..args.clone()
        };
        super::ycsb::run_on(&load_only, 'c', &target)?;
    }
    let records = args.records.max(1);
    let request = move |i: u64| {
        let filter =
            doc(vec![("_id", Value::String(key(i.wrapping_mul(2_654_435_761) % records)))]);
        body(
            doc(vec![
                ("find", Value::String("usertable".into())),
                ("filter", Value::Document(filter)),
                ("limit", Value::Int64(1)),
                ("singleBatch", Value::Boolean(true)),
            ]),
            "ycsb",
        )
    };
    let m = open_loop::run(addr, plan(args)?, &request, &ok)?;
    Ok(report("open-read", args, &m))
}

/// `open-insert`.
pub fn insert(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let addr = target.addr()?;
    let mut setup = Client::connect(addr)?;
    let _ = setup.command("bench", doc(vec![("drop", Value::String("open_insert".into()))]));
    setup.until_ready("bench", doc(vec![("create", Value::String("open_insert".into()))]))?;
    let request = |i: u64| {
        let one = doc(vec![
            ("_id", Value::Int64(i64::try_from(i).unwrap_or(i64::MAX))),
            ("sensor", Value::String(format!("sensor-{:05}", i % 10_000))),
            ("v", Value::Double((i % 400) as f64 / 10.0)),
        ]);
        let insert = doc(vec![
            ("insert", Value::String("open_insert".into())),
            ("documents", Value::Array(vec![Value::Document(one)])),
        ]);
        body(durable(insert), "bench")
    };
    let m = open_loop::run(addr, plan(args)?, &request, &ok)?;
    Ok(report("open-insert", args, &m))
}
