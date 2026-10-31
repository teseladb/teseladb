//! A stored document or catalog entry that does not parse is a fault the command reports, not a
//! document it skips or options it reads as empty (006 audit, pass 1).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

fn s(x: &str) -> Value {
    Value::from(x)
}

fn failed(r: &Document) -> bool {
    field(r, "ok") == &Value::Double(0.0)
        || matches!(r.get("writeErrors"), Some(Value::Array(e)) if !e.is_empty())
}

#[test]
fn a_document_that_does_not_parse_fails_reads_and_writes() {
    let mut m = Mem::new();
    let docs = Value::Array(vec![Value::Document(doc(&[("_id", 1.into()), ("a", 1.into())]))]);
    m.run("db", doc(&[("insert", s("c")), ("documents", docs)]));
    for v in m.data.values_mut() {
        // A length prefix past the end of the bytes.
        v[0] = 0xff;
    }
    let all = Value::Document(Document::new());
    let set = Value::Document(doc(&[("$set", Value::Document(doc(&[("b", 1.into())])))]));
    let multi = Value::Array(vec![Value::Document(doc(&[
        ("q", all.clone()),
        ("u", set),
        ("multi", true.into()),
    ]))]);
    for cmd in [
        doc(&[("find", s("c"))]),
        doc(&[("count", s("c"))]),
        doc(&[("update", s("c")), ("updates", multi)]),
    ] {
        let r = m.run("db", cmd.clone());
        assert!(failed(&r), "{cmd:?}: {r:?}");
    }
}

#[test]
fn options_that_do_not_parse_fail_the_commands_that_read_them() {
    let mut m = Mem::new();
    m.run("db", doc(&[("create", s("c"))]));
    for c in m.catalog.collections.values_mut() {
        c.options = vec![9, 0, 0, 0];
    }
    for cmd in [
        doc(&[("listCollections", 1.into())]),
        doc(&[("listIndexes", s("c"))]),
        doc(&[("collStats", s("c"))]),
        doc(&[
            ("insert", s("c")),
            ("documents", Value::Array(vec![Value::Document(Document::new())])),
        ]),
    ] {
        let r = m.run("db", cmd.clone());
        assert!(failed(&r), "{cmd:?}: {r:?}");
    }
}
