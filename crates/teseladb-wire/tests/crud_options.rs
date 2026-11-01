//! Options of the write commands at their edges (001 FR-007, FR-011): an empty `sort` of an
//! update, `upsertSupplied` without `upsert`, an empty validator, and a validator that only warns,
//! whose failures the node logs as a `refnode` does (id 20294, «Document would fail validation»).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

const DB: &str = "db";
/// `FailedToParse`.
const FAILED_TO_PARSE: i32 = 9;
/// Log id of a document that would fail a validator that only warns.
const LOG_WOULD_FAIL: i32 = 20294;

fn ok(r: &Document) -> bool {
    r.get("ok") == Some(&Value::Double(1.0))
}

fn update(st: Document) -> Document {
    doc(&[("update", Value::from("c")), ("updates", Value::Array(vec![Value::Document(st)]))])
}

fn set_x() -> Document {
    doc(&[("$set", Value::Document(doc(&[("x", Value::Int32(2))])))])
}

#[test]
fn an_empty_sort_takes_any_document_and_upsert_supplied_needs_upsert() {
    let mut m = Mem::new();
    let one = Value::Document(doc(&[("x", Value::Int32(1))]));
    let r = m.run(DB, doc(&[("insert", Value::from("c")), ("documents", Value::Array(vec![one]))]));
    assert!(ok(&r), "{r:?}");
    let st = doc(&[
        ("q", Value::Document(Document::new())),
        ("u", Value::Document(set_x())),
        ("sort", Value::Document(Document::new())),
    ]);
    let r = m.run(DB, update(st));
    assert_eq!(r.get("nModified"), Some(&Value::Int32(1)), "{r:?}");
    let st = doc(&[
        ("q", Value::Document(Document::new())),
        ("u", Value::Document(set_x())),
        ("upsertSupplied", Value::Boolean(true)),
    ]);
    let r = m.run(DB, update(st));
    let code =
        r.get("code").or_else(|| r.get("writeErrors").map(|_| field(&r, "writeErrors.0.code")));
    assert_eq!(code, Some(&Value::Int32(FAILED_TO_PARSE)), "{r:?}");
}

#[test]
fn a_validator_that_only_warns_logs_the_document_and_an_empty_one_checks_nothing() {
    let mut m = Mem::new();
    let schema = doc(&[("x", Value::Document(doc(&[("$type", Value::from("string"))])))]);
    let r = m.run(
        DB,
        doc(&[
            ("create", Value::from("w")),
            ("validator", Value::Document(schema)),
            ("validationAction", Value::from("warn")),
        ]),
    );
    assert!(ok(&r), "{r:?}");
    let bad = Value::Document(doc(&[("x", Value::Int32(1))]));
    let r = m.run(DB, doc(&[("insert", Value::from("w")), ("documents", Value::Array(vec![bad]))]));
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    let (id, msg, attr) = m.logged.last().expect("a warning").clone();
    assert_eq!((id, msg.as_str()), (LOG_WOULD_FAIL, "Document would fail validation"));
    assert_eq!(attr.get("namespace"), Some(&Value::from("db.w")));
    // `validator: {}` is none: `listCollections` shows no validator.
    let r = m.run(
        DB,
        doc(&[("create", Value::from("e")), ("validator", Value::Document(Document::new()))]),
    );
    assert!(ok(&r), "{r:?}");
    let filter = doc(&[("name", Value::from("e"))]);
    let r = m
        .run(DB, doc(&[("listCollections", Value::Int32(1)), ("filter", Value::Document(filter))]));
    let Value::Document(options) = field(&r, "cursor.firstBatch.0.options") else {
        panic!("{r:?}")
    };
    assert!(options.get("validationAction").is_none(), "{options:?}");
}
