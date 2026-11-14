//! A read through an index whose document went away between the index entry and the document: reads
//! that are not at a snapshot read the index and the documents separately, so a delete in between
//! leaves an entry without its document. The read goes on without it, as the reference server skips
//! a record deleted while the read yields (found by 018 T013 with a client writing during a copy:
//! `an index entry without its document`).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

#[test]
fn a_document_deleted_after_its_index_entry_is_skipped() {
    let mut m = Mem::new();
    let docs: Vec<Value> = (1..=3)
        .map(|i| Value::Document(Document::new().with("_id", i).with("k", i * 10)))
        .collect();
    let r = m.run("db", doc(&[("insert", "c".into()), ("documents", Value::Array(docs))]));
    assert_eq!(r.get("n"), Some(&Value::Int32(3)), "{r:?}");
    let k = Document::new().with("key", Document::new().with("k", 1)).with("name", "k_1");
    let r = m.run(
        "db",
        doc(&[("createIndexes", "c".into()), ("indexes", Value::Array(vec![k.into()]))]),
    );
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let gone = Document::new().with("_id", 2).with("k", 20).to_bytes().expect("bytes");
    let key = m
        .data
        .iter()
        .find(|(_, v)| teseladb_keys::stored::bson(v).is_ok_and(|b| b == gone.as_slice()))
        .map(|(k, _)| k.clone())
        .expect("the document's key");
    m.data.remove(&key);
    let find = doc(&[
        ("find", "c".into()),
        ("sort", Value::Document(Document::new().with("k", 1))),
        ("hint", Value::Document(Document::new().with("k", 1))),
    ]);
    let r = m.run("db", find);
    let ids: Vec<Value> = match field(&r, "cursor.firstBatch") {
        Value::Array(a) => a
            .iter()
            .map(|d| match d {
                Value::Document(d) => d.get("_id").cloned().expect("an _id"),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{r:?} {other:?}"),
    };
    assert_eq!(ids, [Value::Int32(1), Value::Int32(3)]);
}
