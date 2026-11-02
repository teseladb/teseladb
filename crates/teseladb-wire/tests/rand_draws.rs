//! `$rand` and `$sampleRate` draw a new number for every document (010 FR-005): in `find`'s
//! `$expr`, in an `aggregate`'s stages and in the stages after `$changeStream` (014 FR-013). A
//! `refrouter` 9.0.4 keeps about half of 400 documents with `{$sampleRate: 0.5}` and gives each
//! document of `$addFields: {r: {$rand: {}}}` its own number (measured on 02-11).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

const DB: &str = "rd";
const COUNT: i32 = 400;

fn load(m: &mut Mem) {
    let docs: Vec<Value> = (0..COUNT).map(|i| Value::Document(doc(&[("_id", i.into())]))).collect();
    let r = m.run(DB, Document::new().with("insert", "c").with("documents", Value::Array(docs)));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
}

fn first_batch(r: &Document) -> Vec<Document> {
    match field(r, "cursor.firstBatch") {
        Value::Array(a) => a
            .iter()
            .map(|v| match v {
                Value::Document(d) => d.clone(),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?} in {r:?}"),
    }
}

fn run(m: &mut Mem, cmd: &str) -> Vec<Document> {
    first_batch(&m.run(DB, extjson::parse(cmd).expect("command")))
}

fn draws(docs: &[Document]) -> Vec<f64> {
    docs.iter()
        .map(|d| match d.get("r") {
            Some(Value::Double(x)) if (0.0..1.0).contains(x) => *x,
            other => panic!("{other:?} in {d:?}"),
        })
        .collect()
}

fn distinct(xs: &[f64]) -> usize {
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    v.dedup();
    v.len()
}

#[test]
fn sample_rate_in_a_find_keeps_about_the_rate() {
    let mut m = Mem::new();
    load(&mut m);
    let kept = run(&mut m, r#"{"find":"c","filter":{"$sampleRate":0.5},"batchSize":1000}"#).len();
    assert!((100..300).contains(&kept), "{kept} of {COUNT}");
    let kept = run(
        &mut m,
        r#"{"find":"c","filter":{"$expr":{"$lt":[{"$rand":{}},0.25]}},"batchSize":1000}"#,
    )
    .len();
    assert!((40..160).contains(&kept), "{kept} of {COUNT}");
}

#[test]
fn rand_in_an_aggregate_differs_per_document() {
    let mut m = Mem::new();
    load(&mut m);
    let docs = run(
        &mut m,
        r#"{"aggregate":"c","pipeline":[{"$addFields":{"r":{"$rand":{}}}}],"cursor":{"batchSize":1000}}"#,
    );
    let xs = draws(&docs);
    assert_eq!(xs.len(), COUNT as usize);
    assert!(distinct(&xs) > 390, "{} distinct", distinct(&xs));
    let kept = run(
        &mut m,
        r#"{"aggregate":"c","pipeline":[{"$match":{"$sampleRate":0.5}}],"cursor":{"batchSize":1000}}"#,
    )
    .len();
    assert!((100..300).contains(&kept), "{kept} of {COUNT}");
}

#[test]
fn rand_in_a_find_projection_differs_per_document() {
    let mut m = Mem::new();
    load(&mut m);
    let docs = run(&mut m, r#"{"find":"c","projection":{"r":{"$rand":{}}},"batchSize":1000}"#);
    let xs = draws(&docs);
    assert!(distinct(&xs) > 390, "{} distinct", distinct(&xs));
}

#[test]
fn rand_after_a_change_stream_differs_per_event() {
    let mut m = Mem::new();
    m.run(DB, Document::new().with("create", "e"));
    let open = m.run(
        DB,
        extjson::parse(
            r#"{"aggregate":"e","pipeline":[{"$changeStream":{}},{"$addFields":{"r":{"$rand":{}}}}],"cursor":{}}"#,
        )
        .expect("command"),
    );
    let id = match field(&open, "cursor.id") {
        Value::Int64(n) => *n,
        other => panic!("{other:?} in {open:?}"),
    };
    let docs: Vec<Value> =
        (0..20).map(|i| Value::Document(Document::new().with("_id", i))).collect();
    m.run(DB, Document::new().with("insert", "e").with("documents", Value::Array(docs)));
    let r = m.run(DB, Document::new().with("getMore", id).with("collection", "e"));
    let events = match field(&r, "cursor.nextBatch") {
        Value::Array(a) => a
            .iter()
            .map(|v| match v {
                Value::Document(d) => d.clone(),
                other => panic!("{other:?}"),
            })
            .collect::<Vec<_>>(),
        other => panic!("{other:?} in {r:?}"),
    };
    let xs = draws(&events);
    assert_eq!(xs.len(), 20);
    assert_eq!(distinct(&xs), 20, "{xs:?}");
}
