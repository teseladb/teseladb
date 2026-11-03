//! `explain` of `count` and `distinct` over multikey indexes, with `executionStats` (010; the
//! stages and counters of a `refnode` 9.0.4, measured on 02-11-2026 on `mk` with
//! `{_id: 1, a: [1, 2], b: 1}`, `{_id: 2, a: 3, b: [4, 5]}`, `{_id: 3, a: 7, b: 2}` and indexes
//! on `a` and `b`): a range of a multikey index fetches the documents it counts, and `distinct`
//! of a multikey field reads them too.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

fn collection() -> Mem {
    let mut m = Mem::new();
    for text in [
        r#"{"insert":"mk","documents":[{"_id":1,"a":[1,2],"b":1},{"_id":2,"a":3,"b":[4,5]},{"_id":3,"a":7,"b":2}]}"#,
        r#"{"createIndexes":"mk","indexes":[{"key":{"a":1},"name":"a_1"},{"key":{"b":1},"name":"b_1"}]}"#,
    ] {
        let r = m.run("db", extjson::parse(text).expect("command"));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    }
    m
}

/// The stages of the winning plan of the only shard, first input after first.
fn chain(r: &Document) -> Vec<String> {
    let Value::Array(shards) = field(r, "queryPlanner.winningPlan.shards") else { panic!("{r:?}") };
    let Some(Value::Document(shard)) = shards.first() else { panic!("{r:?}") };
    let mut out = Vec::new();
    let mut at = shard.get("winningPlan").cloned();
    while let Some(Value::Document(stage)) = at {
        if let Some(Value::String(s)) = stage.get("stage") {
            out.push(s.clone());
        }
        at = match (stage.get("inputStage"), stage.get("inputStages")) {
            (Some(i), _) => Some(i.clone()),
            (None, Some(Value::Array(all))) => all.first().cloned(),
            _ => None,
        };
    }
    out
}

/// `totalKeysExamined` and `totalDocsExamined` of the run.
fn examined(r: &Document) -> (Value, Value) {
    (
        field(r, "executionStats.totalKeysExamined").clone(),
        field(r, "executionStats.totalDocsExamined").clone(),
    )
}

fn explain(m: &mut Mem, cmd: &str) -> Document {
    let text = format!(r#"{{"explain":{cmd},"verbosity":"executionStats"}}"#);
    let r = m.run("db", extjson::parse(&text).expect("command"));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    r
}

#[test]
fn a_range_of_a_multikey_index_fetches_what_it_counts() {
    let mut m = collection();
    let r = explain(&mut m, r#"{"count":"mk","query":{"a":{"$gt":1,"$lt":5}}}"#);
    assert_eq!(chain(&r), ["COUNT", "FETCH", "IXSCAN"]);
    assert_eq!(examined(&r), (Value::Int32(3), Value::Int32(2)));
    let r = m.run(
        "db",
        doc(&[("count", Value::from("mk")), ("query", field(&r, "command.query").clone())]),
    );
    assert_eq!(field(&r, "n"), &Value::Int32(2), "{r:?}");
}

#[test]
fn distinct_of_a_multikey_field_reads_the_documents() {
    let mut m = collection();
    let r = explain(&mut m, r#"{"distinct":"mk","key":"a","query":{"a":{"$gt":0}}}"#);
    assert_eq!(chain(&r), ["FETCH", "IXSCAN"]);
    assert_eq!(examined(&r), (Value::Int32(4), Value::Int32(3)));
}

/// An `$or` of two multikey indexes is counted under `SUBPLAN` from their keys alone: the `OR`
/// stage keeps one entry per document, so no document is read.
#[test]
fn an_or_of_multikey_indexes_counts_keys_only() {
    let mut m = collection();
    let r = explain(&mut m, r#"{"count":"mk","query":{"$or":[{"a":{"$gt":2}},{"b":{"$gt":4}}]}}"#);
    assert_eq!(chain(&r), ["COUNT", "SUBPLAN", "OR", "IXSCAN"]);
    assert_eq!(examined(&r), (Value::Int32(3), Value::Int32(0)));
    let r = m.run(
        "db",
        extjson::parse(r#"{"count":"mk","query":{"$or":[{"a":{"$gt":2}},{"b":{"$gt":4}}]}}"#)
            .expect("command"),
    );
    assert_eq!(field(&r, "n"), &Value::Int32(2), "{r:?}");
}

/// The `IXSCAN` of a multikey index under `executionStats`: what it tested, dropped and gave.
fn ixscan(r: &Document) -> (Value, Value, Value) {
    let Value::Array(shards) = field(r, "executionStats.executionStages.shards") else {
        panic!("{r:?}")
    };
    let Some(Value::Document(shard)) = shards.first() else { panic!("{r:?}") };
    let mut at = shard.get("executionStages").cloned();
    while let Some(Value::Document(stage)) = at {
        if stage.get("stage") == Some(&Value::from("IXSCAN")) {
            let get = |k: &str| stage.get(k).cloned().unwrap_or(Value::Null);
            return (get("dupsTested"), get("dupsDropped"), get("nReturned"));
        }
        at = stage.get("inputStage").cloned();
    }
    panic!("no IXSCAN in {r:?}")
}

/// A multikey scan tests each entry it reads for a document it gave and drops the repeats:
/// `{a: [1, 2]}` is given once (measured: `dupsTested` 3 and 4, `dupsDropped` 1).
#[test]
fn a_multikey_scan_drops_the_repeats_of_a_document() {
    let mut m = collection();
    let r = explain(&mut m, r#"{"count":"mk","query":{"a":{"$gt":1,"$lt":5}}}"#);
    assert_eq!(ixscan(&r), (Value::Int32(3), Value::Int32(1), Value::Int32(2)));
    let r = explain(&mut m, r#"{"find":"mk","filter":{"a":{"$gte":0}}}"#);
    assert_eq!(ixscan(&r), (Value::Int32(4), Value::Int32(1), Value::Int32(3)));
}
