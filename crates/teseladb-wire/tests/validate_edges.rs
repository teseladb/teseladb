//! `validate` beyond the replies of `tests/validate/*.out` (010 FR-017), measured on a `refnode`
//! 9.0.4 replica set on 02-11-2026: `background: true` alone and with `metadata`, a descending
//! multikey index and a wildcard one, a capped collection and a time series collection.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

const DB: &str = "test";

fn run(m: &mut Mem, cmd: &str) -> Document {
    m.run(DB, extjson::parse(cmd).expect("command"))
}

fn ok(m: &mut Mem, cmd: &str) -> Document {
    let r = run(m, cmd);
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{cmd}: {r:?}");
    r
}

fn keys(r: &Document) -> Vec<(String, Value)> {
    let Value::Document(k) = field(r, "keysPerIndex") else { panic!("{r:?}") };
    k.iter().map(|(n, v)| (n.to_owned(), v.clone())).collect()
}

fn loaded() -> Mem {
    let mut m = Mem::new();
    ok(&mut m, r#"{"insert":"c","documents":[{"_id":1,"a":[1,2],"w":{"x":1}},{"_id":2,"a":3}]}"#);
    ok(&mut m, r#"{"createIndexes":"c","indexes":[{"key":{"a":-1},"name":"a_-1"}]}"#);
    ok(&mut m, r#"{"createIndexes":"c","indexes":[{"key":{"w.$**":1},"name":"w.$**_1"}]}"#);
    m
}

/// `{background: true}` answers like a full check of the indexes plus the time it read at:
/// `{_id_: 2, "a_-1": 3, "w.$**_1": 1}` and a `readTimestamp`.
#[test]
fn background_validate_reads_at_a_timestamp() {
    let mut m = loaded();
    let r = ok(&mut m, r#"{"validate":"c","background":true}"#);
    assert!(matches!(r.get("readTimestamp"), Some(Value::Timestamp { .. })), "{r:?}");
    let want: Vec<(String, Value)> =
        vec![("_id_".into(), 2.into()), ("a_-1".into(), 3.into()), ("w.$**_1".into(), 1.into())];
    assert_eq!(keys(&r), want);
    assert_eq!(field(&r, "valid"), &Value::Boolean(true));
    assert_eq!(field(&r, "indexDetails.a_-1.isMultikey"), &Value::Boolean(true));
}

/// `metadata` with `background` fails with 72 like with the other options; without `background`
/// the reply has no `readTimestamp`.
#[test]
fn metadata_refuses_background() {
    let mut m = loaded();
    let r = run(&mut m, r#"{"validate":"c","metadata":true,"background":true}"#);
    assert_eq!(r.get("code"), Some(&Value::Int32(72)), "{r:?}");
    assert_eq!(
        r.get("errmsg"),
        Some(&Value::from(
            "Running the validate command with { metadata: true } is not supported with any \
             other options"
        ))
    );
    let r = m.run(DB, doc(&[("validate", Value::from("c"))]));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    assert!(r.get("readTimestamp").is_none(), "{r:?}");
}

/// A capped collection validates with its `_id` index: one key per document, and no entry
/// reported missing or extra (its `_id_` entries are written unique, as a `refnode` keeps them).
#[test]
fn capped_collection_validates() {
    let mut m = Mem::new();
    ok(&mut m, r#"{"create":"cap","capped":true,"size":4096}"#);
    ok(&mut m, r#"{"insert":"cap","documents":[{"_id":1},{"_id":2}]}"#);
    let r = ok(&mut m, r#"{"validate":"cap"}"#);
    assert_eq!(keys(&r), vec![("_id_".to_owned(), Value::Int32(2))]);
    assert_eq!(field(&r, "nIndexes"), &Value::Int32(1));
    assert_eq!(field(&r, "valid"), &Value::Boolean(true), "{r:?}");
}

/// A time series collection has no `_id` index: `nIndexes: 0` and empty `keysPerIndex`.
#[test]
fn time_series_validates_without_id_index() {
    let mut m = Mem::new();
    ok(&mut m, r#"{"create":"ts","timeseries":{"timeField":"t"}}"#);
    ok(&mut m, r#"{"insert":"ts","documents":[{"t":{"$date":"2026-11-02T00:00:00Z"}}]}"#);
    let r = ok(&mut m, r#"{"validate":"ts"}"#);
    assert_eq!(field(&r, "nIndexes"), &Value::Int32(0));
    assert_eq!(keys(&r), vec![]);
    assert_eq!(field(&r, "valid"), &Value::Boolean(true));
    assert_eq!(field(&r, "nrecords"), &Value::Int32(1));
}
