//! The five profiles of 002 FR-037, which TeselaDB and RefDb run the same way on the same
//! machine, every write with `{w: "majority", j: true}` (FR-038): A (`profile-a`, YCSB A, half
//! reads and half updates by `_id`), B (`profile-b`, YCSB B, 95 % reads), C (`profile-c`, inserts
//! only, one in two alone and the rest in batches), D (`profile-d`, `$match`, `$group`, `$sort` and
//! `$project` over `--records` documents, 100 million for the comparison) and E (`profile-e`, YCSB
//! B over data of at least eight times the node's memory, which the report checks).

use std::time::Instant;

use teseladb_bson::{Document, Value};
use teseladb_types::Histogram;

use super::query::number_at;
use crate::cli::Args;
use crate::client::{Client, Target, check, doc, durable};
use crate::report::Report;

const DB: &str = "bench";
const COLLECTION: &str = "profile_d";
/// Documents per insert while loading profile D.
const LOAD_BATCH: i64 = 1000;
/// Values of `g` in profile D.
const GROUPS: i64 = 1000;
/// Least ratio of data to the node's memory that profile E measures (FR-037).
pub const E_DATA_OVER_MEMORY: f64 = 8.0;

/// Profile A.
pub fn a(args: &Args) -> Result<Report, String> {
    renamed(super::ycsb::run(args, 'a')?, "profile-a")
}

/// Profile B.
pub fn b(args: &Args) -> Result<Report, String> {
    renamed(super::ycsb::run(args, 'b')?, "profile-b")
}

/// Profile C.
pub fn c(args: &Args) -> Result<Report, String> {
    super::ingest::run_mixed(args)
}

fn renamed(mut r: Report, name: &str) -> Result<Report, String> {
    r.scenario = name.to_owned();
    Ok(r)
}

/// The pipeline of profile D: documents with `n` from a threshold, grouped by `g` with their sum
/// and average, the groups by sum downwards, and only those three fields.
pub fn pipeline_d(threshold: i64) -> Vec<Value> {
    let gte = doc(vec![("$gte", Value::Int64(threshold))]);
    let matching = doc(vec![("$match", Value::Document(doc(vec![("n", Value::Document(gte))])))]);
    let sum = doc(vec![("$sum", Value::String("$n".into()))]);
    let avg = doc(vec![("$avg", Value::String("$n".into()))]);
    let group = doc(vec![(
        "$group",
        Value::Document(doc(vec![
            ("_id", Value::String("$g".into())),
            ("total", Value::Document(sum)),
            ("mean", Value::Document(avg)),
        ])),
    )]);
    let sort = doc(vec![("$sort", Value::Document(doc(vec![("total", Value::Int32(-1))])))]);
    let fields =
        vec![("_id", Value::Int32(1)), ("total", Value::Int32(1)), ("mean", Value::Int32(1))];
    let project = doc(vec![("$project", Value::Document(doc(fields)))]);
    [matching, group, sort, project].into_iter().map(Value::Document).collect()
}

/// Profile D: loads `--records` documents `{_id, g, n, ts}` (unless `--skip-load`), then runs the
/// pipeline once per iteration for `--duration`, after `--warmup`.
pub fn d(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let mut c = Client::connect(target.addr()?)?;
    let records = i64::try_from(args.records).unwrap_or(i64::MAX);
    let loading = Instant::now();
    if !args.skip_load {
        let _ = c.command(DB, doc(vec![("drop", Value::String(COLLECTION.into()))]));
        let mut next = 0;
        while next < records {
            let docs = (next..(next + LOAD_BATCH).min(records))
                .map(|i| {
                    Value::Document(doc(vec![
                        ("_id", Value::Int64(i)),
                        ("g", Value::Int64(i % GROUPS)),
                        ("n", Value::Int64(i % 10_000)),
                        ("ts", Value::Int64(1_700_000_000_000 + i)),
                    ]))
                })
                .collect();
            next += LOAD_BATCH;
            let insert = doc(vec![
                ("insert", Value::String(COLLECTION.into())),
                ("documents", Value::Array(docs)),
                ("ordered", Value::Boolean(false)),
            ]);
            check(&c.command(DB, durable(insert))?)?;
        }
    }
    let loaded = loading.elapsed();
    let command = doc(vec![
        ("aggregate", Value::String(COLLECTION.into())),
        ("pipeline", Value::Array(pipeline_d(5_000))),
        ("cursor", Value::Document(Document::new())),
        ("allowDiskUse", Value::Boolean(true)),
    ]);
    let warm_until = Instant::now() + args.warmup;
    while Instant::now() < warm_until {
        check(&c.command(DB, command.clone())?)?;
    }
    let mut h = Histogram::new();
    let until = Instant::now() + args.duration;
    let mut runs = 0u64;
    while Instant::now() < until || runs == 0 {
        let start = Instant::now();
        check(&c.command(DB, command.clone())?)?;
        h.record(u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX));
        runs += 1;
    }
    let mut report = Report::new("profile-d");
    report.latency("pipeline", &h, None, None);
    report.push("runs", "op", runs as f64, None);
    report.push("load_seconds", "s", loaded.as_secs_f64(), None);
    report.notes.push(format!(
        "{records} documents, $match on half of them, $group in {GROUPS} groups, $sort and $project, target {}",
        args.target.as_deref().unwrap_or("local cluster of three nodes")
    ));
    Ok(report)
}

/// Profile E: YCSB B, with the ratio of the data to the node's resident memory checked after the
/// run; under [`E_DATA_OVER_MEMORY`] the report says the run does not measure profile E.
pub fn e(args: &Args) -> Result<Report, String> {
    let mut report = renamed(super::ycsb::run(args, 'b')?, "profile-e")?;
    let target = Target::open(args.target.as_deref())?;
    let mut c = Client::connect(target.addr()?)?;
    let stats = c.command("ycsb", doc(vec![("dbStats", Value::Int32(1))]))?;
    let status = c.command("admin", doc(vec![("serverStatus", Value::Int32(1))]))?;
    let data = number_at(&stats, "dataSize").unwrap_or(0.0);
    // `mem.resident` is in MiB.
    let memory = number_at(&status, "mem.resident").unwrap_or(0.0) * 1_048_576.0;
    let ratio = if memory > 0.0 { data / memory } else { 0.0 };
    report.push(
        "data_over_memory",
        "x",
        ratio,
        Some(crate::report::Target::AtLeast(E_DATA_OVER_MEMORY)),
    );
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn profile_d_matches_groups_sorts_and_projects() {
        let stages: Vec<String> = pipeline_d(10)
            .iter()
            .map(|v| match v {
                Value::Document(d) => d.iter().next().unwrap().0.to_owned(),
                _ => panic!("a stage"),
            })
            .collect();
        assert_eq!(stages, ["$match", "$group", "$sort", "$project"]);
    }

    #[test]
    fn every_write_of_the_bench_asks_for_majority_and_the_journal() {
        let d = durable(doc(vec![("insert", Value::String("x".into()))]));
        let Some(Value::Document(wc)) = d.get("writeConcern") else { panic!("writeConcern") };
        assert_eq!(wc.get("w"), Some(&Value::String("majority".into())));
        assert_eq!(wc.get("j"), Some(&Value::Boolean(true)));
    }
}
