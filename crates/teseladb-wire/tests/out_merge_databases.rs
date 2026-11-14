//! The databases `$merge` and `$out` need, and the order of the cursor of an `aggregate: 1`
//! that writes, as the reference router 9.0.4 answers (measured on 14-11-2026, 002 T048):
//! `$merge` needs its target's database, and from a collection its source's first; an
//! `aggregate: 1` with `$out` fails only when neither its database nor the target's exists,
//! naming its own; an `aggregate: 1` that writes answers its cursor as a collection's does.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

fn arr(items: Vec<Document>) -> Value {
    Value::Array(items.into_iter().map(Value::Document).collect())
}

fn documents() -> Document {
    doc(&[("$documents", arr(vec![doc(&[("a", Value::Int32(1))])]))])
}

fn into(db: &str, coll: &str) -> Value {
    Value::Document(doc(&[("db", Value::from(db)), ("coll", Value::from(coll))]))
}

fn merge(db: &str, coll: &str) -> Document {
    doc(&[("$merge", Value::Document(doc(&[("into", into(db, coll))])))])
}

fn out(db: &str, coll: &str) -> Document {
    doc(&[("$out", into(db, coll))])
}

fn aggregate(m: &mut Mem, db: &str, target: Value, stages: Vec<Document>) -> Document {
    m.run(
        db,
        doc(&[
            ("aggregate", target),
            ("pipeline", arr(stages)),
            ("cursor", Value::Document(Document::new())),
        ]),
    )
}

/// `None` when the command passed, the code and message of its error otherwise.
fn failure(r: &Document) -> Option<(i32, String)> {
    if r.get("ok") == Some(&Value::Double(1.0)) {
        return None;
    }
    let code = match r.get("code") {
        Some(Value::Int32(c)) => *c,
        other => panic!("no code: {other:?} in {r:?}"),
    };
    let Some(Value::String(msg)) = r.get("errmsg") else { panic!("{r:?}") };
    Some((code, msg.clone()))
}

fn not_found(db: &str) -> Option<(i32, String)> {
    Some((26, format!("database {db} not found")))
}

fn with_database(m: &mut Mem, db: &str) {
    let r = m.run(db, doc(&[("insert", Value::from("c")), ("documents", arr(vec![doc(&[])]))]));
    assert_eq!(failure(&r), None, "{r:?}");
}

#[test]
fn merge_needs_its_target_database_and_from_a_collection_its_source_first() {
    let mut m = Mem::new();
    with_database(&mut m, "exists_y");
    let one = Value::Int32(1);
    let c = Value::from("c");
    let r = aggregate(&mut m, "exists_y", c.clone(), vec![merge("absent_z", "c")]);
    assert_eq!(failure(&r), not_found("absent_z"));
    let r = aggregate(&mut m, "absent_v", c, vec![merge("absent_z", "c")]);
    assert_eq!(failure(&r), not_found("absent_v"), "the source's database first");
    let r = aggregate(&mut m, "absent_q", one.clone(), vec![documents(), merge("absent_r", "c")]);
    assert_eq!(failure(&r), not_found("absent_r"));
    let r = aggregate(&mut m, "absent_w", one, vec![documents(), merge("exists_y", "c")]);
    assert_eq!(failure(&r), None, "{r:?}");
}

#[test]
fn a_database_level_out_fails_only_when_neither_database_exists() {
    let mut m = Mem::new();
    with_database(&mut m, "exists_y");
    let one = Value::Int32(1);
    let r = aggregate(&mut m, "absent_q", one.clone(), vec![documents(), out("absent_r", "c")]);
    assert_eq!(failure(&r), not_found("absent_q"), "its own database is named");
    let r = aggregate(&mut m, "nodb_x", one.clone(), vec![documents(), out("nodb_x", "c")]);
    assert_eq!(failure(&r), not_found("nodb_x"));
    let r = aggregate(&mut m, "absent_w", one.clone(), vec![documents(), out("exists_y", "d")]);
    assert_eq!(failure(&r), None, "{r:?}");
    let r = aggregate(&mut m, "exists_y", one, vec![documents(), out("absent_z", "c")]);
    assert_eq!(failure(&r), None, "{r:?}");
    // From a collection, `$out` creates what it needs.
    let r = aggregate(&mut m, "other_x", Value::from("c"), vec![out("other_x", "c")]);
    assert_eq!(failure(&r), None, "{r:?}");
}

#[test]
fn a_database_level_aggregate_that_writes_answers_as_a_collection() {
    let mut m = Mem::new();
    with_database(&mut m, "exists_y");
    let keys = |r: &Document| -> Vec<String> {
        let Some(Value::Document(c)) = r.get("cursor") else { panic!("{r:?}") };
        c.iter().map(|(k, _)| k.to_owned()).collect()
    };
    let one = Value::Int32(1);
    let r = aggregate(&mut m, "exists_y", one.clone(), vec![documents()]);
    assert_eq!(keys(&r), ["firstBatch", "id", "ns"]);
    let r = aggregate(&mut m, "exists_y", one.clone(), vec![documents(), merge("exists_y", "c")]);
    assert_eq!(keys(&r), ["id", "ns", "firstBatch"]);
    let r = aggregate(&mut m, "exists_y", one, vec![documents(), out("exists_y", "d")]);
    assert_eq!(keys(&r), ["id", "ns", "firstBatch"]);
    assert_eq!(field(&r, "cursor.ns"), &Value::from("exists_y.$cmd.aggregate"));
}
