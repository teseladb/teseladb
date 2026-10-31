//! Unique secondary indexes in the writes (009 T010, FR-004): a document whose key another one
//! holds, or that an earlier statement of the same write took, is a `DuplicateKey` with the
//! index's `keyPattern` and `keyValue` (message measured on a `refrouter` 9.0.4); `null` and a
//! missing field are the same key; a document that moves off a key frees it; a build over
//! duplicates fails with 11000 and leaves no index.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

const DB: &str = "t";

/// A field of a reply, by dotted path.
fn get(r: &Document, path: &str) -> Value {
    field(r, path).clone()
}

fn arr(v: Vec<Document>) -> Value {
    Value::Array(v.into_iter().map(Value::Document).collect())
}

fn unique_b(m: &mut Mem) -> Document {
    let spec = Document::new().with("key", Document::new().with("b", 1)).with("name", "b_u");
    let spec = spec.with("unique", true);
    m.run(DB, Document::new().with("createIndexes", "c").with("indexes", arr(vec![spec])))
}

fn insert(m: &mut Mem, docs: Vec<Document>, ordered: bool) -> Document {
    let cmd = Document::new().with("insert", "c").with("documents", arr(docs));
    m.run(DB, cmd.with("ordered", ordered))
}

fn one(id: i32, b: Option<Value>) -> Document {
    let d = doc(&[("_id", id.into())]);
    match b {
        Some(v) => d.with("b", v),
        None => d,
    }
}

/// The write errors of a reply as `(index, code, keyValue)`.
fn errors(r: &Document) -> Vec<(i32, i32, Option<Value>)> {
    let Some(Value::Array(items)) = r.get("writeErrors") else { return Vec::new() };
    items
        .iter()
        .map(|e| {
            let Value::Document(e) = e else { panic!("{e:?}") };
            let int = |k: &str| match e.get(k) {
                Some(Value::Int32(n)) => *n,
                other => panic!("{k}: {other:?}"),
            };
            (int("index"), int("code"), e.get("keyValue").cloned())
        })
        .collect()
}

fn key_value(b: Value) -> Option<Value> {
    Some(Value::Document(Document::new().with("b", b)))
}

#[test]
fn a_second_document_with_the_same_key_is_a_duplicate() {
    let mut m = Mem::new();
    assert_eq!(get(&unique_b(&mut m), "ok"), Value::Double(1.0));
    let r = insert(&mut m, vec![one(1, Some(0.into()))], true);
    assert_eq!(get(&r, "n"), Value::Int32(1));
    let r = insert(&mut m, vec![one(2, Some(0.into()))], true);
    assert_eq!(get(&r, "n"), Value::Int32(0));
    assert_eq!(errors(&r), vec![(0, 11000, key_value(0.into()))]);
    let Value::Array(items) = get(&r, "writeErrors") else { panic!("{r:?}") };
    let Value::Document(e) = &items[0] else { panic!() };
    assert_eq!(
        e.get("errmsg"),
        Some(&Value::from(
            "E11000 duplicate key error collection: t.c index: b_u dup key: { b: 0 }"
        ))
    );
    assert_eq!(e.get("keyPattern"), Some(&Value::Document(Document::new().with("b", 1))));
    // The document that holds the key may write it again.
    let set = Document::new().with("$set", Document::new().with("b", 0).with("x", 1));
    let u = Document::new().with("q", Document::new().with("_id", 1)).with("u", set);
    let r = m.run(DB, Document::new().with("update", "c").with("updates", arr(vec![u])));
    assert_eq!((get(&r, "n"), errors(&r)), (Value::Int32(1), Vec::new()));
}

#[test]
fn statements_of_one_write_see_each_other() {
    let mut m = Mem::new();
    unique_b(&mut m);
    let docs = vec![one(1, Some(5.into())), one(2, Some(5.into())), one(3, Some(6.into()))];
    let r = insert(&mut m, docs, false);
    assert_eq!(get(&r, "n"), Value::Int32(2));
    assert_eq!(errors(&r), vec![(1, 11000, key_value(5.into()))]);
}

#[test]
fn null_and_a_missing_field_collide_and_a_moved_document_frees_its_key() {
    let mut m = Mem::new();
    unique_b(&mut m);
    insert(&mut m, vec![one(1, Some(Value::Null))], true);
    let r = insert(&mut m, vec![one(2, None)], true);
    assert_eq!(errors(&r), vec![(0, 11000, key_value(Value::Null))]);
    // Moving the first document to b: 1 frees null.
    let set = Document::new().with("$set", Document::new().with("b", 1));
    let u = Document::new().with("q", Document::new().with("_id", 1)).with("u", set);
    m.run(DB, Document::new().with("update", "c").with("updates", arr(vec![u])));
    let r = insert(&mut m, vec![one(2, None)], true);
    assert_eq!((get(&r, "n"), errors(&r)), (Value::Int32(1), Vec::new()));
    // An update onto a held key is refused.
    let set = Document::new().with("$set", Document::new().with("b", 1));
    let u = Document::new().with("q", Document::new().with("_id", 2)).with("u", set);
    let r = m.run(DB, Document::new().with("update", "c").with("updates", arr(vec![u])));
    assert_eq!(errors(&r), vec![(0, 11000, key_value(1.into()))]);
    // A deleted document frees its key.
    let d = Document::new().with("q", Document::new().with("_id", 1)).with("limit", 0);
    m.run(DB, Document::new().with("delete", "c").with("deletes", arr(vec![d])));
    let r = insert(&mut m, vec![one(3, Some(1.into()))], true);
    assert_eq!((get(&r, "n"), errors(&r)), (Value::Int32(1), Vec::new()));
}

#[test]
fn a_build_over_duplicates_fails_and_leaves_no_index() {
    let mut m = Mem::new();
    insert(&mut m, vec![one(1, Some(0.into())), one(2, Some(0.into()))], true);
    let r = unique_b(&mut m);
    assert_eq!(get(&r, "code"), Value::Int32(11000), "{r:?}");
    let r = m.run(DB, Document::new().with("listIndexes", "c"));
    let Value::Array(batch) = get(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    assert_eq!(batch.len(), 1, "only _id_ is left");
    // Without the duplicate the build succeeds and then guards the key.
    let d = Document::new().with("q", Document::new().with("_id", 2)).with("limit", 0);
    m.run(DB, Document::new().with("delete", "c").with("deletes", arr(vec![d])));
    assert_eq!(get(&unique_b(&mut m), "ok"), Value::Double(1.0));
    let r = insert(&mut m, vec![one(9, Some(0.into()))], true);
    assert_eq!(errors(&r), vec![(0, 11000, key_value(0.into()))]);
}
