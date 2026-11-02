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

/// Measurements before 1970 and past what a bucket `_id` can hold are stored and read back by
/// time, by meta value and time through the `{m: 1, t: 1}` index, and in time order, as a `refnode`
/// 9.0.4 gave them on 02-11-2026 (015 T020, adversarial pass).
#[test]
fn measurements_outside_the_bucket_id_years_are_read_by_time() {
    let mut m = Mem::new();
    series(&mut m);
    let far = 8_640_000_000_000_000;
    run(
        &mut m,
        &format!(
            r#"{{"insert":"ts","documents":[{{"t":{},"m":1,"v":1}},{{"t":{},"m":1,"v":2}},{{"t":{},"m":1,"v":3}},{{"t":{},"m":1,"v":4}}]}}"#,
            date(-1),
            date(-86_400_001),
            date(far),
            date(T0)
        ),
    );
    let read = |m: &mut Mem, filter: String| {
        let r = run(m, &format!(r#"{{"find":"ts","filter":{filter},"sort":{{"v":1}}}}"#));
        values(&r)
    };
    let ints = |v: &[i32]| v.iter().map(|i| Value::Int32(*i)).collect::<Vec<_>>();
    let cases = |m: &mut Mem| {
        assert_eq!(read(m, format!(r#"{{"t":{{"$lt":{}}}}}"#, date(0))), ints(&[1, 2]));
        assert_eq!(read(m, format!(r#"{{"t":{{"$gte":{}}}}}"#, date(T0))), ints(&[3, 4]));
        assert_eq!(
            read(m, format!(r#"{{"t":{{"$gt":{}}}}}"#, date(-86_400_001))),
            ints(&[1, 3, 4])
        );
        assert_eq!(read(m, format!(r#"{{"t":{{"$lte":{}}}}}"#, date(-86_400_001))), ints(&[2]));
        assert_eq!(
            read(m, format!(r#"{{"m":1,"t":{{"$gte":{},"$lt":{}}}}}"#, date(-2), date(T0))),
            ints(&[1])
        );
        assert_eq!(
            read(m, format!(r#"{{"m":1,"t":{{"$gt":{}}}}}"#, date(4_000_000_000_000_000))),
            ints(&[3])
        );
    };
    cases(&mut m);
    run(&mut m, r#"{"createIndexes":"ts","indexes":[{"key":{"m":1,"t":1},"name":"m_1_t_1"}]}"#);
    cases(&mut m);
    let r = run(&mut m, r#"{"find":"ts","sort":{"t":-1}}"#);
    assert_eq!(values(&r), ints(&[3, 4, 1, 2]));
    // The catalog's mark is not an option: `listCollections` does not show it and `collMod`
    // keeps it.
    let r = run(&mut m, r#"{"collMod":"ts","expireAfterSeconds":3600}"#);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let r = run(&mut m, r#"{"listCollections":1,"filter":{"name":"ts"}}"#);
    let Value::Document(options) = field(&batch(&r)[0], "options").clone() else { panic!("{r:?}") };
    let keys: Vec<&str> = options.iter().map(|(k, _)| k).collect();
    assert_eq!(keys, vec!["expireAfterSeconds", "timeseries"], "{r:?}");
    cases(&mut m);
    // A collection already marked takes more such measurements as they come.
    let cmd = format!(r#"{{"insert":"ts","documents":[{{"t":{},"m":9,"v":9}}]}}"#, date(-1));
    run(&mut m, &cmd);
    assert_eq!(read(&mut m, format!(r#"{{"m":9,"t":{{"$lt":{}}}}}"#, date(0))), ints(&[9]));
}

/// `listCollections` shows a time series' `expireAfterSeconds` before `timeseries`, whether it
/// came with `create` or with `collMod`, as an `int64`, and without it once `collMod` turns it
/// off (measured on a `refnode` 9.0.4 on 02-11-2026).
#[test]
fn a_time_series_lists_its_expiry_before_its_buckets() {
    let mut m = Mem::new();
    let options = |m: &mut Mem, name: &str| {
        let r = run(m, &format!(r#"{{"listCollections":1,"filter":{{"name":"{name}"}}}}"#));
        let Value::Document(o) = field(&batch(&r)[0], "options").clone() else { panic!("{r:?}") };
        o
    };
    run(&mut m, r#"{"create":"a","timeseries":{"timeField":"t"},"expireAfterSeconds":60}"#);
    let o = options(&mut m, "a");
    assert_eq!(
        o.iter().map(|(k, _)| k).collect::<Vec<_>>(),
        vec!["expireAfterSeconds", "timeseries"]
    );
    assert_eq!(o.get("expireAfterSeconds"), Some(&Value::Int64(60)));
    run(&mut m, r#"{"collMod":"a","expireAfterSeconds":"off"}"#);
    let o = options(&mut m, "a");
    assert_eq!(o.iter().map(|(k, _)| k).collect::<Vec<_>>(), vec!["timeseries"]);
    run(&mut m, r#"{"collMod":"a","expireAfterSeconds":7}"#);
    run(&mut m, r#"{"collMod":"a","timeseries":{"granularity":"minutes"}}"#);
    let o = options(&mut m, "a");
    assert_eq!(
        o.iter().map(|(k, _)| k).collect::<Vec<_>>(),
        vec!["expireAfterSeconds", "timeseries"]
    );
    assert_eq!(o.get("expireAfterSeconds"), Some(&Value::Int64(7)));
}

/// Conditions on the meta value and the time inside `$and`, also repeated at the top level, pick
/// the buckets through their index (015 T021).
#[test]
fn conditions_inside_and_reach_the_bucket_index() {
    let mut m = Mem::new();
    series(&mut m);
    run(
        &mut m,
        &format!(
            r#"{{"insert":"ts","documents":[{{"t":{},"m":1,"v":1}},{{"t":{},"m":2,"v":2}},{{"t":{},"m":1,"v":3}}]}}"#,
            date(T0),
            date(T0),
            date(T0 + 3_600_000)
        ),
    );
    let r = run(
        &mut m,
        &format!(
            r#"{{"find":"ts","filter":{{"$and":[{{"m":1}},{{"t":{{"$gte":{}}}}}]}},"sort":{{"v":1}}}}"#,
            date(T0 + 1000)
        ),
    );
    assert_eq!(values(&r), vec![Value::Int32(3)]);
    let r = run(
        &mut m,
        r#"{"find":"ts","filter":{"m":1,"$and":[{"m":1},{"v":{"$gt":0}}]},"sort":{"v":1}}"#,
    );
    assert_eq!(values(&r), vec![Value::Int32(1), Value::Int32(3)]);
}

/// A window with more buckets than one scan reads (each change of a field's type opens a bucket)
/// still takes its next measurement after reading them all (015 T021).
#[test]
fn a_window_of_many_buckets_is_read_in_chunks() {
    let mut m = Mem::new();
    series(&mut m);
    let docs: Vec<String> = (0..1001)
        .map(|i| {
            let x = if i % 2 == 0 { "1".to_owned() } else { r#""a""#.to_owned() };
            format!(r#"{{"t":{},"m":1,"x":{x}}}"#, date(T0 + i))
        })
        .collect();
    let r = run(&mut m, &format!(r#"{{"insert":"ts","documents":[{}]}}"#, docs.join(",")));
    assert_eq!(field(&r, "n"), &Value::Int32(1001), "{r:?}");
    let r = run(
        &mut m,
        &format!(r#"{{"insert":"ts","documents":[{{"t":{},"m":1,"x":2}}]}}"#, date(T0 + 2000)),
    );
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    let r = run(&mut m, r#"{"count":"ts","query":{"m":1}}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(1002), "{r:?}");
}

/// Without the index on the meta value, an insert that looks up a bucket notes no index access and
/// still lands in its window (015 T021).
#[test]
fn a_lookup_without_the_meta_index() {
    let mut m = Mem::new();
    series(&mut m);
    run(&mut m, r#"{"createIndexes":"ts","indexes":[{"key":{"v":1},"name":"v_1"}]}"#);
    let r = run(&mut m, r#"{"dropIndexes":"ts","index":"m_1_t_1"}"#);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    for (t, v) in [(T0 + 10_000, 1), (T0, 2), (T0 + 5_000, 3)] {
        let cmd = format!(r#"{{"insert":"ts","documents":[{{"t":{},"m":1,"v":{v}}}]}}"#, date(t));
        run(&mut m, &cmd);
    }
    let r = run(&mut m, r#"{"find":"ts","sort":{"t":1}}"#);
    assert_eq!(values(&r), vec![Value::Int32(2), Value::Int32(3), Value::Int32(1)]);
}

/// An index specification without its key, or with a key that is not an object, is refused as on
/// a collection that is not a time series, and reads bounded both ways in time on a series without
/// a meta field find their measurements (015 T021, measured on a `refnode` 9.0.4 on 02-11-2026).
#[test]
fn index_specifications_without_a_key_and_bounded_reads_without_meta() {
    let mut m = Mem::new();
    series(&mut m);
    for (spec, code, msg) in [
        (
            r#"{"name":"x"}"#,
            67,
            r#"Error in specification { name: "x", key: {} } :: caused by :: Index keys cannot be empty."#,
        ),
        (r#"{"key":5,"name":"x"}"#, 10065, "invalid parameter: expected an object (key)"),
    ] {
        let r = run(&mut m, &format!(r#"{{"createIndexes":"ts","indexes":[{spec}]}}"#));
        assert_eq!(field(&r, "code"), &Value::Int32(code), "{r:?}");
        assert_eq!(field(&r, "errmsg"), &Value::from(msg), "{r:?}");
    }
    run(&mut m, r#"{"create":"nm","timeseries":{"timeField":"t"}}"#);
    run(
        &mut m,
        &format!(
            r#"{{"insert":"nm","documents":[{{"t":{},"v":1}},{{"t":{},"v":2}},{{"t":{},"v":3}}]}}"#,
            date(T0),
            date(T0 + 7_200_000),
            date(T0 + 14_400_000)
        ),
    );
    let r = run(
        &mut m,
        &format!(
            r#"{{"find":"nm","filter":{{"t":{{"$gte":{},"$lt":{}}}}},"sort":{{"v":1}}}}"#,
            date(T0 + 3_600_000),
            date(T0 + 10_000_000)
        ),
    );
    assert_eq!(values(&r), vec![Value::Int32(2)]);
}

/// An index made on the buckets with `rawData` lists its key on the measurements' fields, the
/// least and greatest of a field once (measured on a `refnode` 9.0.4 on 02-11-2026).
#[test]
fn a_raw_bucket_index_lists_its_measurement_key() {
    let mut m = Mem::new();
    series(&mut m);
    let r = run(
        &mut m,
        r#"{"createIndexes":"ts","rawData":true,"indexes":[{"key":{"control.min.v":1,"control.max.v":1},"name":"raw_v"}]}"#,
    );
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let listed = listed(&mut m, "raw_v", false).expect("raw_v");
    assert_eq!(listed, json(r#"{"v":2,"key":{"v":1},"name":"raw_v"}"#));
}

/// An ordered insert with a refused geometry, back into a window an earlier insert closed by
/// moving forward in time, finds that window's bucket in its attempt and writes the measurements
/// before the refused one (015 T021).
#[test]
fn an_attempt_back_in_time_finds_the_closed_bucket() {
    let mut m = Mem::new();
    series(&mut m);
    run(&mut m, r#"{"createIndexes":"ts","indexes":[{"key":{"loc":"2dsphere"},"name":"geo"}]}"#);
    let point = r#"{"type":"Point","coordinates":[1,2]}"#;
    for t in [T0, T0 + 7_200_000] {
        let cmd =
            format!(r#"{{"insert":"ts","documents":[{{"t":{},"m":1,"loc":{point}}}]}}"#, date(t));
        let r = run(&mut m, &cmd);
        assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    }
    let bad = r#"{"type":"Point","coordinates":[500,1]}"#;
    let r = run(
        &mut m,
        &format!(
            r#"{{"insert":"ts","documents":[{{"t":{},"m":1,"loc":{point}}},{{"t":{},"m":1,"loc":{bad}}}]}}"#,
            date(T0 + 1000),
            date(T0 + 1001)
        ),
    );
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    assert_eq!(field(&r, "writeErrors.0.index"), &Value::Int32(1), "{r:?}");
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(511), "{r:?}");
    let r = run(&mut m, r#"{"count":"ts"}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(3), "{r:?}");
}

/// A raw bucket index on a field that is not a measurement's nor the meta value's is listed only
/// with `rawData` (measured on a `refnode` 9.0.4 on 02-11-2026).
#[test]
fn a_raw_index_off_the_measurements_lists_only_raw() {
    let mut m = Mem::new();
    series(&mut m);
    run(
        &mut m,
        r#"{"createIndexes":"ts","rawData":true,"indexes":[{"key":{"meta.a":1,"control.version":1},"name":"raw_mv"}]}"#,
    );
    assert_eq!(listed(&mut m, "raw_mv", false), None);
    assert!(listed(&mut m, "raw_mv", true).is_some());
}
