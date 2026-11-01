//! Inserts of time series measurements in batches (015 FR-006, FR-008, T023), as a `refnode`
//! 9.0.4 answered them on 02-11-2026: the measurements of one bucket go in together or fail
//! together, a measurement that changes the type of a field closes its bucket
//! (`numBucketsClosedDueToSchemaChange`), and an ordered insert stops at its first failure.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

const T0: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, cmd: &str) -> Document {
    m.run("g", json(cmd))
}

fn date(ms: i64) -> String {
    format!(r#"{{"$date":{{"$numberLong":"{ms}"}}}}"#)
}

/// A measurement at `T0 + i` ms of meta `m` with field `loc` = `loc`.
fn at(i: i64, m: i32, loc: &str) -> String {
    format!(r#"{{"t":{},"m":{m},"loc":{loc}}}"#, date(T0 + i))
}

/// A point `[x, 1]`: out of bounds at `x` = 500.
fn p(i: i64, x: i32) -> String {
    at(i, 1, &format!(r#"{{"type":"Point","coordinates":[{x},1]}}"#))
}

/// A legacy pair `[x, 1]`: an array, where a point is an object.
fn l(i: i64, x: i32) -> String {
    at(i, 1, &format!("[{x},1]"))
}

fn x(i: i64, m: i32) -> String {
    at(i, m, r#""x""#)
}

/// A fresh time series `name`, with a `2dsphere` index on `loc` when `geo`.
fn series(m: &mut Mem, name: &str, geo: bool) {
    let r = run(
        m,
        &format!(r#"{{"create":"{name}","timeseries":{{"timeField":"t","metaField":"m"}}}}"#),
    );
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    if geo {
        let r = run(
            m,
            &format!(
                r#"{{"createIndexes":"{name}","indexes":[{{"key":{{"loc":"2dsphere"}},"name":"g"}}]}}"#
            ),
        );
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    }
}

/// `n`, the places of the write errors in their order, and the errors' messages.
fn insert(m: &mut Mem, name: &str, docs: &[String], ordered: bool) -> (i32, Vec<i32>, Vec<String>) {
    let r = run(
        m,
        &format!(r#"{{"insert":"{name}","ordered":{ordered},"documents":[{}]}}"#, docs.join(",")),
    );
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let Value::Int32(n) = field(&r, "n") else { panic!("{r:?}") };
    let errors = match r.get("writeErrors") {
        Some(Value::Array(e)) => e.clone(),
        _ => Vec::new(),
    };
    let mut at = Vec::new();
    let mut msgs = Vec::new();
    for e in &errors {
        let Value::Document(e) = e else { panic!("{r:?}") };
        let Value::Int32(i) = field(e, "index") else { panic!("{r:?}") };
        let Value::String(msg) = field(e, "errmsg") else { panic!("{r:?}") };
        at.push(*i);
        msgs.push(msg.clone());
    }
    (*n, at, msgs)
}

/// `bucketCount` and `numBucketsClosedDueToSchemaChange` of `name`.
fn buckets(m: &mut Mem, name: &str) -> (i32, i32) {
    let r = m.run("g", doc(&[("collStats", Value::from(name))]));
    let get = |k: &str| match field(&r, &format!("timeseries.{k}")) {
        Value::Int32(n) => *n,
        Value::Int64(n) => i32::try_from(*n).expect("small"),
        other => panic!("{k}: {other:?}"),
    };
    (get("bucketCount"), get("numBucketsClosedDueToSchemaChange"))
}

/// Measurements, ordered, `n`, the errors in order, and `bucketCount` with the buckets closed
/// by a type change.
type Case = (Vec<String>, bool, i32, Vec<i32>, (i32, i32));

const GEO: &str = "Could not extract geo keys at path 'data.loc' for document with _id ObjectId('";
const UPDATE: &str = "Plan executor error during update :: caused by :: ";

#[test]
fn the_measurements_of_one_bucket_go_in_or_fail_together() {
    // (measurements, ordered, n, errors in order, buckets, closed by a type change)
    let cases: Vec<Case> = vec![
        (vec![p(0, 1), p(1, 500), p(2, 2)], false, 0, vec![0, 1, 2], (0, 0)),
        (vec![p(0, 500), p(1, 1)], false, 0, vec![0, 1], (0, 0)),
        (vec![p(0, 1), p(1, 500)], false, 0, vec![0, 1], (0, 0)),
        (vec![p(0, 1), x(1, 1), p(2, 2)], false, 1, vec![1, 2], (1, 1)),
        (vec![p(0, 1), x(1, 1), p(2, 2)], true, 1, vec![1], (1, 1)),
        (vec![p(0, 1), p(1, 500), p(2, 2)], true, 1, vec![1], (1, 0)),
        (vec![p(0, 1), x(1, 1), p(2, 500), p(3, 3)], false, 1, vec![1, 2, 3], (1, 1)),
        (vec![p(0, 1), l(1, 1), p(2, 2)], false, 3, vec![], (3, 2)),
        (vec![p(0, 1), l(1, 1), p(2, 500)], false, 2, vec![2], (2, 2)),
        (vec![p(0, 1), l(1, 1), p(2, 2), l(3, 500)], false, 3, vec![3], (3, 3)),
        (vec![p(0, 1), l(1, 500), p(2, 2)], false, 1, vec![1, 2], (1, 1)),
        (vec![p(0, 500), l(1, 1), p(2, 2)], false, 0, vec![0, 1, 2], (0, 0)),
        (vec![p(0, 1), l(1, 1), p(2, 2)], true, 3, vec![], (3, 2)),
        // Each meta value its own batch: the errors of one batch, then those of the next.
        (vec![p(0, 1), x(1, 2), p(2, 500)], false, 0, vec![0, 2, 1], (0, 0)),
        // A measurement without its date fails before the batches.
        (vec![p(0, 500), r#"{"m":1}"#.to_owned(), p(2, 1)], false, 0, vec![1, 0, 2], (0, 0)),
        // An ordered insert stops at its first failure, whatever bucket comes after.
        (
            vec![p(0, 1), at(1, 2, r#"{"type":"Point","coordinates":[500,1]}"#), at(2, 1, "[2,1]")],
            true,
            1,
            vec![1],
            (1, 0),
        ),
    ];
    for (k, (docs, ordered, n, errors, counted)) in cases.into_iter().enumerate() {
        let mut m = Mem::new();
        series(&mut m, "s", true);
        let (got, at, msgs) = insert(&mut m, "s", &docs, ordered);
        assert_eq!((got, &at), (n, &errors), "case {k}: {msgs:?}");
        for msg in &msgs {
            assert!(
                msg.starts_with(GEO) || msg.starts_with(UPDATE) || msg.starts_with("'t'"),
                "{k}: {msg}"
            );
        }
        assert_eq!(buckets(&mut m, "s"), counted, "case {k}");
    }
}

#[test]
fn a_measurement_refused_into_a_bucket_already_there_fails_as_an_update() {
    let mut m = Mem::new();
    series(&mut m, "s", true);
    assert_eq!(insert(&mut m, "s", &[p(0, 1)], false).0, 1);
    let (n, at, msgs) = insert(&mut m, "s", &[p(1, 500)], false);
    assert_eq!((n, at), (0, vec![0]));
    assert!(msgs[0].starts_with(&format!("{UPDATE}{GEO}")), "{}", msgs[0]);
    let (n, at, msgs) = insert(&mut m, "s", &[p(2, 2), p(3, 500)], false);
    assert_eq!((n, at), (0, vec![0, 1]));
    assert!(msgs.iter().all(|e| e.starts_with(&format!("{UPDATE}{GEO}"))), "{msgs:?}");
    // In order, those before the failure go in; it fails as an update of their bucket.
    let mut m = Mem::new();
    series(&mut m, "o", true);
    let (n, at, msgs) = insert(&mut m, "o", &[p(0, 1), p(1, 2), p(2, 500), p(3, 3)], true);
    assert_eq!((n, at), (2, vec![2]));
    assert!(msgs[0].starts_with(&format!("{UPDATE}{GEO}")), "{}", msgs[0]);
    assert!(
        msgs[0].ends_with("Longitude/latitude is out of bounds, lng: 500 lat: 1"),
        "{}",
        msgs[0]
    );
    // A new bucket's first measurement fails as an insert.
    let (_, _, msgs) = insert(&mut m, "o", &[x(10, 3)], true);
    assert!(msgs[0].starts_with(GEO), "{}", msgs[0]);
}

#[test]
fn a_field_of_another_type_closes_the_bucket() {
    let d = |i: i64, x: &str| format!(r#"{{"t":{},"m":1,"x":{x}}}"#, date(T0 + i));
    let cases: Vec<(&str, Vec<String>, (i32, i32))> = vec![
        (
            "numbers",
            vec![
                d(0, "1"),
                d(1, "1.5"),
                d(2, r#"{"$numberLong":"3"}"#),
                d(3, r#"{"$numberDecimal":"1"}"#),
            ],
            (1, 0),
        ),
        ("null", vec![d(0, r#""a""#), d(1, "null"), d(2, r#""b""#)], (3, 2)),
        (
            "missing",
            vec![d(0, "1"), format!(r#"{{"t":{},"m":1}}"#, date(T0 + 1)), d(2, "1")],
            (1, 0),
        ),
        ("nested", vec![d(0, r#"{"a":1}"#), d(1, r#"{"a":"s"}"#), d(2, r#"{"b":"s"}"#)], (2, 1)),
        ("arrays", vec![d(0, "[1]"), d(1, r#"["s"]"#), d(2, r#"[1,"s"]"#), d(3, "[]")], (3, 2)),
        (
            "scalars",
            vec![
                d(0, "true"),
                d(1, &date(0)),
                d(2, r#"{"$oid":"6955b90001122b8546e073d1"}"#),
                d(3, r#"{"$timestamp":{"t":1,"i":1}}"#),
            ],
            (4, 3),
        ),
        ("keys", vec![d(0, r#"{"$minKey":1}"#), d(1, r#"{"$maxKey":1}"#)], (2, 1)),
        (
            "array of objects",
            vec![d(0, r#"{"a":[1]}"#), d(1, r#"{"a":[{"b":1}]}"#), d(2, r#"{"a":[{"b":"s"}]}"#)],
            (3, 2),
        ),
    ];
    for (what, docs, counted) in cases {
        let mut m = Mem::new();
        series(&mut m, "s", false);
        let (n, at, _) = insert(&mut m, "s", &docs, false);
        assert_eq!((n, at), (i32::try_from(docs.len()).expect("few"), vec![]), "{what}");
        assert_eq!(buckets(&mut m, "s"), counted, "{what}");
    }
    // Other meta values keep their buckets.
    let mut m = Mem::new();
    series(&mut m, "s", false);
    let other = format!(r#"{{"t":{},"m":"s","x":1}}"#, date(T0 + 1));
    insert(&mut m, "s", &[d(0, "1"), other], false);
    assert_eq!(buckets(&mut m, "s"), (2, 0));
    // A later insert goes to the open bucket, not back to the one closed.
    let mut m = Mem::new();
    series(&mut m, "s", false);
    for doc in [d(0, "1"), d(1, r#""s""#), d(2, "2")] {
        assert_eq!(insert(&mut m, "s", &[doc], true).0, 1);
    }
    assert_eq!(buckets(&mut m, "s"), (3, 2));
}

/// The counters of `collStats.timeseries` named `names`.
fn counters(m: &mut Mem, name: &str, names: &[&str]) -> Vec<i64> {
    let r = m.run("g", doc(&[("collStats", Value::from(name))]));
    names
        .iter()
        .map(|k| match field(&r, &format!("timeseries.{k}")) {
            Value::Int32(n) => i64::from(*n),
            Value::Int64(n) => *n,
            other => panic!("{k}: {other:?}"),
        })
        .collect()
}

const COUNTED: [&str; 6] = [
    "bucketCount",
    "numCommits",
    "numBucketInserts",
    "numBucketUpdates",
    "numBucketsClosedDueToCount",
    "numMeasurementsCommitted",
];

#[test]
fn one_insert_past_a_bucket_count_fills_two_buckets() {
    let mut m = Mem::new();
    series(&mut m, "s", false);
    let docs: Vec<String> =
        (0..1005).map(|i| format!(r#"{{"t":{},"m":1,"v":{i}}}"#, date(T0 + i))).collect();
    assert_eq!(insert(&mut m, "s", &docs, true).0, 1005);
    assert_eq!(counters(&mut m, "s", &COUNTED), vec![2, 2, 2, 0, 1, 1005]);
}

#[test]
fn a_retried_insert_commits_nothing_again() {
    let mut m = Mem::new();
    series(&mut m, "r", false);
    let cmd = format!(
        r#"{{"insert":"r","documents":[{},{}],"lsid":{{"id":{{"$binary":{{"base64":"AQEBAQEBAQEBAQEBAQEBAQ==","subType":"04"}}}}}},"txnNumber":{{"$numberLong":"1"}}}}"#,
        at(0, 1, "1"),
        at(1, 1, "2")
    );
    for _ in 0..2 {
        let r = run(&mut m, &cmd);
        assert_eq!(field(&r, "n"), &Value::Int32(2), "{r:?}");
        assert_eq!(counters(&mut m, "r", &COUNTED[..3]), vec![1, 1, 1]);
    }
    let r = run(&mut m, r#"{"count":"r"}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(2), "{r:?}");
}
