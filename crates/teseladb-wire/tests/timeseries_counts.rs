//! Time series writes measured on a `refnode` 9.0.4 (replica set) on 01-11-2026: the bucket
//! counters of an insert that fills a bucket by count or by size in one statement and of a bucket
//! made again after its measurements were deleted; a delete and a find with a collation; an update whose
//! operator is no document, or that gives measurements without one a meta value; and an index
//! hidden after it was made, as `listIndexes` shows it.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

const T0: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, cmd: &str) -> Document {
    m.run("m", json(cmd))
}

fn at(ms: i64) -> String {
    format!(r#"{{"$date":{{"$numberLong":"{}"}}}}"#, T0 + ms)
}

fn series(m: &mut Mem, name: &str) {
    let r = run(
        m,
        &format!(r#"{{"create":"{name}","timeseries":{{"timeField":"t","metaField":"m"}}}}"#),
    );
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
}

fn insert(m: &mut Mem, name: &str, docs: &[String]) -> Document {
    run(m, &format!(r#"{{"insert":"{name}","documents":[{}]}}"#, docs.join(",")))
}

fn check(m: &mut Mem, name: &str, want: &[(&str, i32)]) {
    let r = m.run("m", doc(&[("collStats", Value::from(name))]));
    let Value::Document(ts) = field(&r, "timeseries") else { panic!("{r:?}") };
    for (k, v) in want {
        assert_eq!(ts.get(k), Some(&Value::Int32(*v)), "{k} in {ts:?}");
    }
}

#[test]
fn one_insert_that_fills_a_bucket_by_count_or_size_closes_it() {
    let mut m = Mem::new();
    series(&mut m, "c");
    let docs: Vec<String> =
        (0..1001).map(|i| format!(r#"{{"t":{},"m":9,"v":{i}}}"#, at(i))).collect();
    let r = insert(&mut m, "c", &docs);
    assert_eq!(field(&r, "n"), &Value::Int32(1001), "{r:?}");
    check(
        &mut m,
        "c",
        &[
            ("bucketCount", 2),
            ("numBucketInserts", 2),
            ("numBucketsOpenedDueToMetadata", 1),
            ("numBucketsClosedDueToCount", 1),
            ("numCommits", 2),
            ("numMeasurementsCommitted", 1001),
            ("avgNumMeasurementsPerCommit", 500),
        ],
    );
    series(&mut m, "s");
    let big: Vec<String> = (0..12)
        .map(|i| format!(r#"{{"t":{},"m":1,"s":"{}{i}"}}"#, at(i), "x".repeat(20_000)))
        .collect();
    let r = insert(&mut m, "s", &big);
    assert_eq!(field(&r, "n"), &Value::Int32(12), "{r:?}");
    check(
        &mut m,
        "s",
        &[
            ("bucketCount", 2),
            ("numBucketInserts", 2),
            ("numBucketsOpenedDueToMetadata", 1),
            ("numBucketsClosedDueToSize", 1),
            ("numBucketsKeptOpenDueToLargeMeasurements", 1),
            ("numCommits", 2),
            ("numMeasurementsCommitted", 12),
        ],
    );
}

#[test]
fn a_bucket_made_again_in_its_window_counts_as_an_insert_only() {
    let mut m = Mem::new();
    series(&mut m, "r");
    insert(&mut m, "r", &[format!(r#"{{"t":{},"m":1}}"#, at(0))]);
    let r = run(&mut m, r#"{"delete":"r","deletes":[{"q":{"m":1},"limit":0}]}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    insert(&mut m, "r", &[format!(r#"{{"t":{},"m":1}}"#, at(5))]);
    check(
        &mut m,
        "r",
        &[
            ("bucketCount", 1),
            ("numBucketInserts", 2),
            ("numBucketsOpenedDueToMetadata", 1),
            ("numBucketsClosedDueToTimeForward", 0),
            ("numBucketsArchivedDueToTimeBackward", 0),
            ("numCommits", 2),
        ],
    );
}

#[test]
fn deletes_match_the_meta_value_with_their_collation() {
    let mut m = Mem::new();
    series(&mut m, "c");
    insert(
        &mut m,
        "c",
        &[format!(r#"{{"t":{},"m":"a"}}"#, at(1000)), format!(r#"{{"t":{},"m":"A"}}"#, at(2000))],
    );
    let r = run(
        &mut m,
        r#"{"delete":"c","deletes":[{"q":{"m":"a"},"limit":0,"collation":{"locale":"en","strength":2}}]}"#,
    );
    assert_eq!(field(&r, "n"), &Value::Int32(2), "{r:?}");
    insert(
        &mut m,
        "c",
        &[format!(r#"{{"t":{},"m":"a"}}"#, at(1000)), format!(r#"{{"t":{},"m":"A"}}"#, at(2000))],
    );
    let r =
        run(&mut m, r#"{"find":"c","filter":{"m":"a"},"collation":{"locale":"en","strength":2}}"#);
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    assert_eq!(b.len(), 2, "{r:?}");
}

#[test]
fn an_update_operator_that_is_no_document_fails_to_parse() {
    let mut m = Mem::new();
    series(&mut m, "c");
    insert(&mut m, "c", &[format!(r#"{{"t":{},"m":"a"}}"#, at(1000))]);
    let r = run(&mut m, r#"{"update":"c","updates":[{"q":{},"u":{"$set":5},"multi":true}]}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(0), "{r:?}");
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(9), "{r:?}");
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &Value::from(
            "Modifiers operate on fields but we found type int instead. For example: {$mod: {<field>: ...}} not {$set: 5}"
        ),
        "{r:?}"
    );
}

#[test]
fn an_index_hidden_after_it_was_made_lists_hidden() {
    let mut m = Mem::new();
    series(&mut m, "c");
    let r = run(&mut m, r#"{"createIndexes":"c","indexes":[{"key":{"x":1},"name":"x_1"}]}"#);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let r = run(&mut m, r#"{"collMod":"c","index":{"name":"x_1","hidden":true}}"#);
    assert_eq!(field(&r, "hidden_new"), &Value::Boolean(true), "{r:?}");
    let r = run(&mut m, r#"{"listIndexes":"c"}"#);
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    let want = json(r#"{"v":2,"key":{"x":1},"name":"x_1","hidden":true}"#);
    assert_eq!(b.get(1), Some(&Value::Document(want)), "{r:?}");
}

#[test]
fn an_update_gives_a_meta_value_to_measurements_without_one() {
    let mut m = Mem::new();
    series(&mut m, "n");
    insert(
        &mut m,
        "n",
        &[format!(r#"{{"t":{},"x":1}}"#, at(0)), format!(r#"{{"t":{},"x":2}}"#, at(1))],
    );
    let r =
        run(&mut m, r#"{"update":"n","updates":[{"q":{},"u":{"$set":{"m":"z"}},"multi":true}]}"#);
    assert_eq!(
        (field(&r, "n"), field(&r, "nModified")),
        (&Value::Int32(2), &Value::Int32(2)),
        "{r:?}"
    );
    let r = run(&mut m, r#"{"find":"n","projection":{"_id":0,"t":0},"sort":{"x":1}}"#);
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    let want: Vec<Value> = [r#"{"m":"z","x":1}"#, r#"{"m":"z","x":2}"#]
        .iter()
        .map(|t| Value::Document(json(t)))
        .collect();
    assert_eq!(b, &want, "{r:?}");
}
