//! The part of a `$match` after `$changeStream` that goes down to the reads of the ranges (014
//! FR-012): the namespaces and the event types it allows decide which collections' ranges a read
//! asks; the whole `$match` still runs on each event.

use teseladb_bson::{Document, Value};
use teseladb_changestream::pushdown::Pushdown;

fn d(pairs: &[(&str, Value)]) -> Document {
    let mut doc = Document::new();
    for (k, v) in pairs {
        doc.push(*k, v.clone());
    }
    doc
}

fn m(filter: Document) -> Vec<Document> {
    vec![d(&[("$match", Value::Document(filter))])]
}

fn strings(v: &[&str]) -> Value {
    Value::Array(v.iter().map(|s| Value::from(*s)).collect())
}

#[test]
fn a_match_on_the_collection_reads_only_its_ranges() {
    let p = Pushdown::of(&m(d(&[("ns.coll", "a".into())])));
    assert!(p.reads("t", "a") && !p.reads("t", "b"));
    let p = Pushdown::of(&m(d(&[("ns.coll", d(&[("$in", strings(&["a", "b"]))]).into())])));
    assert!(p.reads("t", "b") && !p.reads("t", "c"));
    let p = Pushdown::of(&m(d(&[("ns.coll", d(&[("$eq", "a".into())]).into())])));
    assert!(p.reads("x", "a") && !p.reads("x", "c"));
    let p = Pushdown::of(&m(d(&[("ns", d(&[("db", "t".into()), ("coll", "a".into())]).into())])));
    assert!(p.reads("t", "a") && !p.reads("u", "a") && !p.reads("t", "b"));
    let p = Pushdown::of(&m(d(&[("ns.db", "t".into())])));
    assert!(p.reads("t", "z") && !p.reads("u", "z"));
    // Inside `$and`, every condition holds.
    let and = strings(&[]);
    let both = Value::Array(vec![
        d(&[("ns.db", "t".into())]).into(),
        d(&[("ns.coll", "a".into())]).into(),
    ]);
    let p = Pushdown::of(&m(d(&[("$and", both)])));
    assert!(p.reads("t", "a") && !p.reads("u", "a") && !p.reads("t", "b"));
    let p = Pushdown::of(&m(d(&[("$and", and)])));
    assert!(p.reads("t", "a"));
}

#[test]
fn a_match_on_catalog_events_only_reads_no_documents() {
    let p = Pushdown::of(&m(d(&[("operationType", "drop".into())])));
    assert!(!p.reads_documents());
    let p = Pushdown::of(&m(d(&[(
        "operationType",
        d(&[("$in", strings(&["drop", "rename"]))]).into(),
    )])));
    assert!(!p.reads_documents());
    let p = Pushdown::of(&m(d(&[(
        "operationType",
        d(&[("$in", strings(&["drop", "insert"]))]).into(),
    )])));
    assert!(p.reads_documents());
    let p = Pushdown::of(&m(d(&[("operationType", "update".into())])));
    assert!(p.reads_documents());
}

#[test]
fn what_the_ranges_cannot_decide_reads_everything() {
    for filter in [
        d(&[("ns.coll", d(&[("$regex", "^a".into())]).into())]),
        d(&[("ns.coll", 5.into())]),
        d(&[("ns.coll", d(&[("$eq", "a".into()), ("$ne", "b".into())]).into())]),
        d(&[("ns.coll", d(&[("$in", Value::Array(vec![5.into()]))]).into())]),
        d(&[("ns", "t.a".into())]),
        d(&[("ns", d(&[("db", "t".into())]).into())]),
        d(&[("ns", d(&[("db", 1.into()), ("coll", "a".into())]).into())]),
        d(&[("operationType", d(&[("$ne", "drop".into())]).into())]),
        d(&[("operationType", 3.into())]),
        d(&[("$or", Value::Array(vec![d(&[("ns.coll", "a".into())]).into()]))]),
        d(&[("$and", "x".into())]),
        d(&[("$and", Value::Array(vec![1.into()]))]),
        d(&[("fullDocument.a", 1.into())]),
    ] {
        let p = Pushdown::of(&m(filter.clone()));
        assert!(p.reads("t", "b") && p.reads_documents(), "{filter:?}");
    }
    // Only the first stage, and only a `$match`.
    let later = vec![
        d(&[("$project", d(&[("a", 1.into())]).into())]),
        m(d(&[("ns.coll", "a".into())])).remove(0),
    ];
    assert!(Pushdown::of(&later).reads("t", "b"));
    assert!(Pushdown::of(&[]).reads("t", "b"));
    assert!(Pushdown::of(&[d(&[("$match", 1.into())])]).reads("t", "b"));
}
