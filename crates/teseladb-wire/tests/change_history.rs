//! The meta each written document keeps next to it for its change event (014, D-114) is no
//! document: finds, counts, `collStats`, `dbStats`, index builds and the TTL monitor see only the
//! documents, over the in-memory node of `common`, whose scans skip those versions as a range's do.

mod common;

use common::{Mem, doc, field};
use teseladb_wire::commands::ttl::pass;
use teseladb_bson::{Document, Value};

const DB: &str = "t";

fn arr(v: Vec<Document>) -> Value {
    Value::Array(v.into_iter().map(Value::Document).collect())
}

fn int(d: &Document, path: &str) -> i64 {
    match field(d, path) {
        Value::Int32(n) => i64::from(*n),
        Value::Int64(n) => *n,
        Value::Double(n) => *n as i64,
        other => panic!("{path}: {other:?}"),
    }
}

fn metas(m: &Mem) -> usize {
    m.types.values().filter(|t| **t == teseladb_range::HISTORY_VALUE).count()
}

#[test]
fn documents_with_a_meta_count_once_everywhere() {
    let mut m = Mem::new();
    let at = Value::DateTime(1_000);
    let docs = (1..=3).map(|i| doc(&[("_id", i.into()), ("t", at.clone())])).collect();
    m.run(DB, Document::new().with("insert", "c").with("documents", arr(docs)));
    let set = Document::new().with("$set", doc(&[("a", 1.into())]));
    let update = Document::new().with("q", Document::new()).with("u", set).with("multi", true);
    let r = m.run(DB, Document::new().with("update", "c").with("updates", arr(vec![update])));
    assert_eq!(int(&r, "nModified"), 3, "{r:?}");
    assert_eq!(metas(&m), 3, "the update described each document");
    let r = m.run(DB, Document::new().with("count", "c"));
    assert_eq!(int(&r, "n"), 3);
    let r = m.run(DB, Document::new().with("find", "c"));
    let Value::Array(batch) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    assert_eq!(batch.len(), 3);
    let r = m.run(DB, Document::new().with("collStats", "c"));
    assert_eq!(int(&r, "count"), 3, "{r:?}");
    let r = m.run(DB, Document::new().with("dbStats", 1));
    assert_eq!(int(&r, "objects"), 3, "{r:?}");
    // An index built over the documents has one entry per document.
    let ttl = Document::new()
        .with("key", doc(&[("t", 1.into())]))
        .with("name", "t_ttl")
        .with("expireAfterSeconds", 30);
    let r = m.run(DB, Document::new().with("createIndexes", "c").with("indexes", arr(vec![ttl])));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let entries = m
        .data
        .keys()
        .filter(|k| {
            matches!(teseladb_keys::KeyParts::of(k), Ok(teseladb_keys::KeyParts::Data { index, .. }) if index > 0)
        })
        .count();
    assert_eq!(entries, 3);
    // The TTL monitor deletes the three documents and nothing else.
    let shared = std::mem::take(&mut m.shared);
    assert_eq!(pass(&mut m, &shared, 100_000), Ok(3));
    m.shared = shared;
    let r = m.run(DB, Document::new().with("count", "c"));
    assert_eq!(int(&r, "n"), 0);
}
