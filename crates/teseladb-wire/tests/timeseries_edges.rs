//! Edges of time series through the commands (015 FR-006 to FR-008): unordered inserts with a
//! measurement without its date, reads bounded beyond the windows a bucket `_id` can hold,
//! `$rename` within the meta value, updates that change nothing, collections without a meta
//! field, and index specifications and partial filters a `refnode` 9.0.4 accepted or refused on
//! 01-11-2026.

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

fn date(ms: i64) -> String {
    format!(r#"{{"$date":{{"$numberLong":"{ms}"}}}}"#)
}

fn batch(r: &Document) -> Vec<Document> {
    let Value::Array(b) = field(r, "cursor.firstBatch") else { panic!("{r:?}") };
    b.iter()
        .map(|d| match d {
            Value::Document(d) => d.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn values(r: &Document) -> Vec<Value> {
    batch(r).iter().map(|d| field(d, "v").clone()).collect()
}

fn listed(m: &mut Mem, name: &str, raw: bool) -> Option<Document> {
    let r =
        m.run("m", doc(&[("listIndexes", Value::from("ts")), ("rawData", Value::Boolean(raw))]));
    batch(&r).into_iter().find(|x| x.get("name") == Some(&Value::from(name)))
}

fn series(m: &mut Mem) {
    run(m, r#"{"create":"ts","timeseries":{"timeField":"t","metaField":"m"}}"#);
}

/// An unordered insert keeps going after a measurement without its date.
#[test]
fn an_unordered_insert_skips_a_measurement_without_its_date() {
    let mut m = Mem::new();
    series(&mut m);
    let r = run(
        &mut m,
        &format!(
            r#"{{"insert":"ts","documents":[{{"t":{},"m":1,"v":1}},{{"m":1,"v":2}},{{"t":{},"m":1,"v":3}}],"ordered":false}}"#,
            date(T0),
            date(T0 + 1000)
        ),
    );
    assert_eq!(field(&r, "n"), &Value::Int32(2), "{r:?}");
    assert_eq!(field(&r, "writeErrors.0.index"), &Value::Int32(1));
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(2));
    let r = run(&mut m, r#"{"find":"ts","sort":{"v":1}}"#);
    assert_eq!(values(&r), vec![Value::Int32(1), Value::Int32(3)]);
}

/// A time bound before 1970 or past what a bucket `_id` can hold scans every bucket.
#[test]
fn time_bounds_outside_the_bucket_ids_read_every_bucket() {
    let mut m = Mem::new();
    series(&mut m);
    run(
        &mut m,
        &format!(
            r#"{{"insert":"ts","documents":[{{"t":{},"m":1,"v":1}},{{"t":{},"m":1,"v":3}}]}}"#,
            date(T0),
            date(T0 + 1000)
        ),
    );
    let r = run(
        &mut m,
        &format!(r#"{{"find":"ts","filter":{{"t":{{"$gt":{}}}}},"sort":{{"v":1}}}}"#, date(-5000)),
    );
    assert_eq!(values(&r), vec![Value::Int32(1), Value::Int32(3)]);
    let far = date(9_000_000_000_000_000);
    let r = run(
        &mut m,
        &format!(r#"{{"find":"ts","filter":{{"t":{{"$lt":{far}}}}},"sort":{{"v":1}}}}"#),
    );
    assert_eq!(values(&r), vec![Value::Int32(1), Value::Int32(3)]);
    let r = run(&mut m, &format!(r#"{{"count":"ts","query":{{"t":{{"$gte":{far}}}}}}}"#));
    assert_eq!(field(&r, "n"), &Value::Int32(0));
}

/// `$rename` may move a field within the meta value, never out of it; setting the meta value it
/// already has matches and modifies nothing.
#[test]
fn rename_stays_within_the_meta_value_and_a_same_value_set_modifies_nothing() {
    let mut m = Mem::new();
    series(&mut m);
    run(
        &mut m,
        &format!(
            r#"{{"insert":"ts","documents":[{{"t":{},"m":{{"a":1}},"v":1}},{{"t":{},"m":{{"a":1}},"v":2}}]}}"#,
            date(T0),
            date(T0 + 1000)
        ),
    );
    let r = run(
        &mut m,
        r#"{"update":"ts","updates":[{"q":{"m.a":1},"u":{"$rename":{"m.a":"x"}},"multi":true}]}"#,
    );
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(72), "{r:?}");
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &Value::from(
            "Cannot perform an update on a time-series collection which updates a field that is not the metaField 'm'"
        )
    );
    let r = run(
        &mut m,
        r#"{"update":"ts","updates":[{"q":{"m.a":1},"u":{"$rename":{"m.a":"m.b"}},"multi":true}]}"#,
    );
    assert_eq!((field(&r, "n"), field(&r, "nModified")), (&Value::Int32(2), &Value::Int32(2)));
    let r = run(&mut m, r#"{"count":"ts","query":{"m.b":1}}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(2));
    let r = run(
        &mut m,
        r#"{"update":"ts","updates":[{"q":{"m.b":1},"u":{"$set":{"m":{"b":1}}},"multi":true}]}"#,
    );
    assert_eq!((field(&r, "n"), field(&r, "nModified")), (&Value::Int32(2), &Value::Int32(0)));
}

/// Deletes on a time series without a meta field, and on measurements without a meta value.
#[test]
fn deletes_without_a_meta_value() {
    let mut m = Mem::new();
    run(&mut m, r#"{"create":"nm","timeseries":{"timeField":"t"}}"#);
    run(
        &mut m,
        &format!(
            r#"{{"insert":"nm","documents":[{{"t":{},"v":1}},{{"t":{},"v":2}}]}}"#,
            date(T0),
            date(T0 + 1)
        ),
    );
    let r = run(&mut m, r#"{"delete":"nm","deletes":[{"q":{"v":1},"limit":0}]}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    let r = run(&mut m, r#"{"find":"nm","projection":{"_id":0}}"#);
    assert_eq!(batch(&r), vec![json(&format!(r#"{{"t":{},"v":2}}"#, date(T0 + 1)))]);

    series(&mut m);
    run(
        &mut m,
        &format!(
            r#"{{"insert":"ts","documents":[{{"t":{},"v":7}},{{"t":{},"v":8}}]}}"#,
            date(T0),
            date(T0)
        ),
    );
    let r = run(&mut m, r#"{"delete":"ts","deletes":[{"q":{"v":7},"limit":0}]}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    let r = run(&mut m, r#"{"find":"ts","projection":{"_id":0}}"#);
    assert_eq!(batch(&r), vec![json(&format!(r#"{{"t":{},"v":8}}"#, date(T0)))]);
}

/// Index specifications on a time series, as a `refnode` 9.0.4 takes them.
#[test]
fn index_specifications_over_the_buckets() {
    let mut m = Mem::new();
    series(&mut m);
    // A field whose name starts with the meta field's is a measurement.
    let r = run(&mut m, r#"{"createIndexes":"ts","indexes":[{"key":{"mx":1},"name":"mx"}]}"#);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let got = listed(&mut m, "mx", true).expect("listed");
    assert_eq!(
        field(&got, "key"),
        &Value::Document(json(r#"{"control.min.mx":1,"control.max.mx":1}"#))
    );
    // Only ascending or descending on the time field.
    let r =
        run(&mut m, r#"{"createIndexes":"ts","indexes":[{"key":{"t":"2dsphere"},"name":"g"}]}"#);
    assert_eq!(field(&r, "code"), &Value::Int32(67), "{r:?}");
    assert_eq!(
        field(&r, "errmsg"),
        &Value::from(
            "BadValue: Invalid index spec for time-series collection: { t: \"2dsphere\" }. Indexes on the time field must be ascending or descending (numbers only): t: \"2dsphere\""
        )
    );
    for (key, allowed) in [
        (r#"{"t":"hashed"}"#, "the time field must be ascending or descending (numbers only)"),
        (
            r#"{"v":"hashed"}"#,
            "measurement fields must be ascending or descending (numbers only), or '2dsphere'",
        ),
    ] {
        let r = run(
            &mut m,
            &format!(r#"{{"createIndexes":"ts","indexes":[{{"key":{key},"name":"q"}}]}}"#),
        );
        let (path, kind) = key[2..].split_once("\":").expect("one field");
        assert_eq!(
            field(&r, "errmsg"),
            &Value::String(format!(
                "BadValue: Invalid index spec for time-series collection: {{ {path}: {kind} }}. Indexes on {allowed}: {path}: {kind}",
                kind = kind.trim_end_matches('}')
            )),
            "{key}"
        );
    }
    let r = run(&mut m, r#"{"createIndexes":"ts","indexes":[{"key":{"t":"text"},"name":"q"}]}"#);
    assert_eq!(
        field(&r, "errmsg"),
        &Value::from("Text indexes are not supported on time-series collections")
    );
    // Other options go along to the bucket index.
    let r = run(
        &mut m,
        r#"{"createIndexes":"ts","indexes":[{"key":{"m":1},"name":"s","sparse":true}]}"#,
    );
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let raw = listed(&mut m, "s", true).expect("listed");
    assert_eq!(raw, json(r#"{"v":2,"key":{"meta":1},"name":"s","sparse":true}"#));
    let user = listed(&mut m, "s", false).expect("listed");
    assert_eq!(user, json(r#"{"v":2,"key":{"m":1},"name":"s","sparse":true}"#));
    // TTL only on the time field, also with a partial filter on the meta value.
    let r = run(
        &mut m,
        r#"{"createIndexes":"ts","indexes":[{"key":{"m":1},"name":"ttl","expireAfterSeconds":10,"partialFilterExpression":{"m":{"$gt":0}}}]}"#,
    );
    assert_eq!(field(&r, "code"), &Value::Int32(72), "{r:?}");
    assert_eq!(
        field(&r, "errmsg"),
        &Value::from("TTL indexes on non-time fields are not supported on time-series collections")
    );
    let r = run(
        &mut m,
        r#"{"createIndexes":"ts","indexes":[{"key":{"t":1,"m":1},"name":"ttl","expireAfterSeconds":10,"partialFilterExpression":{"m":{"$gt":0}}}]}"#,
    );
    assert_eq!(field(&r, "code"), &Value::Int32(67), "{r:?}");
    assert_eq!(
        field(&r, "errmsg"),
        &Value::from(
            "TTL indexes are single-field indexes, compound indexes do not support TTL. Index spec: { t: 1, m: 1 }"
        )
    );
    let r = run(
        &mut m,
        r#"{"createIndexes":"ts","indexes":[{"key":{"t":1},"name":"ttl","expireAfterSeconds":10}]}"#,
    );
    assert_eq!(
        field(&r, "errmsg"),
        &Value::from(
            "TTL indexes on time-series collections require a partialFilterExpression on the metaField"
        )
    );
    let r = run(
        &mut m,
        r#"{"createIndexes":"ts","indexes":[{"key":{"t":-1},"name":"ttl","expireAfterSeconds":10,"partialFilterExpression":{"m":{"$gt":0}}}]}"#,
    );
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    assert_eq!(
        listed(&mut m, "ttl", true).expect("listed"),
        json(
            r#"{"v":2,"key":{"control.min.t":-1},"name":"ttl","partialFilterExpression":{"meta":{"$gt":0}},"expireAfterSeconds":10,"originalSpec":{"key":{"t":-1},"name":"ttl","expireAfterSeconds":10,"partialFilterExpression":{"m":{"$gt":0}},"v":2}}"#
        )
    );
    assert_eq!(
        listed(&mut m, "ttl", false).expect("listed"),
        json(
            r#"{"v":2,"key":{"t":-1},"name":"ttl","partialFilterExpression":{"m":{"$gt":0}},"expireAfterSeconds":10}"#
        )
    );
    // `sparse`, even false, only on the time and the meta value.
    for key in [r#"{"v":1}"#, r#"{"m":1,"v":1}"#] {
        for sparse in ["true", "false"] {
            let r = run(
                &mut m,
                &format!(
                    r#"{{"createIndexes":"ts","indexes":[{{"key":{key},"name":"q","sparse":{sparse}}}]}}"#
                ),
            );
            assert_eq!(field(&r, "code"), &Value::Int32(72), "{key} {sparse}: {r:?}");
            assert_eq!(
                field(&r, "errmsg"),
                &Value::from("Sparse indexes are not supported on time-series measurements")
            );
        }
    }
    let r = run(
        &mut m,
        r#"{"createIndexes":"ts","indexes":[{"key":{"t":1},"name":"q","sparse":true}]}"#,
    );
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
}

/// Partial filters a `refnode` 9.0.4 refused or translated on 01-11-2026.
#[test]
fn partial_filter_edges() {
    let mut m = Mem::new();
    series(&mut m);
    let create = |m: &mut Mem, pf: &str| {
        run(
            m,
            &format!(
                r#"{{"createIndexes":"ts","indexes":[{{"key":{{"m":1,"z":1}},"name":"z","partialFilterExpression":{pf}}}]}}"#
            ),
        )
    };
    let r = create(&mut m, r#"{"v":{"$in":5}}"#);
    assert_eq!(field(&r, "code"), &Value::Int32(2), "{r:?}");
    assert_eq!(field(&r, "errmsg"), &Value::from("$in needs an array"));
    let refused = [
        (r#"{"v":{"$eq":null}}"#, "can't handle comparison to null: { v: { $eq: null } }"),
        (
            r#"{"v":{"a":{"$gt":1}}}"#,
            "operand can't be an object or array: { v: { $eq: { a: { $gt: 1 } } } }",
        ),
    ];
    for (pf, msg) in refused {
        let r = create(&mut m, pf);
        assert_eq!(field(&r, "code"), &Value::Int32(5_916_301), "{pf}: {r:?}");
        assert_eq!(
            field(&r, "errmsg"),
            &Value::String(format!(
                "Error translating non-metadata time-series predicate to operate on buckets: {msg}"
            )),
            "{pf}"
        );
    }
    let r = create(&mut m, r#"{"mx":{"$gt":1}}"#);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let got = listed(&mut m, "z", true).expect("listed");
    assert_eq!(
        field(&got, "partialFilterExpression"),
        &Value::Document(json(r#"{"control.max.mx":{"$_internalExprGt":1}}"#))
    );
}
