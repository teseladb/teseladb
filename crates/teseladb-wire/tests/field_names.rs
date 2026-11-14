//! The dictionary of field names of each collection (003 FR-022, T036) through the commands: the
//! names of the documents a write stores get numbers in the catalog before the write, the stored
//! tables carry them, the reads give the same documents, a dictionary only grows, a rename keeps
//! it and a drop takes it, so a collection created again starts another one.

mod common;

use common::{Mem, doc, field};
use teseladb_keys::space::{collection_id_of, in_documents};
use teseladb_keys::stored::{self, MIN_TABLE_FIELDS};
use teseladb_meta::catalog::Namespace;
use teseladb_bson::{Document, Value};

/// Document `i` with fields `{prefix}0…` enough for a table and a nested document with its own.
fn wide(i: i32, prefix: &str) -> Document {
    let mut d = Document::new().with("_id", i);
    let mut inner = Document::new();
    for f in 0..MIN_TABLE_FIELDS {
        d.push(format!("{prefix}{f}"), i * 10 + f as i32);
        inner.push(format!("in{f}"), i + f as i32);
    }
    d.push("nested", inner);
    d
}

fn insert(m: &mut Mem, coll: &str, docs: Vec<Document>) {
    let docs: Vec<Value> = docs.into_iter().map(Value::Document).collect();
    let n = docs.len() as i32;
    let r = m.run("db", Document::new().with("insert", coll).with("documents", docs));
    assert_eq!(r.get("n"), Some(&Value::Int32(n)), "{r:?}");
}

fn find(m: &mut Mem, coll: &str, filter: Document) -> Vec<Value> {
    let cmd = Document::new()
        .with("find", coll)
        .with("filter", filter)
        .with("sort", doc(&[("_id", 1.into())]));
    let r = m.run("db", cmd);
    match field(&r, "cursor.firstBatch") {
        Value::Array(a) => a.clone(),
        other => panic!("{other:?} in {r:?}"),
    }
}

fn id_of(m: &Mem, coll: &str) -> u32 {
    m.catalog.get(&Namespace { db: "db".into(), coll: coll.into() }).expect("collection").id
}

/// The dictionary identifier of the table of each stored document of collection `id`.
fn tables_of(m: &Mem, id: u32) -> Vec<Option<u32>> {
    m.stored(..)
        .into_iter()
        .filter(|(k, _)| in_documents(k) && collection_id_of(k) == Some(id))
        .map(|(_, v)| stored::split(v).expect("stored").table().and_then(|t| t.dictionary()))
        .collect()
}

fn number(m: &Mem, id: u32, name: &str) -> Option<u16> {
    m.catalog.fields_of(id).and_then(|d| d.get(name.as_bytes()))
}

fn ok(r: &Document) -> bool {
    matches!(r.get("ok"), Some(Value::Double(x)) if *x == 1.0)
        || matches!(r.get("ok"), Some(Value::Int32(1)))
}

fn d(i: i32, prefix: &str) -> Value {
    Value::Document(wide(i, prefix))
}

#[test]
fn a_write_numbers_its_names_and_the_reads_give_the_same_documents() {
    let mut m = Mem::new();
    insert(&mut m, "c", (0..20).map(|i| wide(i, "f")).collect());
    let id = id_of(&m, "c");
    let dict = m.catalog.fields_of(id).expect("a dictionary").clone();
    for n in ["_id", "f0", "f7", "nested", "in3"] {
        assert!(dict.get(n.as_bytes()).is_some(), "{n} numbered");
    }
    let tables = tables_of(&m, id);
    assert_eq!(tables.len(), 20);
    assert!(tables.iter().all(|t| *t == Some(dict.dictionary())), "{tables:?}");
    assert_eq!(find(&mut m, "c", doc(&[("f3", 53.into())])), vec![d(5, "f")]);
    assert_eq!(find(&mut m, "c", doc(&[("nested.in4", 13.into())])), vec![d(9, "f")]);
    let range = doc(&[("f0", doc(&[("$gte", 180.into())]).into())]);
    assert_eq!(find(&mut m, "c", range), vec![d(18, "f"), d(19, "f")]);
    // A narrow collection gets no dictionary: its documents have no table.
    insert(&mut m, "small", vec![doc(&[("_id", 1.into()), ("a", 1.into())])]);
    assert!(m.catalog.fields_of(id_of(&m, "small")).is_none());
}

#[test]
fn the_dictionary_grows_and_a_new_version_keeps_the_numbers() {
    let mut m = Mem::new();
    insert(&mut m, "c", (0..5).map(|i| wide(i, "f")).collect());
    let id = id_of(&m, "c");
    let before: Vec<Option<u16>> =
        (0..MIN_TABLE_FIELDS).map(|f| number(&m, id, &format!("f{f}"))).collect();
    insert(&mut m, "c", (5..10).map(|i| wide(i, "g")).collect());
    let after: Vec<Option<u16>> =
        (0..MIN_TABLE_FIELDS).map(|f| number(&m, id, &format!("f{f}"))).collect();
    assert_eq!(before, after, "a number never changes");
    assert!(number(&m, id, "g3").is_some());
    // An update brings a new name to a document of the first batch.
    let set = doc(&[("$set", doc(&[("g3", Value::from("late")), ("h", 1.into())]).into())]);
    let upd = doc(&[("q", doc(&[("_id", 2.into())]).into()), ("u", set.into())]);
    let r =
        m.run("db", doc(&[("update", "c".into()), ("updates", Value::Array(vec![upd.into()]))]));
    assert_eq!(r.get("nModified"), Some(&Value::Int32(1)), "{r:?}");
    assert!(number(&m, id, "h").is_some());
    let dictionary = m.catalog.fields_of(id).unwrap().dictionary();
    assert!(tables_of(&m, id).iter().all(|t| *t == Some(dictionary)));
    let mut two = wide(2, "f");
    two.push("g3", "late");
    two.push("h", 1);
    assert_eq!(find(&mut m, "c", doc(&[("g3", "late".into())])), vec![Value::Document(two)]);
    assert_eq!(find(&mut m, "c", doc(&[("g3", 53.into())])), vec![d(5, "g")]);
    assert_eq!(find(&mut m, "c", doc(&[("f1", 41.into())])), vec![d(4, "f")]);
}

#[test]
fn a_rename_keeps_the_dictionary_and_a_drop_takes_it() {
    let mut m = Mem::new();
    insert(&mut m, "c", (0..6).map(|i| wide(i, "f")).collect());
    let id = id_of(&m, "c");
    let dictionary = m.catalog.fields_of(id).unwrap().dictionary();
    let rename = doc(&[("renameCollection", "db.c".into()), ("to", "db.d".into())]);
    assert!(ok(&m.run("admin", rename)));
    assert_eq!(id_of(&m, "d"), id);
    assert_eq!(m.catalog.fields_of(id).map(|f| f.dictionary()), Some(dictionary));
    assert_eq!(find(&mut m, "d", doc(&[("f2", 42.into())])), vec![d(4, "f")]);
    insert(&mut m, "d", vec![wide(6, "f")]);
    assert!(tables_of(&m, id).iter().all(|t| *t == Some(dictionary)));
    // Dropped and created again under the name: another collection, another dictionary.
    assert!(ok(&m.run("db", doc(&[("drop", "d".into())]))));
    assert!(m.catalog.fields_of(id).is_none());
    insert(&mut m, "d", (0..3).map(|i| wide(i, "f")).collect());
    let again = id_of(&m, "d");
    assert_ne!(again, id);
    let other = m.catalog.fields_of(again).unwrap().dictionary();
    assert_ne!(other, dictionary);
    assert!(tables_of(&m, again).iter().all(|t| *t == Some(other)));
    assert_eq!(find(&mut m, "d", doc(&[("f2", 22.into())])), vec![d(2, "f")]);
    assert_eq!(find(&mut m, "d", doc(&[("f2", 42.into())])), Vec::<Value>::new());
}
