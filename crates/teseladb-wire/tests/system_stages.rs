//! The stages a node answers itself refuse the namespaces they do not run on (010 T010, T033):
//! `$currentOp` only with `aggregate: 1` on `admin`, `$listSessions` only on
//! `config.system.sessions`.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

/// `InvalidNamespace`.
const INVALID_NAMESPACE: i32 = 73;

fn aggregate(target: Value, stage: &str) -> Document {
    let stage = doc(&[(stage, Value::Document(Document::new()))]);
    doc(&[
        ("aggregate", target),
        ("pipeline", Value::Array(vec![Value::Document(stage)])),
        ("cursor", Value::Document(Document::new())),
    ])
}

#[test]
fn current_op_on_a_collection_of_admin_is_refused() {
    let mut m = Mem::new();
    let r = m.run("admin", aggregate(Value::from("c"), "$currentOp"));
    assert_eq!(r.get("code"), Some(&Value::Int32(INVALID_NAMESPACE)), "{r:?}");
    let r = m.run("admin", aggregate(Value::Int32(1), "$currentOp"));
    assert!(matches!(field(&r, "cursor.firstBatch"), Value::Array(_)), "{r:?}");
}

#[test]
fn list_sessions_runs_on_the_sessions_collection_only() {
    let mut m = Mem::new();
    let r = m.run("db", aggregate(Value::from("c"), "$listSessions"));
    assert_eq!(r.get("code"), Some(&Value::Int32(INVALID_NAMESPACE)), "{r:?}");
    assert_eq!(
        r.get("errmsg"),
        Some(&Value::from(teseladb_exec::pipeline::LIST_SESSIONS_NAMESPACE))
    );
    let r = m.run("config", aggregate(Value::from("system.sessions"), "$listSessions"));
    assert!(matches!(field(&r, "cursor.firstBatch"), Value::Array(_)), "{r:?}");
}
