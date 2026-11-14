//! `createIndexes`, `listIndexes` and `dropIndexes` (009 T013, FR-006, FR-007): the replies and
//! errors of a `refrouter` 9.0.4 (measured), the backfill of the documents already there, the
//! entries the writes keep afterwards and the entries a drop removes.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_keys::KeyBuilder;

const DB: &str = "t";

fn arr(v: Vec<Document>) -> Value {
    Value::Array(v.into_iter().map(Value::Document).collect())
}

fn spec(key: Document, name: &str) -> Document {
    Document::new().with("key", key).with("name", name)
}

fn create(m: &mut Mem, specs: Vec<Document>) -> Document {
    m.run(DB, Document::new().with("createIndexes", "c").with("indexes", arr(specs)))
}

fn a1() -> Document {
    Document::new().with("a", 1)
}

/// Entries of every secondary index of collection `c`.
fn entries(m: &Mem, c: u32) -> usize {
    let start = KeyBuilder::index_prefix(0, c, 1).as_bytes().to_vec();
    let end = KeyBuilder::collection_end(0, c).as_bytes().to_vec();
    m.data.range(start..end).count()
}

fn code(r: &Document) -> i32 {
    match r.get("code") {
        Some(Value::Int32(c)) => *c,
        _ => panic!("no error: {r:?}"),
    }
}

fn names(m: &mut Mem) -> Vec<Value> {
    let r = m.run(DB, Document::new().with("listIndexes", "c"));
    let Value::Array(batch) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    batch
        .iter()
        .map(|i| match i {
            Value::Document(d) => d.get("name").cloned().unwrap_or(Value::Null),
            other => other.clone(),
        })
        .collect()
}

/// The key of the node's part of a reply under `raw`: `node<N>/host:port`.
fn raw_name(m: &Mem) -> String {
    format!("node1/{}:{}", m.facts.host.hostname, m.facts.port)
}

#[test]
fn an_index_is_built_over_the_documents_and_kept_by_the_writes() {
    let mut m = Mem::new();
    let docs = (0..5).map(|i| doc(&[("_id", i.into()), ("a", i.into())])).collect();
    m.run(DB, Document::new().with("insert", "c").with("documents", arr(docs)));
    let c = m.catalog.collections.values().next().unwrap().id;
    let r = create(&mut m, vec![spec(a1(), "a_1")]);
    // The node's own reply goes first under `raw`, as a `refrouter` gives its shard's (D-102).
    let own = Document::new()
        .with("numIndexesBefore", 1)
        .with("numIndexesAfter", 2)
        .with("createdCollectionAutomatically", false)
        .with("commitQuorum", "votingMembers")
        .with("ok", 1.0);
    let mut want = Document::new().with("raw", Document::new().with(raw_name(&m), own.clone()));
    for (k, v) in own.iter() {
        want.push(k, v.clone());
    }
    assert_eq!(r, want);
    assert_eq!(entries(&m, c), 5, "one entry per document");
    assert_eq!(names(&mut m), vec![Value::from("_id_"), Value::from("a_1")]);
    m.run(
        DB,
        Document::new()
            .with("insert", "c")
            .with("documents", arr(vec![doc(&[("_id", 9.into()), ("a", 9.into())])])),
    );
    assert_eq!(entries(&m, c), 6, "a write keeps the index");
    // The same index again changes nothing.
    let r = create(&mut m, vec![spec(a1(), "a_1")]);
    assert_eq!(r.get("note"), Some(&Value::from("all indexes already exist")));
    assert_eq!(r.get("createdCollectionAutomatically"), None);
    let r = create(&mut m, vec![spec(Document::new().with("b", 1), "b_1"), spec(a1(), "a_1")]);
    assert_eq!(r.get("note"), Some(&Value::from("index already exists")));
    assert_eq!(r.get("numIndexesAfter"), Some(&Value::Int32(3)));
}

#[test]
fn conflicts_and_bad_requests_answer_as_a_refrouter() {
    let mut m = Mem::new();
    create(&mut m, vec![spec(a1(), "a_1")]);
    let r = create(&mut m, vec![spec(a1(), "other")]);
    assert_eq!(code(&r), 85);
    assert_eq!(
        r.get("errmsg"),
        Some(&Value::from("Index already exists with a different name: a_1"))
    );
    let r = create(&mut m, vec![spec(Document::new().with("b", 1), "a_1")]);
    assert_eq!(code(&r), 86);
    let want = "An existing index has the same name as the requested index. When index names are \
                not specified, they are auto generated and can cause conflicts. Please refer to \
                our documentation. Requested index: { v: 2, key: { b: 1 }, name: \"a_1\" }, \
                existing index: { v: 2, key: { a: 1 }, name: \"a_1\" }";
    assert_eq!(r.get("errmsg"), Some(&Value::from(want)));
    let r = create(&mut m, vec![spec(a1(), "a_1").with("unique", true)]);
    let Some(Value::String(msg)) = r.get("errmsg") else { panic!() };
    assert!(
        msg.contains("Requested index: { v: 2, unique: true, key: { a: 1 }, name: \"a_1\" }"),
        "{msg}"
    );
    assert_eq!(code(&create(&mut m, vec![])), 2);
    let unique = create(&mut m, vec![spec(Document::new().with("u", 1), "u").with("unique", true)]);
    assert_eq!(unique.get("ok"), Some(&Value::Double(1.0)), "{unique:?}");
    let r = m.run(
        DB,
        Document::new()
            .with("createIndexes", "c")
            .with("indexes", arr(vec![spec(Document::new().with("q", 1), "q")]))
            .with("commitQuorum", "bogus"),
    );
    assert_eq!(code(&r), 79);
    assert_eq!(
        r.get("errmsg"),
        Some(&Value::from(
            "No write concern mode named 'bogus' found in replica set configuration"
        ))
    );
    let r = m.run(
        DB,
        Document::new()
            .with("createIndexes", "c")
            .with("indexes", arr(vec![spec(Document::new().with("q", 1), "q")]))
            .with("commitQuorum", "majority"),
    );
    assert_eq!(r.get("commitQuorum"), Some(&Value::from("majority")));
}

#[test]
fn drops_remove_the_entries_and_the_catalog_entry() {
    let mut m = Mem::new();
    let docs =
        (0..3).map(|i| doc(&[("_id", i.into()), ("a", i.into()), ("b", i.into())])).collect();
    m.run(DB, Document::new().with("insert", "c").with("documents", arr(docs)));
    let c = m.catalog.collections.values().next().unwrap().id;
    create(&mut m, vec![spec(a1(), "a_1"), spec(Document::new().with("b", -1), "b_-1")]);
    assert_eq!(entries(&m, c), 6);
    let drop = |m: &mut Mem, which: Value| {
        m.run(DB, Document::new().with("dropIndexes", "c").with("index", which))
    };
    let r = drop(&mut m, Value::from("a_1"));
    let own = Document::new().with("nIndexesWas", 3).with("ok", 1.0);
    assert_eq!(r, own.clone().with("raw", Document::new().with(raw_name(&m), own)));
    assert_eq!(entries(&m, c), 3);
    assert_eq!(names(&mut m), vec![Value::from("_id_"), Value::from("b_-1")]);
    let r = drop(&mut m, Value::from("nope"));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "a missing name is not an error");
    let r = drop(&mut m, Value::from("_id_"));
    assert_eq!((code(&r), r.get("errmsg")), (72, Some(&Value::from("cannot drop _id index"))));
    let r = drop(&mut m, Value::Document(Document::new().with("b", -1)));
    assert_eq!(r.get("nIndexesWas"), Some(&Value::Int32(2)));
    create(&mut m, vec![spec(a1(), "a_1")]);
    let r = drop(&mut m, Value::from("*"));
    assert_eq!(r.get("msg"), Some(&Value::from("non-_id indexes dropped for collection")));
    assert_eq!(entries(&m, c), 0);
    let r = m.run(DB, Document::new().with("dropIndexes", "nope").with("index", "*"));
    assert_eq!(code(&r), 26);
    // Dropping the collection removes its indexes and their entries.
    create(&mut m, vec![spec(a1(), "a_1")]);
    assert_eq!(entries(&m, c), 3);
    m.run(DB, Document::new().with("drop", "c"));
    assert_eq!(entries(&m, c), 0);
    assert_eq!(m.indexes.of(c).count(), 0);
}

/// Two specifications of one `createIndexes` that are the same index, or that conflict as an
/// existing index would, fail the build and create nothing (measured on a `refrouter` 9.0.4 on
/// 03-11-2026).
#[test]
fn conflicting_specifications_of_one_command_fail_the_build() {
    let mut m = Mem::new();
    create(&mut m, vec![spec(Document::new().with("z", 1), "z_1")]);
    let cases = [
        (
            spec(a1(), "a_1"),
            96,
            "Cannot build two identical indexes. Try again without duplicate indexes.",
        ),
        (spec(a1(), "x"), 85, "Index already exists with a different name: a_1"),
        (
            spec(a1(), "a_1").with("sparse", true),
            86,
            "An existing index has the same name as the requested index. When index names are \
             not specified, they are auto generated and can cause conflicts. Please refer to our \
             documentation. Requested index: { v: 2, key: { a: 1 }, name: \"a_1\", sparse: true \
             }, existing index: { v: 2, key: { a: 1 }, name: \"a_1\" }",
        ),
    ];
    for (second, want_code, cause) in cases {
        let r = create(&mut m, vec![spec(a1(), "a_1"), second]);
        assert_eq!(code(&r), want_code, "{r:?}");
        let Some(Value::String(msg)) = r.get("errmsg") else { panic!("{r:?}") };
        assert!(msg.starts_with("Index build failed: "), "{msg}");
        assert!(msg.contains(" ( ") && msg.contains("Collection t.c"), "{msg}");
        assert!(msg.ends_with(&format!(" ) :: caused by :: {cause}")), "{msg}");
        assert_eq!(names(&mut m).len(), 2, "{r:?}");
    }
}
