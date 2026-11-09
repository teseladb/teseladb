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

/// Only btree and `2dsphere` indexes track the paths through arrays (RefDb r9.0.4
/// `collection_impl.cpp`, `indexTypeSupportsPathLevelMultikeyTracking`): the `IXSCAN` of a
/// hashed index has no `multiKeyPaths`, a wildcard one does, and `$listCatalog` keeps
/// `multikeyPaths` for btree indexes only (measured on the native `refrouter` 9.0.4 on 09-11-2026).
#[test]
fn only_indexes_that_track_paths_through_arrays_show_them() {
    let mut m = Mem::new();
    for text in [
        r#"{"insert":"h","documents":[{"_id":1,"a":1,"t":"x"}]}"#,
        r#"{"createIndexes":"h","indexes":[{"key":{"a":"hashed"},"name":"a_hashed"},{"key":{"t":"text"},"name":"t_text"},{"key":{"$**":1},"name":"w"}]}"#,
    ] {
        let r = m.run("db", extjson::parse(text).expect("command"));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    }

    let hashed = explain(&mut m, r#"{"find":"h","filter":{"a":1},"hint":"a_hashed"}"#);
    let wildcard = explain(&mut m, r#"{"find":"h","filter":{"zz":1},"hint":"w"}"#);
    let catalog = m.run(
        "db",
        extjson::parse(r#"{"aggregate":"h","pipeline":[{"$listCatalog":{}}],"cursor":{}}"#)
            .expect("command"),
    );

    let paths = |r: &Document| {
        let plan = field(r, "queryPlanner.winningPlan");
        let ixscan = first_ixscan(plan).unwrap_or_else(|| panic!("no IXSCAN in {r:?}"));
        ixscan.get("multiKeyPaths").is_some()
    };
    assert!(!paths(&hashed), "{hashed:?}");
    assert!(paths(&wildcard), "{wildcard:?}");
    let Value::Array(docs) = field(&catalog, "cursor.firstBatch") else { panic!("{catalog:?}") };
    let Some(Value::Document(entry)) = docs.first() else { panic!("{catalog:?}") };
    let Value::Array(indexes) = field(entry, "md.indexes") else { panic!("{entry:?}") };
    let tracked: Vec<(String, bool)> = indexes
        .iter()
        .filter_map(|i| match i {
            Value::Document(i) => Some(i),
            _ => None,
        })
        .map(|i| match field(i, "spec.name") {
            Value::String(n) => (n.clone(), i.get("multikeyPaths").is_some()),
            other => panic!("{other:?}"),
        })
        .collect();
    let want = [("_id_", true), ("a_hashed", false), ("t_text", false), ("w", false)];
    assert_eq!(tracked, want.map(|(n, t)| (n.to_owned(), t)).to_vec());
}

/// The first `IXSCAN` stage under `v`, depth first.
fn first_ixscan(v: &Value) -> Option<&Document> {
    match v {
        Value::Document(d) if d.get("stage") == Some(&Value::String("IXSCAN".into())) => Some(d),
        Value::Document(d) => d.iter().find_map(|(_, v)| first_ixscan(v)),
        Value::Array(a) => a.iter().find_map(first_ixscan),
        _ => None,
    }
}
