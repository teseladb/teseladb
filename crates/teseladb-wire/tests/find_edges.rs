//! `find` with a `$natural` sort and with computed projections (001 FR-011, 010): a backward
//! `$natural` reverses the order of the collection, a fractional one is refused, and a
//! projection computes `$`-paths, keeps literals and goes into sub-documents.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

/// `BadValue`.
const BAD_VALUE: i32 = 2;

fn collection() -> Mem {
    let mut m = Mem::new();
    let docs: Vec<Value> = (1..=3)
        .map(|i| {
            let o = Value::Document(doc(&[("k", Value::Int32(i)), ("v", Value::Int32(10 * i))]));
            Value::Document(doc(&[("_id", Value::Int32(i)), ("a", Value::Int32(i)), ("o", o)]))
        })
        .collect();
    let r = m.run("db", doc(&[("insert", Value::from("c")), ("documents", Value::Array(docs))]));
    assert_eq!(r.get("n"), Some(&Value::Int32(3)), "{r:?}");
    m
}

fn find(m: &mut Mem, extra: &[(&str, Value)]) -> Document {
    let mut cmd = doc(&[("find", Value::from("c"))]);
    for (k, v) in extra {
        cmd.push(*k, v.clone());
    }
    m.run("db", cmd)
}

fn natural(v: Value) -> (&'static str, Value) {
    ("sort", Value::Document(doc(&[("$natural", v)])))
}

#[test]
fn a_backward_natural_sort_reverses_and_a_fractional_one_is_refused() {
    let mut m = collection();
    let forward = find(&mut m, &[natural(Value::Double(1.0))]);
    let backward = find(&mut m, &[natural(Value::Int32(-1))]);
    let ids = |r: &Document| -> Vec<Value> {
        (0..3).map(|i| field(r, &format!("cursor.firstBatch.{i}._id")).clone()).collect()
    };
    let mut reversed = ids(&forward);
    reversed.reverse();
    assert_eq!(ids(&backward), reversed);
    let r = find(&mut m, &[natural(Value::Double(1.5))]);
    assert_eq!(r.get("code"), Some(&Value::Int32(BAD_VALUE)), "{r:?}");
}

#[test]
fn a_projection_computes_paths_keeps_literals_and_goes_into_sub_documents() {
    let mut m = collection();
    let projection = doc(&[
        ("_id", Value::Int32(0)),
        ("copy", Value::from("$a")),
        ("label", Value::from("x")),
        ("o", Value::Document(doc(&[("k", Value::Int32(1))]))),
    ]);
    let filter = doc(&[("_id", Value::Int32(2))]);
    let r = find(
        &mut m,
        &[("filter", Value::Document(filter)), ("projection", Value::Document(projection))],
    );
    let Value::Document(d) = field(&r, "cursor.firstBatch.0") else { panic!("{r:?}") };
    assert_eq!(d.get("copy"), Some(&Value::Int32(2)), "{d:?}");
    assert_eq!(d.get("label"), Some(&Value::from("x")), "{d:?}");
    assert_eq!(field(d, "o.k"), &Value::Int32(2));
    assert!(matches!(d.get("o"), Some(Value::Document(o)) if o.get("v").is_none()), "{d:?}");
}

/// A `find` with `$text` keeps its `batchSize`, `comment` and `maxTimeMS` (the aggregation it
/// runs as takes them): two matches in batches of one.
#[test]
fn a_text_find_keeps_its_batch_size_and_options() {
    let mut m = Mem::new();
    let json = |t: &str| teseladb_bson::extjson::parse(t).expect("command");
    m.run(
        "tx",
        json(r#"{"insert":"c","documents":[{"_id":1,"s":"red fox"},{"_id":2,"s":"red hen"}]}"#),
    );
    m.run("tx", json(r#"{"createIndexes":"c","indexes":[{"key":{"s":"text"},"name":"s_text"}]}"#));
    let r = m.run(
        "tx",
        json(r#"{"find":"c","filter":{"$text":{"$search":"red"}},"batchSize":1,"comment":"c","maxTimeMS":5000}"#),
    );
    let Value::Array(first) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    assert_eq!(first.len(), 1, "{r:?}");
    assert_ne!(field(&r, "cursor.id"), &Value::Int64(0), "{r:?}");
}
