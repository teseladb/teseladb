//! A time series lives in one namespace (015 FR-006, T022), as a `refnode` 9.0.4 answered on
//! 02-11-2026: `listCollections` gives it with type `timeseries` and no bucket collection, a
//! change stream on it is refused, a `2dsphere` index on the meta value refuses what is no
//! geometry with 510, a `2d` index is refused, and it renames into another database but not into
//! a bucket namespace.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

const T0: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn date(ms: i64) -> String {
    format!(r#"{{"$date":{{"$numberLong":"{ms}"}}}}"#)
}

fn series(m: &mut Mem) {
    let r = m.run("x4", json(r#"{"create":"s","timeseries":{"timeField":"t","metaField":"m"}}"#));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let one = format!(r#"{{"insert":"s","documents":[{{"t":{},"m":1,"loc":[1,2]}}]}}"#, date(T0));
    assert_eq!(field(&m.run("x4", json(&one)), "n"), &Value::Int32(1));
}

#[test]
fn list_collections_gives_the_series_alone() {
    let mut m = Mem::new();
    series(&mut m);
    let r = m.run(
        "x4",
        doc(&[("listCollections", Value::Int32(1)), ("nameOnly", Value::Boolean(true))]),
    );
    let want = json(r#"{"b":[{"name":"s","type":"timeseries"}]}"#);
    assert_eq!(field(&r, "cursor.firstBatch"), want.get("b").unwrap(), "{r:?}");
}

#[test]
fn a_change_stream_on_a_time_series_is_refused() {
    let mut m = Mem::new();
    series(&mut m);
    let r = m.run("x4", json(r#"{"aggregate":"s","pipeline":[{"$changeStream":{}}],"cursor":{}}"#));
    assert_eq!(field(&r, "code"), &Value::Int32(115), "{r:?}");
    assert_eq!(
        field(&r, "errmsg"),
        &Value::from("Cannot run aggregation on timeseries with namespace x4.s"),
        "{r:?}"
    );
}

#[test]
fn a_meta_geometry_index_refuses_with_510_and_2d_is_refused() {
    let mut m = Mem::new();
    series(&mut m);
    let r =
        m.run("x4", json(r#"{"createIndexes":"s","indexes":[{"key":{"loc":"2d"},"name":"l2d"}]}"#));
    assert_eq!(field(&r, "code"), &Value::Int32(67), "{r:?}");
    assert_eq!(
        field(&r, "errmsg"),
        &Value::from(
            "BadValue: Invalid index spec for time-series collection: { loc: \"2d\" }. Indexes on measurement fields must be ascending or descending (numbers only), or '2dsphere': loc: \"2d\""
        ),
        "{r:?}"
    );
    let r = m.run(
        "x4",
        json(r#"{"createIndexes":"s","indexes":[{"key":{"m.g":"2dsphere"},"name":"mg"}]}"#),
    );
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    for (g, why, element) in [
        (r#""x""#, "geo element must be an array or object, got string", r#"{"element":"x"}"#),
        (
            r#"{"type":"Point","coordinates":[500,1]}"#,
            "Longitude/latitude is out of bounds, lng: 500 lat: 1",
            r#"{"coordinates":[500,1],"type":"Point"}"#,
        ),
    ] {
        let cmd =
            format!(r#"{{"insert":"s","documents":[{{"t":{},"m":{{"g":{g}}}}}]}}"#, date(T0 + 2));
        let r = m.run("x4", json(&cmd));
        assert_eq!(field(&r, "n"), &Value::Int32(0), "{r:?}");
        assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(510), "{r:?}");
        let Value::String(msg) = field(&r, "writeErrors.0.errmsg") else { panic!("{r:?}") };
        assert!(
            msg.starts_with(
                "Could not extract geo keys at path 'meta.g' for document with _id ObjectId('"
            ),
            "{msg}"
        );
        assert!(msg.ends_with(&format!("'): {why}")), "{msg}");
        let info = format!(
            r#"{{"failingPath":"meta.g","underlyingCode":2,"underlyingReason":"{why}","failingElement":{element}}}"#
        );
        assert_eq!(field(&r, "writeErrors.0.errInfo"), &Value::Document(json(&info)), "{r:?}");
    }
}

#[test]
fn a_time_series_renames_into_another_database_but_not_into_buckets() {
    let mut m = Mem::new();
    series(&mut m);
    let r = m.run("admin", json(r#"{"renameCollection":"x4.s","to":"x4.system.buckets.s"}"#));
    assert_eq!(field(&r, "code"), &Value::Int32(20), "{r:?}");
    assert_eq!(
        field(&r, "errmsg"),
        &Value::from(
            "Cannot rename non timeseries buckets collection 'x4.s' to a timeseries buckets namespace 'x4.system.buckets.s'."
        ),
        "{r:?}"
    );
    let r = m.run("admin", json(r#"{"renameCollection":"x4.none","to":"x4.system.foo"}"#));
    assert_eq!(field(&r, "code"), &Value::Int32(20), "{r:?}");
    assert_eq!(
        field(&r, "errmsg"),
        &Value::from("error with target namespace: Invalid system namespace: x4.system.foo"),
        "{r:?}"
    );
    let r = m.run("admin", json(r#"{"renameCollection":"x4.s","to":"x5.s"}"#));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let r = m.run("x5", json(r#"{"count":"s"}"#));
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    let r = m.run("x5", json(r#"{"listCollections":1,"nameOnly":true}"#));
    let want = json(r#"{"b":[{"name":"s","type":"timeseries"}]}"#);
    assert_eq!(field(&r, "cursor.firstBatch"), want.get("b").unwrap(), "{r:?}");
}
