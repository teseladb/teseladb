//! A comparison with a document scans the index between the documents' ends (`[{}, [])` for
//! `$gte: {}` on a `refrouter` 9.0.4, measured on 05-11-2026) and the index decides it alone: the
//! scan finds what reading the collection finds. Before, the planner scanned the whole index
//! and filtered the documents (battery's `sharding-key-compound`).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

/// The `_id`s `find` with `filter` gives, read through the index on `k` or the collection.
fn ids(m: &mut Mem, filter: &Document, hint: Value) -> Vec<Value> {
    let find = doc(&[
        ("find", "c".into()),
        ("filter", Value::Document(filter.clone())),
        ("sort", Value::Document(Document::new().with("_id", 1))),
        ("hint", hint),
    ]);
    let r = m.run("db", find);
    match field(&r, "cursor.firstBatch") {
        Value::Array(a) => a
            .iter()
            .map(|d| match d {
                Value::Document(d) => d.get("_id").cloned().expect("an _id"),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{r:?} {other:?}"),
    }
}

#[test]
fn a_comparison_with_a_document_scans_the_documents_of_the_index() {
    let mut m = Mem::new();
    let values = [
        Value::Int32(5),
        Value::String("s".into()),
        Value::Document(Document::new()),
        Value::Document(Document::new().with("a", 1)),
        Value::Document(Document::new().with("a", 2).with("b", 1)),
        Value::Document(Document::new().with("b", 0)),
        Value::Array(vec![Value::Int32(1)]),
        Value::Array(vec![Value::Document(Document::new().with("a", 3))]),
        Value::Boolean(true),
        Value::Null,
    ];
    let docs: Vec<Value> = values
        .iter()
        .enumerate()
        .map(|(i, v)| Value::Document(Document::new().with("_id", i as i32).with("k", v.clone())))
        .collect();
    let r = m.run("db", doc(&[("insert", "c".into()), ("documents", Value::Array(docs))]));
    assert_eq!(r.get("n"), Some(&Value::Int32(values.len() as i32)), "{r:?}");
    let k = Document::new().with("key", Document::new().with("k", 1)).with("name", "k_1");
    let r = m.run(
        "db",
        doc(&[("createIndexes", "c".into()), ("indexes", Value::Array(vec![k.into()]))]),
    );
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let index = Value::Document(Document::new().with("k", 1));
    let collection = Value::Document(Document::new().with("$natural", 1));
    for (op, arg) in [
        ("$gte", Document::new()),
        ("$gt", Document::new()),
        ("$gte", Document::new().with("a", 1)),
        ("$gt", Document::new().with("a", 1)),
        ("$lt", Document::new().with("b", 0)),
        ("$lte", Document::new().with("a", 2).with("b", 1)),
    ] {
        let filter = Document::new().with("k", Document::new().with(op, Value::Document(arg)));
        let scanned = ids(&mut m, &filter, index.clone());
        let read = ids(&mut m, &filter, collection.clone());
        assert_eq!(scanned, read, "{filter:?}");
        assert!(!read.is_empty(), "{filter:?}");
    }
}

/// An equality or an `$in` on a hashed field reads the hashed index by the values' hashes and
/// filters the documents, as a `refnode` 9.0.4 does (measured on 05-11-2026): the same documents
/// as reading the collection, and `explain` gives the hash as the bound.
#[test]
fn an_equality_on_a_hashed_field_scans_the_hashed_index() {
    let mut m = Mem::new();
    let docs: Vec<Value> =
        (0..50).map(|i| Value::Document(Document::new().with("_id", i).with("t", i % 7))).collect();
    let r = m.run("db", doc(&[("insert", "c".into()), ("documents", Value::Array(docs))]));
    assert_eq!(r.get("n"), Some(&Value::Int32(50)), "{r:?}");
    let k =
        Document::new().with("key", Document::new().with("t", "hashed")).with("name", "t_hashed");
    let r = m.run(
        "db",
        doc(&[("createIndexes", "c".into()), ("indexes", Value::Array(vec![k.into()]))]),
    );
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let collection = Value::Document(Document::new().with("$natural", 1));
    let index = Value::from("t_hashed");
    for filter in [
        Document::new().with("t", 2),
        Document::new()
            .with("t", Document::new().with("$in", vec![Value::Int32(5), Value::Int32(2)])),
    ] {
        let scanned = ids(&mut m, &filter, index.clone());
        let read = ids(&mut m, &filter, collection.clone());
        assert_eq!(scanned, read, "{filter:?}");
        assert!(!read.is_empty(), "{filter:?}");
    }
    let explain = doc(&[
        (
            "explain",
            Value::Document(doc(&[
                ("find", "c".into()),
                ("filter", Value::Document(Document::new().with("t", 2))),
            ])),
        ),
        ("verbosity", "queryPlanner".into()),
    ]);
    let r = m.run("db", explain);
    // Through the router: the plan of the one shard.
    let plan = match field(&r, "queryPlanner.winningPlan.shards") {
        Value::Array(shards) => field(plan_doc(&shards[0]), "winningPlan"),
        other => panic!("{other:?}"),
    };
    assert_eq!(field(plan_doc(plan), "stage"), &Value::from("FETCH"), "{r:?}");
    let scan = field(plan_doc(plan), "inputStage");
    assert_eq!(field(plan_doc(scan), "indexName"), &Value::from("t_hashed"), "{r:?}");
    let bounds = Value::Array(vec![Value::from("[848411777775835583, 848411777775835583]")]);
    assert_eq!(field(plan_doc(scan), "indexBounds.t"), &bounds, "{r:?}");
}

fn plan_doc(v: &Value) -> &Document {
    match v {
        Value::Document(d) => d,
        other => panic!("{other:?}"),
    }
}
