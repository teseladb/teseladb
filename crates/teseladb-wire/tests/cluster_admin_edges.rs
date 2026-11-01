//! 007 FR-018, FR-029 and DV-017: the checks of `setDefaultRWConcern`, `fsync`, `compact`,
//! `autoCompact` and `shutdown` that a `refrouter` 9.0.4 (or a `refnode`, for `compact`) makes before
//! running them (measured, 01-11-2026), with the shards of `config.shards` in the answers.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

const ADMIN: &str = "admin";
/// `BadValue`.
const BAD_VALUE: i32 = 2;
/// `TypeMismatch`.
const TYPE_MISMATCH: i32 = 14;
/// `IllegalOperation`.
const ILLEGAL: i32 = 20;
/// `UnknownReplWriteConcern`.
const UNKNOWN_WC: i32 = 79;
/// `IDLUnknownField`.
const UNKNOWN_FIELD: i32 = 40415;

fn ok(r: &Document) -> bool {
    r.get("ok") == Some(&Value::Double(1.0))
}

fn code(r: &Document) -> Option<&Value> {
    r.get("code")
}

/// The answer of shard `name` in `all.raw`, whose names hold dots.
fn shard<'a>(r: &'a Document, name: &str) -> &'a Document {
    let Value::Document(raw) = field(r, "all.raw") else { panic!("{r:?}") };
    match raw.get(name) {
        Some(Value::Document(d)) => d,
        other => panic!("{name}: {other:?}"),
    }
}

fn two_shards() -> Mem {
    let mut m = Mem::new();
    m.shards = vec!["s0".to_owned(), "s1".to_owned()];
    m
}

fn rw(field: &str, v: Value) -> Document {
    doc(&[("setDefaultRWConcern", Value::Int32(1)), (field, v)])
}

#[test]
fn the_default_read_concern_takes_no_point_in_time() {
    let mut m = Mem::new();
    let ts = Value::Timestamp { time: 5, increment: 1 };
    for (name, rc) in [
        (
            "'atClusterTime'",
            doc(&[("level", Value::from("snapshot")), ("atClusterTime", ts.clone())]),
        ),
        ("'afterClusterTime'", doc(&[("afterClusterTime", ts.clone())])),
        (
            "'afterOpTime'",
            doc(&[(
                "afterOpTime",
                Value::Document(doc(&[("ts", ts.clone()), ("t", Value::Int64(1))])),
            )]),
        ),
    ] {
        let r = m.run(ADMIN, rw("defaultReadConcern", Value::Document(rc)));
        assert_eq!(code(&r), Some(&Value::Int32(BAD_VALUE)), "{name}: {r:?}");
        let Some(Value::String(msg)) = r.get("errmsg") else { panic!("{r:?}") };
        assert!(msg.contains(name), "{msg}");
    }
}

#[test]
fn a_tagged_default_write_concern_names_every_shard_and_a_set_one_cannot_be_unset() {
    let mut m = two_shards();
    let tagged = doc(&[("w", Value::from("dc"))]);
    let r = m.run(ADMIN, rw("defaultWriteConcern", Value::Document(tagged)));
    assert_eq!(code(&r), Some(&Value::Int32(UNKNOWN_WC)), "{r:?}");
    let Some(Value::String(msg)) = r.get("errmsg") else { panic!("{r:?}") };
    assert!(msg.ends_with("missing on: 's0', 's1'."), "{msg}");
    // Without `w`: only `j` and `wtimeout` are shown.
    let journal = doc(&[("j", Value::Boolean(true))]);
    let r = m.run(ADMIN, rw("defaultWriteConcern", Value::Document(journal)));
    assert!(ok(&r), "{r:?}");
    assert_eq!(field(&r, "defaultWriteConcern.j"), &Value::Boolean(true));
    assert!(
        matches!(&r.get("defaultWriteConcern"), Some(Value::Document(d)) if d.get("w").is_none())
    );
    let r = m.run(ADMIN, rw("defaultWriteConcern", Value::Document(Document::new())));
    assert_eq!(code(&r), Some(&Value::Int32(ILLEGAL)), "{r:?}");
}

#[test]
fn fsync_answers_for_each_shard_and_its_errors_too() {
    let mut m = two_shards();
    let r = m.run(ADMIN, doc(&[("fsync", Value::Int32(1)), ("comment", Value::from("c"))]));
    assert!(ok(&r), "{r:?}");
    assert_eq!(shard(&r, "s0/s0.cluster:27018").get("ok"), Some(&Value::Int32(1)));
    let r = m.run(ADMIN, doc(&[("fsync", Value::Int32(1)), ("bogus", Value::Int32(1))]));
    assert_eq!(code(&r), Some(&Value::Int32(UNKNOWN_FIELD)), "{r:?}");
    let s1 = shard(&r, "s1/s1.cluster:27018");
    assert_eq!(s1.get("code"), Some(&Value::Int32(UNKNOWN_FIELD)));
}

#[test]
fn compact_auto_compact_and_shutdown_take_the_generic_arguments() {
    let mut m = Mem::new();
    let comment = ("comment", Value::from("c"));
    // `compact` of a name that is no string, once forced.
    let r = m.run(
        "db",
        doc(&[("compact", Value::Int32(1)), ("force", Value::Boolean(true)), comment.clone()]),
    );
    assert_eq!(code(&r), Some(&Value::Int32(TYPE_MISMATCH)), "{r:?}");
    let r = m.run("db", doc(&[("compact", Value::from("c")), ("dryRun", Value::Int32(1))]));
    assert_eq!(code(&r), Some(&Value::Int32(TYPE_MISMATCH)), "{r:?}");
    let r = m.run(ADMIN, doc(&[("autoCompact", Value::Boolean(true)), comment.clone()]));
    assert!(ok(&r), "{r:?}");
    let r =
        m.run(ADMIN, doc(&[("autoCompact", Value::Boolean(true)), ("runOnce", Value::Int32(1))]));
    assert_eq!(code(&r), Some(&Value::Int32(TYPE_MISMATCH)), "{r:?}");
    // This backend cannot stop: the checks pass and the node refuses.
    let r = m.run(ADMIN, doc(&[("shutdown", Value::Int32(1)), comment]));
    assert_eq!(code(&r), Some(&Value::Int32(ILLEGAL)), "{r:?}");
}
