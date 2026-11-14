//! Index maintenance in the writes of 006 (009 T007, FR-003): every index being built or ready of
//! the collection gets the entries of a new version and loses those of the old one, in the same
//! write as the document; an index that refuses a document refuses its statement with the reference
//! server's error; a dropping index and a `text` index get nothing here.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_index::delta::{IndexSet, Version};
use teseladb_index::spec::index_def;
use teseladb_keys::{KeyBuilder, PrimaryKey};
use teseladb_meta::indexes::{IndexEntry, IndexState};

const DB: &str = "t";

fn spec(key: Document) -> Vec<u8> {
    Document::new().with("key", key).with("name", "i").with("v", 2).to_bytes().unwrap()
}

fn index(id: u32, collection: u32, key: Document, state: IndexState) -> IndexEntry {
    IndexEntry { id, collection, name: format!("i{id}"), spec: spec(key), buckets: 0, state }
}

/// A node with collection `c` (number returned) and the indexes `make(collection)` gives.
fn node(make: impl Fn(u32) -> Vec<IndexEntry>) -> (Mem, u32) {
    let mut m = Mem::new();
    let r = m.run(DB, Document::new().with("create", "c"));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let id = m.catalog.collections.values().next().expect("c").id;
    for e in make(id) {
        m.indexes.last_index = m.indexes.last_index.max(e.id);
        m.indexes.indexes.insert(e.id, e);
    }
    (m, id)
}

/// The keys of the entries of index `id` in the node.
fn entries(m: &Mem, collection: u32, id: u32) -> Vec<Vec<u8>> {
    let start = KeyBuilder::index_prefix(0, collection, id).as_bytes().to_vec();
    let end = KeyBuilder::index_prefix(0, collection, id + 1).as_bytes().to_vec();
    m.data.range(start..end).map(|(k, _)| k.clone()).collect()
}

/// The keys index `key` gives document `doc` of collection `collection`.
fn expected(collection: u32, id: u32, key: Document, doc: &Document) -> Vec<Vec<u8>> {
    let s = spec(key);
    let def = index_def(RawDoc::from_bytes(&s).unwrap(), 0).unwrap().unwrap();
    let set = IndexSet { tenant: 0, collection, indexes: vec![(id, def)] };
    let bytes = doc.to_bytes().unwrap();
    let raw = RawDoc::from_bytes(&bytes).unwrap();
    let pk = PrimaryKey::id(raw.get("_id").unwrap());
    let keys = set.keys(Version { doc: raw, pk: &pk }, None).unwrap();
    keys.keys().map(|k| k.as_bytes().to_vec()).collect()
}

fn insert(m: &mut Mem, docs: Vec<Document>, ordered: bool) -> Document {
    let docs = Value::Array(docs.into_iter().map(Value::Document).collect());
    m.run(DB, Document::new().with("insert", "c").with("documents", docs).with("ordered", ordered))
}

#[test]
fn writes_keep_the_entries_of_every_index() {
    let a = || Document::new().with("a", 1);
    let ab = || Document::new().with("b", -1).with("a", 1);
    let (mut m, c) = node(|c| {
        vec![
            index(1, c, a(), IndexState::Ready),
            index(2, c, ab(), IndexState::Building),
            index(3, c, Document::new().with("x", 1), IndexState::Dropping),
            index(4, c, Document::new().with("s", "text"), IndexState::Ready),
        ]
    });
    let one = Document::new().with("_id", 1).with("a", Value::Array(vec![1.into(), 2.into()]));
    let two = Document::new().with("_id", 2).with("a", 3).with("b", "x").with("x", 1);
    let r = insert(&mut m, vec![one.clone(), two.clone()], true);
    assert_eq!(field(&r, "n"), &Value::Int32(2), "{r:?}");
    let mut want = expected(c, 1, a(), &one);
    want.extend(expected(c, 1, a(), &two));
    want.sort();
    assert_eq!(entries(&m, c, 1), want);
    assert_eq!(entries(&m, c, 1).len(), 3, "one entry per array element");
    let mut want = expected(c, 2, ab(), &one);
    want.extend(expected(c, 2, ab(), &two));
    want.sort();
    assert_eq!(entries(&m, c, 2), want);
    assert!(entries(&m, c, 3).is_empty() && entries(&m, c, 4).is_empty());

    // An update moves the entries; a delete removes them.
    let u = Document::new()
        .with("q", Document::new().with("_id", 2))
        .with("u", Document::new().with("$set", Document::new().with("a", 7)));
    let r = m
        .run(DB, Document::new().with("update", "c").with("updates", Value::Array(vec![u.into()])));
    assert_eq!(r.get("nModified"), Some(&Value::Int32(1)), "{r:?}");
    let moved = Document::new().with("_id", 2).with("a", 7).with("b", "x").with("x", 1);
    let mut want = expected(c, 1, a(), &one);
    want.extend(expected(c, 1, a(), &moved));
    want.sort();
    assert_eq!(entries(&m, c, 1), want);
    let del = Document::new().with("q", Document::new().with("_id", 1)).with("limit", 1);
    let r = m.run(
        DB,
        Document::new().with("delete", "c").with("deletes", Value::Array(vec![del.into()])),
    );
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    assert_eq!(entries(&m, c, 1), expected(c, 1, a(), &moved));
    assert_eq!(entries(&m, c, 2), expected(c, 2, ab(), &moved));
}

#[test]
fn a_delete_only_index_loses_entries_and_gains_none() {
    let a = || Document::new().with("a", 1);
    let (mut m, c) = node(|c| vec![index(1, c, a(), IndexState::Building)]);
    let one = Document::new().with("_id", 1).with("a", 1);
    let r = insert(&mut m, vec![one.clone()], true);
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    assert_eq!(entries(&m, c, 1), expected(c, 1, a(), &one));
    // Delete-only (009 FR-006): an update removes the old entry and adds no new one, and an
    // insert adds none.
    m.indexes.indexes.get_mut(&1).expect("index").state = IndexState::DeleteOnly;
    let u = Document::new()
        .with("q", Document::new().with("_id", 1))
        .with("u", Document::new().with("$set", Document::new().with("a", 7)));
    let r = m
        .run(DB, Document::new().with("update", "c").with("updates", Value::Array(vec![u.into()])));
    assert_eq!(r.get("nModified"), Some(&Value::Int32(1)), "{r:?}");
    let two = Document::new().with("_id", 2).with("a", 2);
    let r = insert(&mut m, vec![two], true);
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    assert!(entries(&m, c, 1).is_empty(), "{:?}", entries(&m, c, 1));
}

#[test]
fn a_delete_only_unique_index_refuses_nothing() {
    let spec = Document::new()
        .with("key", Document::new().with("a", 1))
        .with("name", "i")
        .with("v", 2)
        .with("unique", true)
        .to_bytes()
        .unwrap();
    let (mut m, c) = node(|c| {
        vec![IndexEntry {
            id: 1,
            collection: c,
            name: "i1".into(),
            spec: spec.clone(),
            buckets: 0,
            state: IndexState::DeleteOnly,
        }]
    });
    let docs = vec![
        Document::new().with("_id", 1).with("a", 1),
        Document::new().with("_id", 2).with("a", 1),
    ];
    let r = insert(&mut m, docs, true);
    assert_eq!(field(&r, "n"), &Value::Int32(2), "{r:?}");
    assert!(entries(&m, c, 1).is_empty());
}

#[test]
fn a_document_an_index_refuses_fails_its_statement() {
    let ab = || Document::new().with("a", 1).with("b", 1);
    let (mut m, c) = node(|c| vec![index(1, c, ab(), IndexState::Ready)]);
    let ok = doc(&[("_id", 1.into()), ("a", 1.into())]);
    let parallel = Document::new()
        .with("_id", 2)
        .with("a", Value::Array(vec![1.into()]))
        .with("b", Value::Array(vec![2.into()]));
    let after = Document::new().with("_id", 3);
    let r = insert(&mut m, vec![ok.clone(), parallel.clone(), after.clone()], true);
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    let Some(Value::Array(errors)) = r.get("writeErrors") else { panic!("{r:?}") };
    let Value::Document(e) = &errors[0] else { panic!() };
    assert_eq!(e.get("index"), Some(&Value::Int32(1)));
    assert_eq!(e.get("code"), Some(&Value::Int32(171)));
    assert_eq!(e.get("errmsg"), Some(&Value::from("cannot index parallel arrays [b] [a]")));
    assert_eq!(entries(&m, c, 1), expected(c, 1, ab(), &ok));
    // Unordered, the documents after it go in.
    let r = insert(&mut m, vec![parallel, after.clone()], false);
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    assert_eq!(entries(&m, c, 1).len(), 2);
    // An update to such a version fails with the update's prefix.
    let u = Document::new().with("q", Document::new().with("_id", 3)).with(
        "u",
        Document::new().with(
            "$set",
            Document::new()
                .with("a", Value::Array(vec![1.into()]))
                .with("b", Value::Array(vec![1.into()])),
        ),
    );
    let r = m
        .run(DB, Document::new().with("update", "c").with("updates", Value::Array(vec![u.into()])));
    let Some(Value::Array(errors)) = r.get("writeErrors") else { panic!("{r:?}") };
    let Value::Document(e) = &errors[0] else { panic!() };
    assert_eq!(e.get("code"), Some(&Value::Int32(171)), "{e:?}");
}
