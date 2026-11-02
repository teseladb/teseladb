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
