//! Stages that pass their memory write to the node's temporary store (010 T024, FR-010): a
//! `$sort` over more than its limit sorts in runs it merges back with `allowDiskUse`, and fails
//! with 292 without it, as a `refnode` 9.0.4 with the limit set does.

mod common;

use std::sync::Arc;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};
use teseladb_exec::pipeline::spill::MemorySpill;

fn cmd(text: &str) -> Document {
    extjson::parse(text).expect("a command")
}

fn node(store: &Arc<MemorySpill>) -> Mem {
    let mut m = Mem::new();
    let docs: Vec<String> = (0..60)
        .map(|i| format!(r#"{{"_id":{i},"k":{},"s":"{}"}}"#, (i * 37) % 60, "x".repeat(100)))
        .collect();
    let r = m.run("sp", cmd(&format!(r#"{{"insert":"c","documents":[{}]}}"#, docs.join(","))));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let mut g = m.shared.lock().unwrap();
    g.spill.store = Some(store.clone());
    g.spill.stage_bytes = Some(2_000);
    drop(g);
    m
}

fn ids(r: &Document) -> Vec<i64> {
    let Value::Array(b) = field(r, "cursor.firstBatch") else { panic!("{r:?}") };
    b.iter()
        .map(|d| match d {
            Value::Document(d) => match d.get("k") {
                Some(Value::Int32(n)) => i64::from(*n),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn a_sort_past_its_memory_goes_through_the_store() {
    let store = Arc::new(MemorySpill::default());
    let mut m = node(&store);
    let r = m.run(
        "sp",
        cmd(r#"{"aggregate":"c","pipeline":[{"$sort":{"k":-1}}],"cursor":{"batchSize":100},"allowDiskUse":true}"#),
    );
    assert_eq!(ids(&r), (0..60).rev().collect::<Vec<i64>>());
    assert!(store.written() >= 3, "{}", store.written());
    assert!(store.is_empty());
    let r = m.run(
        "sp",
        cmd(r#"{"aggregate":"c","pipeline":[{"$sort":{"k":1}}],"cursor":{},"allowDiskUse":false}"#),
    );
    assert_eq!(field(&r, "code"), &Value::Int32(292), "{r:?}");
    let msg = "Sort exceeded memory limit of 2000 bytes, but did not opt in to external sorting.";
    assert!(matches!(field(&r, "errmsg"), Value::String(s) if s.ends_with(msg)), "{r:?}");
}

#[test]
fn a_small_sort_stays_in_memory() {
    let store = Arc::new(MemorySpill::default());
    let mut m = node(&store);
    let r = m.run(
        "sp",
        cmd(r#"{"aggregate":"c","pipeline":[{"$limit":5},{"$sort":{"k":1}}],"cursor":{},"allowDiskUse":true}"#),
    );
    let ok = doc(&[("ok", Value::Double(1.0))]);
    assert_eq!(field(&r, "ok"), field(&ok, "ok"), "{r:?}");
    assert_eq!(store.written(), 0);
}

#[test]
fn a_group_past_its_memory_goes_through_the_store_in_key_order() {
    let store = Arc::new(MemorySpill::default());
    let mut m = node(&store);
    let r = m.run(
        "sp",
        cmd(r#"{"aggregate":"c","pipeline":[{"$group":{"_id":"$k","p":{"$push":"$s"}}},{"$project":{"k":"$_id"}}],"cursor":{"batchSize":100},"allowDiskUse":true}"#),
    );
    assert_eq!(ids(&r), (0..60).collect::<Vec<i64>>());
    assert!(store.written() >= 3, "{}", store.written());
    assert!(store.is_empty());
    let r = m.run(
        "sp",
        cmd(r#"{"aggregate":"c","pipeline":[{"$group":{"_id":"$k","p":{"$push":"$s"}}}],"cursor":{},"allowDiskUse":false}"#),
    );
    assert_eq!(field(&r, "code"), &Value::Int32(292), "{r:?}");
}

/// The stage of an explain's `stages` whose first field is `name`.
fn stage<'a>(r: &'a Document, name: &str) -> &'a Document {
    let Value::Array(list) = field(r, "shards.node1.stages") else { panic!("{r:?}") };
    list.iter()
        .find_map(|s| match s {
            Value::Document(d) if d.iter().next().is_some_and(|(k, _)| k == name) => Some(d),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no {name} in {r:?}"))
}

#[test]
fn explain_counts_what_a_stage_wrote_to_disk() {
    let store = Arc::new(MemorySpill::default());
    let mut m = node(&store);
    let explain = |pipeline: &str, disk: bool| {
        cmd(&format!(
            r#"{{"explain":{{"aggregate":"c","pipeline":{pipeline},"cursor":{{}},"allowDiskUse":{disk}}},"verbosity":"executionStats"}}"#
        ))
    };
    let r = m.run("sp", explain(r#"[{"$bucketAuto":{"groupBy":"$k","buckets":3}}]"#, true));
    let s = stage(&r, "$bucketAuto");
    let keys: Vec<&str> = s.iter().map(|(k, _)| k).collect();
    assert_eq!(
        keys,
        [
            "$bucketAuto",
            "nReturned",
            "executionTimeMillisEstimate",
            "usedDisk",
            "spills",
            "spilledDataStorageSize",
            "spilledBytes",
            "spilledRecords"
        ]
    );
    assert_eq!(s.get("usedDisk"), Some(&Value::Boolean(true)), "{s:?}");
    assert_eq!(s.get("spilledRecords"), Some(&Value::Int64(60)), "{s:?}");
    let window = r#"[{"$setWindowFields":{"partitionBy":"$k","sortBy":{"_id":1},"output":{"r":{"$rank":{}}}}}]"#;
    let r = m.run("sp", explain(window, true));
    let s = stage(&r, "$_internalSetWindowFields");
    assert_eq!(s.get("usedDisk"), Some(&Value::Boolean(true)), "{s:?}");
    assert_eq!(
        s.get("maxFunctionMemoryUsageBytes"),
        Some(&Value::Document(Document::new().with("r", Value::Int64(0))))
    );
    let r = m.run("sp", explain(r#"[{"$bucketAuto":{"groupBy":"$k","buckets":3}}]"#, false));
    assert_eq!(field(&r, "code"), &Value::Int32(292), "{r:?}");
    let r = m
        .run("sp", explain(r#"[{"$limit":3},{"$bucketAuto":{"groupBy":"$k","buckets":3}}]"#, true));
    let s = stage(&r, "$bucketAuto");
    assert_eq!(s.get("usedDisk"), Some(&Value::Boolean(false)), "{s:?}");
    assert_eq!(s.get("spills"), Some(&Value::Int64(0)), "{s:?}");
    assert!(store.is_empty());
}

#[test]
fn query_stats_counts_a_query_that_wrote_to_disk() {
    let store = Arc::new(MemorySpill::default());
    let mut m = node(&store);
    for p in [
        r#"{"setParameter":1,"internalQueryStatsRateLimit":-1}"#,
        r#"{"setParameter":1,"internalQueryStatsSampleRate":0.0}"#,
    ] {
        let r = m.run("admin", cmd(p));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    }
    m.run("sp", cmd(r#"{"aggregate":"c","pipeline":[{"$sort":{"k":-1}}],"cursor":{"batchSize":100},"allowDiskUse":true}"#));
    m.run("sp", cmd(r#"{"aggregate":"c","pipeline":[{"$limit":2}],"cursor":{}}"#));
    let r = m.run("admin", cmd(r#"{"aggregate":1,"pipeline":[{"$queryStats":{}}],"cursor":{}}"#));
    let Value::Array(entries) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    let used: Vec<(bool, bool)> = entries
        .iter()
        .map(|e| match e {
            Value::Document(d) => (
                format!("{:?}", d.get("key")).contains("$sort"),
                field(d, "metrics.queryPlanner.usedDisk.true") == &Value::Int64(1),
            ),
            other => panic!("{other:?}"),
        })
        .collect();
    assert!(used.contains(&(true, true)), "{used:?}");
    assert!(used.contains(&(false, false)), "{used:?}");
}
