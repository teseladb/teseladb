//! `$merge` with `on` fields needs a unique index of the target on exactly those fields, in any
//! order, and `$out` keeps the target's indexes (001 FR-006, 010).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

const DB: &str = "db";

fn ok(r: &Document) -> bool {
    r.get("ok") == Some(&Value::Double(1.0))
}

fn arr(items: Vec<Document>) -> Value {
    Value::Array(items.into_iter().map(Value::Document).collect())
}

fn insert(m: &mut Mem, coll: &str, docs: Vec<Document>) {
    let r = m.run(DB, doc(&[("insert", Value::from(coll)), ("documents", arr(docs))]));
    assert!(ok(&r), "{r:?}");
}

fn index(m: &mut Mem, coll: &str, name: &str, key: Document, unique: bool) {
    let spec = doc(&[
        ("key", Value::Document(key)),
        ("name", Value::from(name)),
        ("unique", Value::Boolean(unique)),
    ]);
    let r = m.run(DB, doc(&[("createIndexes", Value::from(coll)), ("indexes", arr(vec![spec]))]));
    assert!(ok(&r), "{r:?}");
}

fn aggregate(m: &mut Mem, stages: Vec<Document>) -> Document {
    m.run(
        DB,
        doc(&[
            ("aggregate", Value::from("src")),
            ("pipeline", arr(stages)),
            ("cursor", Value::Document(Document::new())),
        ]),
    )
}

fn ab(a: i32, b: i32) -> Document {
    doc(&[("a", Value::Int32(a)), ("b", Value::Int32(b))])
}

#[test]
fn merge_on_fields_finds_the_unique_index_on_exactly_them() {
    let mut m = Mem::new();
    insert(&mut m, "src", vec![ab(1, 2), ab(3, 4)]);
    insert(&mut m, "t", vec![ab(1, 2)]);
    let ab_key = doc(&[("a", Value::Int32(1)), ("b", Value::Int32(1))]);
    index(&mut m, "t", "a_only", doc(&[("a", Value::Int32(1))]), false);
    index(&mut m, "t", "ab", ab_key, true);
    // Without the source's `_id`, which the target's matching document keeps.
    let on = |fields: &[&str]| {
        let on = Value::Array(fields.iter().map(|f| Value::from(*f)).collect());
        let no_id = doc(&[("$project", Value::Document(doc(&[("_id", Value::Int32(0))])))]);
        let merge = doc(&[("into", Value::from("t")), ("on", on)]);
        vec![no_id, doc(&[("$merge", Value::Document(merge))])]
    };
    // The fields in another order than the index's.
    let r = aggregate(&mut m, on(&["b", "a"]));
    assert!(ok(&r), "{r:?}");
    let r = m.run(DB, doc(&[("count", Value::from("t"))]));
    assert_eq!(r.get("n"), Some(&Value::Int32(2)), "{r:?}");
    // `a` alone has an index, but not a unique one.
    let r = aggregate(&mut m, on(&["a"]));
    assert_eq!(r.get("code"), Some(&Value::Int32(51190)), "{r:?}");
}

#[test]
fn out_replaces_the_target_and_keeps_its_indexes() {
    let mut m = Mem::new();
    insert(&mut m, "src", vec![ab(1, 2)]);
    insert(&mut m, "t", vec![ab(9, 9)]);
    index(&mut m, "t", "b_1", doc(&[("b", Value::Int32(1))]), false);
    let r = aggregate(&mut m, vec![doc(&[("$out", Value::from("t"))])]);
    assert!(ok(&r), "{r:?}");
    let r = m.run(DB, doc(&[("listIndexes", Value::from("t"))]));
    assert_eq!(field(&r, "cursor.firstBatch.1.name"), &Value::from("b_1"), "{r:?}");
    let r = m.run(DB, doc(&[("find", Value::from("t"))]));
    assert_eq!(field(&r, "cursor.firstBatch.0.a"), &Value::Int32(1), "{r:?}");
}

#[test]
fn a_project_of_id_alone_drops_or_keeps_only_id() {
    // `{_id: 0}` alone is an exclusion: every other field stays, as in `find`. `{_id: 1}` alone
    // keeps `_id` only.
    let mut m = Mem::new();
    insert(&mut m, "src", vec![ab(1, 2)]);
    let project = |v: Value| doc(&[("$project", Value::Document(doc(&[("_id", v)])))]);
    for drop in [Value::Int32(0), Value::Boolean(false), Value::Double(0.0)] {
        let r = aggregate(&mut m, vec![project(drop)]);
        assert_eq!(field(&r, "cursor.firstBatch.0"), &Value::Document(ab(1, 2)), "{r:?}");
    }
    let r = aggregate(&mut m, vec![project(Value::Int32(1))]);
    let Value::Document(only) = field(&r, "cursor.firstBatch.0") else { panic!("{r:?}") };
    assert_eq!(only.iter().map(|(k, _)| k).collect::<Vec<_>>(), ["_id"]);
}
