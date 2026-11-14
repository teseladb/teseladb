//! `retriedStmtIds` (FR-017, 006 audit pass 1): a retryable write sent again with the same `lsid`
//! and `txnNumber` gets the stored result of each statement, and the reply lists those statements
//! as the reference router 9.0.4 does (measured on 30-10-2026): `insert`, `update` and `delete`
//! when there are any, `bulkWrite` always.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

fn s(x: &str) -> Value {
    Value::from(x)
}

fn d(pairs: &[(&str, Value)]) -> Value {
    Value::Document(doc(pairs))
}

fn retryable(cmd: Document, lsid: u8) -> Document {
    cmd.with("lsid", d(&[("id", Value::Binary { subtype: 4, bytes: vec![lsid; 16] })]))
        .with("txnNumber", Value::Int64(7))
}

fn ids(r: &Document) -> Option<Vec<Value>> {
    match r.get("retriedStmtIds") {
        Some(Value::Array(a)) => Some(a.clone()),
        None => None,
        Some(other) => panic!("{other:?}"),
    }
}

#[test]
fn statements_answered_from_an_earlier_send_are_listed() {
    let mut m = Mem::new();
    let docs = Value::Array(vec![d(&[("_id", 1.into())]), d(&[("_id", 2.into())])]);
    let insert = retryable(doc(&[("insert", s("c")), ("documents", docs)]), 1);
    let first = m.run("db", insert.clone());
    assert_eq!(ids(&first), None);
    let again = m.run("db", insert);
    assert_eq!(field(&again, "n"), &Value::Int32(2));
    assert_eq!(ids(&again), Some(vec![Value::Int32(0), Value::Int32(1)]));

    let set = d(&[("$set", d(&[("a", 1.into())]))]);
    let update = retryable(
        doc(&[
            ("update", s("c")),
            ("updates", Value::Array(vec![d(&[("q", d(&[("_id", 1.into())])), ("u", set)])])),
        ]),
        2,
    );
    assert_eq!(ids(&m.run("db", update.clone())), None);
    let again = m.run("db", update);
    assert_eq!(ids(&again), Some(vec![Value::Int32(0)]));
    let keys: Vec<&str> = again.iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["nModified", "n", "retriedStmtIds", "ok"]);

    let ops = Value::Array(vec![d(&[("delete", 0.into()), ("filter", d(&[("_id", 2.into())]))])]);
    let bulk = retryable(
        doc(&[
            ("bulkWrite", 1.into()),
            ("ops", ops),
            ("nsInfo", Value::Array(vec![d(&[("ns", s("db.c"))])])),
        ]),
        3,
    );
    assert_eq!(ids(&m.run("admin", bulk.clone())), Some(vec![]));
    assert_eq!(ids(&m.run("admin", bulk)), Some(vec![Value::Int32(0)]));

    // A statement that failed is not listed: it ran again.
    let docs = Value::Array(vec![d(&[("_id", 9.into())]), d(&[("_id", 9.into())])]);
    let dup =
        retryable(doc(&[("insert", s("c")), ("documents", docs), ("ordered", false.into())]), 4);
    m.run("db", dup.clone());
    let again = m.run("db", dup);
    assert_eq!(ids(&again), Some(vec![Value::Int32(0)]));
    let keys: Vec<&str> = again.iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["n", "writeErrors", "retriedStmtIds", "ok"]);

    // Without a session nothing is retryable, and `bulkWrite` still lists none.
    let ops = Value::Array(vec![d(&[("insert", 0.into()), ("document", d(&[]))])]);
    let plain = doc(&[
        ("bulkWrite", 1.into()),
        ("ops", ops),
        ("nsInfo", Value::Array(vec![d(&[("ns", s("db.c"))])])),
    ]);
    assert_eq!(ids(&m.run("admin", plain)), Some(vec![]));
}

/// A retryable `bulkWrite` insert, into a collection and into a capped one, sent again lists its
/// statements and writes nothing more (as the deletes above, measured on 30-10-2026).
#[test]
fn bulk_write_inserts_sent_again_are_listed() {
    let mut m = Mem::new();
    let r =
        m.run("db", doc(&[("create", s("cap")), ("capped", true.into()), ("size", 4096.into())]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let ops = Value::Array(vec![
        d(&[("insert", 0.into()), ("document", d(&[("_id", 1.into())]))]),
        d(&[("insert", 1.into()), ("document", d(&[("_id", 2.into())]))]),
    ]);
    let ns = Value::Array(vec![d(&[("ns", s("db.c"))]), d(&[("ns", s("db.cap"))])]);
    let bulk = retryable(doc(&[("bulkWrite", 1.into()), ("ops", ops), ("nsInfo", ns)]), 5);
    let first = m.run("admin", bulk.clone());
    assert_eq!((field(&first, "nInserted"), ids(&first)), (&Value::Int32(2), Some(vec![])));
    let again = m.run("admin", bulk);
    assert_eq!(ids(&again), Some(vec![Value::Int32(0), Value::Int32(1)]), "{again:?}");
    for coll in ["c", "cap"] {
        let r = m.run("db", doc(&[("count", s(coll))]));
        assert_eq!(field(&r, "n"), &Value::Int32(1), "{coll}: {r:?}");
    }
}
