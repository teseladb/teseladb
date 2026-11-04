//! Pipeline updates over the in-memory node of `common` (014 FR-009, 018 FR-009): each case a
//! `refnode` 9.0.4 measured (`teseladb-changestream/tests/fixtures/pipeline_updates.jsonl`, 05-11-2026)
//! runs here with its pipeline, and its change event and oplog entry are the ones measured: an
//! `update` with the same `updateDescription` and `{$v: 2, diff}`, or a `replace` with the whole
//! document.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};
use teseladb_changestream::event::operation_type;
use teseladb_changestream::token::EventKind;

const FIXTURE: &str =
    include_str!("../../teseladb-changestream/tests/fixtures/pipeline_updates.jsonl");
const DB: &str = "p";
const COLL: &str = "c";

fn arr(v: Vec<Document>) -> Value {
    Value::Array(v.into_iter().map(Value::Document).collect())
}

fn ok(r: &Document) {
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
}

fn docs(v: &Value) -> Vec<Document> {
    match v {
        Value::Array(a) => a
            .iter()
            .map(|v| match v {
                Value::Document(d) => d.clone(),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

/// Inserts `before`, runs the pipeline over it with a change stream open, and gives the events of
/// the update, the oplog entries of the update and the document after it.
fn run_case(before: &Document, pipeline: &Value) -> (Vec<Document>, Vec<Document>, Document) {
    let mut m = Mem::new();
    ok(&m.run(
        DB,
        Document::new().with("insert", COLL).with("documents", arr(vec![before.clone()])),
    ));
    let stage = Document::new().with("$changeStream", Document::new());
    let open = m.run(
        DB,
        Document::new()
            .with("aggregate", COLL)
            .with("pipeline", arr(vec![stage]))
            .with("cursor", Document::new()),
    );
    let Value::Int64(id) = field(&open, "cursor.id").clone() else { panic!("{open:?}") };
    let one = doc(&[("q", Value::Document(doc(&[("_id", 1.into())])))]).with("u", pipeline.clone());
    ok(&m.run(DB, Document::new().with("update", COLL).with("updates", arr(vec![one]))));
    let more = m.run(DB, Document::new().with("getMore", id).with("collection", COLL));
    let events = docs(field(&more, "cursor.nextBatch"));
    let find = Document::new()
        .with("find", "oplog.rs")
        .with("filter", Document::new().with("ns", format!("{DB}.{COLL}")).with("op", "u"));
    let entries = docs(field(&m.run("local", find), "cursor.firstBatch"));
    let read = Document::new().with("find", COLL);
    let after = docs(field(&m.run(DB, read), "cursor.firstBatch")).remove(0);
    (events, entries, after)
}

#[test]
fn pipeline_updates_give_the_events_and_entries_a_refnode_gives() {
    let update = operation_type(EventKind::Update);
    let replace = operation_type(EventKind::Replace);
    for line in FIXTURE.lines() {
        let c = extjson::parse(line).unwrap();
        let name = field(&c, "case").clone();
        let Value::Document(before) = field(&c, "before").clone() else { panic!("{name:?}") };
        let (events, entries, after) = run_case(&before, field(&c, "pipeline"));
        // By bytes: `NaN` is itself.
        let Value::Document(want) = field(&c, "after") else { panic!("{name:?}") };
        assert_eq!(after.to_bytes(), want.to_bytes(), "{name:?}: document");
        let kind = |e: &Document| match field(e, "operationType") {
            Value::String(s) => s.clone(),
            other => panic!("{other:?}"),
        };
        match c.get("event") {
            Some(Value::String(e)) if e == update => {
                assert_eq!(events.len(), 1, "{name:?}: {events:?}");
                assert_eq!(kind(&events[0]), update, "{name:?}");
                assert_eq!(
                    field(&events[0], "updateDescription"),
                    field(&c, "updateDescription"),
                    "{name:?}: description"
                );
                let o = Document::new().with("$v", 2).with("diff", field(&c, "diff").clone());
                assert_eq!(entries.len(), 1, "{name:?}: {entries:?}");
                assert_eq!(entries[0].get("o"), Some(&Value::Document(o)), "{name:?}: oplog");
            }
            Some(Value::String(e)) if e == replace => {
                assert_eq!(events.len(), 1, "{name:?}: {events:?}");
                assert_eq!(kind(&events[0]), replace, "{name:?}");
                assert_eq!(entries.len(), 1, "{name:?}: {entries:?}");
                assert_eq!(entries[0].get("o"), Some(&Value::Document(after)), "{name:?}: oplog");
            }
            // A pipeline that changes no byte writes nothing.
            _ => assert!(events.is_empty() && entries.is_empty(), "{name:?}: {events:?}"),
        }
    }
}

/// The two updates of `cs-split-large` as the battery measured them on a `refrouter` 9.0.4 on
/// 04-11-2026: doubling a 1 KiB string 13 times (8 MiB) replaces the document, and adding 6 MB of
/// it as a new field to the 8 MiB document is an `update` of that field.
#[test]
fn the_large_updates_of_cs_split_large() {
    let s = "x".repeat(1024);
    let before = doc(&[("_id", 1.into()), ("s", Value::from(s.as_str()))]);
    let double = Document::new().with(
        "$set",
        Document::new().with("s", Document::new().with("$concat", vec![Value::from("$s"); 2])),
    );
    let pipeline = Value::Array(vec![Value::Document(double); 13]);
    let (events, _, after) = run_case(&before, &pipeline);
    let kind = |e: &Document| field(e, "operationType").clone();
    let replace = Value::from(operation_type(EventKind::Replace));
    assert_eq!(events.iter().map(kind).collect::<Vec<_>>(), [replace]);
    let cut = Document::new()
        .with("$substrBytes", vec![Value::from("$s"), Value::Int32(0), Value::Int32(6_000_000)]);
    let add = Document::new().with("$set", Document::new().with("u", cut));
    let (events, _, _) = run_case(&after, &Value::Array(vec![Value::Document(add)]));
    assert_eq!(events.len(), 1);
    assert_eq!(kind(&events[0]), Value::from(operation_type(EventKind::Update)));
    let Value::Document(fields) = field(&events[0], "updateDescription.updatedFields") else {
        panic!("{:?}", events[0].get("updateDescription"))
    };
    let names: Vec<&str> = fields.iter().map(|(k, _)| k).collect();
    assert_eq!(names, ["u"]);
}
