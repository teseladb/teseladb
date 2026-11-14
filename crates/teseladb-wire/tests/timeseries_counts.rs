//! Time series writes measured on a reference replica set member 9.0.4 (replica set) on 01-11-2026:
//! the bucket counters of an insert that fills a bucket by count or by size in one statement and of
//! a bucket made again after its measurements were deleted; a delete and a find with a collation;
//! an update whose operator is no document, or that gives measurements without one a meta value;
//! and an index hidden after it was made, as `listIndexes` shows it.

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

/// Partial filters of a time series index as a reference replica set member 9.0.4 translates them
/// to the buckets (`listIndexes` with `rawData`) or refuses them, measured on 01-11-2026.
#[test]
fn partial_filters_translate_to_the_buckets_as_the_reference_does() {
    let d = |ms: i64| format!(r#"{{"$date":{{"$numberLong":"{ms}"}}}}"#);
    let translated = [
        (
            r#"{"x":{"$exists":true}}"#.to_owned(),
            r#"{"$and":[{"control.min.x":{"$exists":true}},{"control.max.x":{"$exists":true}}]}"#
                .to_owned(),
        ),
        (r#"{"m":{"a":1}}"#.to_owned(), r#"{"meta":{"$eq":{"a":1}}}"#.to_owned()),
        (
            format!(r#"{{"x":{{"$gte":{}}}}}"#, d(1000)),
            format!(r#"{{"control.max.x":{{"$_internalExprGte":{}}}}}"#, d(1000)),
        ),
        (
            r#"{"$and":[{"x":{"$gt":1}},{"y":{"$lt":2}}]}"#.to_owned(),
            r#"{"$and":[{"control.max.x":{"$_internalExprGt":1}},{"control.min.y":{"$_internalExprLt":2}}]}"#.to_owned(),
        ),
        (
            r#"{"$or":[{"x":{"$gt":1}},{"m":2}]}"#.to_owned(),
            r#"{"$or":[{"control.max.x":{"$_internalExprGt":1}},{"meta":{"$eq":2}}]}"#.to_owned(),
        ),
        (r#"{"m":{"$in":[1,2]}}"#.to_owned(), r#"{"meta":{"$in":[1,2]}}"#.to_owned()),
        (
            r#"{"x":{"$in":[1,2]}}"#.to_owned(),
            r#"{"$or":[{"$and":[{"control.min.x":{"$_internalExprLte":1}},{"control.max.x":{"$_internalExprGte":1}}]},{"$and":[{"control.min.x":{"$_internalExprLte":2}},{"control.max.x":{"$_internalExprGte":2}}]}]}"#.to_owned(),
        ),
        (
            format!(r#"{{"t":{{"$gt":{}}}}}"#, d(1000)),
            format!(
                r#"{{"$and":[{{"control.max.t":{{"$_internalExprGt":{}}}}},{{"control.min.t":{{"$_internalExprGt":{}}}}}]}}"#,
                d(1000),
                d(1000 - 3_600_000)
            ),
        ),
    ];
    let refused = [
        (r#"{"x":{"$gte":null}}"#, "can't handle comparison to null: { x: { $gte: null } }"),
        (r#"{"x":{"$gt":null}}"#, "can't handle comparison to null: { x: { $gt: null } }"),
        (r#"{"x":{"$lt":null}}"#, "can't handle comparison to null: { x: { $lt: null } }"),
        (r#"{"x":{"$lte":null}}"#, "can't handle comparison to null: { x: { $lte: null } }"),
        (
            r#"{"x":{"$lt":5,"$gte":null}}"#,
            "can't handle comparison to null: { x: { $gte: null } }",
        ),
        (
            r#"{"x":{"$exists":false}}"#,
            "can't handle this predicate: { x: { $not: { $exists: true } } }",
        ),
        (
            r#"{"x":{"$in":[null]}}"#,
            "can't handle {$eq: null} predicate (inside $in predicate): { x: { $in: [ null ] } }",
        ),
    ];
    let create = |pf: &str| {
        let mut m = Mem::new();
        series(&mut m, "ts");
        let r = run(
            &mut m,
            &format!(
                r#"{{"createIndexes":"ts","indexes":[{{"key":{{"m":1,"z":1}},"name":"z","partialFilterExpression":{pf}}}]}}"#
            ),
        );
        (m, r)
    };
    for (pf, want) in &translated {
        let (mut m, r) = create(pf);
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{pf}: {r:?}");
        let r = run(&mut m, r#"{"listIndexes":"ts","rawData":true}"#);
        let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
        let z = b
            .iter()
            .find_map(|x| match x {
                Value::Document(x) if x.get("name") == Some(&Value::from("z")) => Some(x.clone()),
                _ => None,
            })
            .expect("index z");
        assert_eq!(field(&z, "partialFilterExpression"), &Value::Document(json(want)), "{pf}");
    }
    for (pf, msg) in refused {
        let (_, r) = create(pf);
        assert_eq!(r.get("code"), Some(&Value::Int32(5_916_301)), "{pf}: {r:?}");
        assert_eq!(
            field(&r, "errmsg"),
            &Value::String(format!(
                "Error translating non-metadata time-series predicate to operate on buckets: {msg}"
            )),
            "{pf}"
        );
    }
}
