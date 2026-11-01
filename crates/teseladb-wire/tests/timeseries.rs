//! Time series through the commands (015 T012, T013, FR-006, FR-007): measurements go into
//! buckets and `find`, `count` and `distinct` read them unpacked, `rawData` reads the buckets,
//! against what a `refrouter` 9.0.4 answered on 31-10-2026.

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

fn series(m: &mut Mem) {
    run(m, r#"{"create":"ts","timeseries":{"timeField":"t","metaField":"m"}}"#);
    let docs = [
        format!(r#"{{"_id":1,"t":{},"m":{{"s":1}},"v":1,"x":"a"}}"#, date(T0 + 1000)),
        format!(r#"{{"_id":2,"t":{},"m":{{"s":1}},"v":2.5}}"#, date(T0 + 2000)),
        format!(r#"{{"_id":3,"t":{},"m":{{"s":2}},"v":3,"x":"b"}}"#, date(T0 + 3000)),
        format!(r#"{{"_id":4,"t":{},"m":{{"s":1}},"v":4}}"#, date(T0 + 7_203_000)),
    ];
    let r = run(m, &format!(r#"{{"insert":"ts","documents":[{}]}}"#, docs.join(",")));
    assert_eq!(field(&r, "n"), &Value::Int32(4), "{r:?}");
}

#[test]
fn measurements_come_back_unpacked_with_time_and_meta_first() {
    let mut m = Mem::new();
    series(&mut m);
    let r = run(&mut m, r#"{"find":"ts","sort":{"_id":1}}"#);
    let want = [
        format!(r#"{{"t":{},"m":{{"s":1}},"_id":1,"v":1,"x":"a"}}"#, date(T0 + 1000)),
        format!(r#"{{"t":{},"m":{{"s":1}},"_id":2,"v":2.5}}"#, date(T0 + 2000)),
        format!(r#"{{"t":{},"m":{{"s":2}},"_id":3,"v":3,"x":"b"}}"#, date(T0 + 3000)),
        format!(r#"{{"t":{},"m":{{"s":1}},"_id":4,"v":4}}"#, date(T0 + 7_203_000)),
    ];
    let want: Vec<Document> = want.iter().map(|w| json(w)).collect();
    assert_eq!(batch(&r), want);
    let r = m.run("m", doc(&[("count", Value::from("ts"))]));
    assert_eq!(field(&r, "n"), &Value::Int32(4));
    let r = run(&mut m, r#"{"count":"ts","query":{"v":{"$gt":2}}}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(3));
    let r = run(&mut m, r#"{"distinct":"ts","key":"x"}"#);
    assert_eq!(field(&r, "values"), &Value::Array(vec![Value::from("a"), Value::from("b")]));
    let r = run(
        &mut m,
        r#"{"find":"ts","filter":{"v":{"$gte":3}},"projection":{"_id":1},"sort":{"_id":-1}}"#,
    );
    assert_eq!(batch(&r), vec![json(r#"{"_id":4}"#), json(r#"{"_id":3}"#)]);
}

#[test]
fn raw_data_reads_the_buckets() {
    let mut m = Mem::new();
    series(&mut m);
    let r = run(&mut m, r#"{"find":"ts","rawData":true}"#);
    let buckets = batch(&r);
    // Two meta values in the first hour, one more bucket two hours later.
    assert_eq!(buckets.len(), 3, "{buckets:?}");
    let counts: Vec<&Value> = buckets.iter().map(|b| field(b, "control.count")).collect();
    assert!(counts.contains(&&Value::Int32(2)), "{counts:?}");
    let r = run(&mut m, r#"{"count":"ts","rawData":true}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(3));
    // A filter on the meta value and an hour reads one bucket's measurements.
    let r = run(
        &mut m,
        &format!(
            r#"{{"find":"ts","filter":{{"m":{{"s":1}},"t":{{"$gte":{},"$lt":{}}}}},"projection":{{"_id":1}}}}"#,
            date(T0),
            date(T0 + 3_600_000)
        ),
    );
    assert_eq!(batch(&r), vec![json(r#"{"_id":1}"#), json(r#"{"_id":2}"#)]);
}

#[test]
fn a_measurement_needs_its_date() {
    let mut m = Mem::new();
    series(&mut m);
    for doc in [r#"{"m":"a"}"#, r#"{"t":5}"#] {
        let r = run(&mut m, &format!(r#"{{"insert":"ts","documents":[{doc}]}}"#));
        assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(2), "{r:?}");
        assert_eq!(
            field(&r, "writeErrors.0.errmsg"),
            &Value::String(
                "'t' must be present and contain a valid BSON UTC datetime value".into()
            )
        );
    }
}

#[test]
fn a_full_bucket_opens_the_next_one() {
    let mut m = Mem::new();
    run(&mut m, r#"{"create":"big","timeseries":{"timeField":"t"}}"#);
    let docs: Vec<String> =
        (0..1001).map(|i| format!(r#"{{"t":{},"v":{i}}}"#, date(T0 + i))).collect();
    let r = run(&mut m, &format!(r#"{{"insert":"big","documents":[{}]}}"#, docs.join(",")));
    assert_eq!(field(&r, "n"), &Value::Int32(1001), "{r:?}");
    let r = run(&mut m, r#"{"find":"big","rawData":true,"projection":{"control.count":1}}"#);
    let counts: Vec<Value> = batch(&r).iter().map(|b| field(b, "control.count").clone()).collect();
    assert_eq!(counts, vec![Value::Int32(1000), Value::Int32(1)]);
    let r = run(&mut m, r#"{"count":"big"}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(1001));
}

fn meta_series(m: &mut Mem) {
    run(m, r#"{"create":"ts","timeseries":{"timeField":"t","metaField":"m"}}"#);
    let docs: Vec<String> = (0..6)
        .map(|i| {
            let s = if i % 2 == 1 { "a" } else { "b" };
            format!(
                r#"{{"_id":{i},"t":{},"m":{{"s":"{s}"}},"v":{i}}}"#,
                date(T0 + i64::from(i) * 1000)
            )
        })
        .collect();
    run(m, &format!(r#"{{"insert":"ts","documents":[{}]}}"#, docs.join(",")));
}

#[test]
fn updates_change_only_the_meta_value_as_a_refrouter_allows() {
    let mut m = Mem::new();
    meta_series(&mut m);
    let refused = [
        (
            r#"{"q":{"_id":99},"u":{"$set":{"m":1}},"multi":true,"upsert":true}"#,
            "Cannot perform an upsert on a time-series collection",
        ),
        (
            r#"{"q":{"m.s":"a"},"u":{"$set":{"m.x":1}}}"#,
            "Cannot perform a non-multi update on a time-series collection",
        ),
        (
            r#"{"q":{"v":1},"u":{"$set":{"m.x":1}},"multi":true}"#,
            "Cannot perform an update or delete on a time-series collection when querying on a field that is not the metaField 'm'",
        ),
        (
            r#"{"q":{},"u":{"m":1},"multi":true}"#,
            "Cannot perform an update on a time-series collection using a replacement document",
        ),
        (
            r#"{"q":{},"u":[{"$set":{"k":1}}],"multi":true}"#,
            "Cannot perform an update on a time-series collection using a pipeline update",
        ),
        (
            r#"{"q":{"m.s":"a"},"u":{"$set":{"v":1}},"multi":true}"#,
            "Cannot perform an update on a time-series collection which updates a field that is not the metaField 'm'",
        ),
    ];
    for (stmt, msg) in refused {
        let r = run(&mut m, &format!(r#"{{"update":"ts","updates":[{stmt}]}}"#));
        assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(72), "{stmt}: {r:?}");
        assert_eq!(field(&r, "writeErrors.0.errmsg"), &Value::String(msg.into()), "{stmt}");
    }
    let r = run(
        &mut m,
        r#"{"update":"ts","updates":[{"q":{"m.s":"a"},"u":{"$set":{"m.s":"c"}},"multi":true}]}"#,
    );
    assert_eq!(
        (field(&r, "n"), field(&r, "nModified")),
        (&Value::Int32(3), &Value::Int32(3)),
        "{r:?}"
    );
    let r = run(&mut m, r#"{"count":"ts","query":{"m.s":"c"}}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(3));
    let r = run(
        &mut m,
        r#"{"update":"ts","updates":[{"q":{"$or":[{"m.s":"b"},{"m":{"s":"c"}}]},"u":{"$unset":{"m":1}},"multi":true}]}"#,
    );
    assert_eq!(field(&r, "n"), &Value::Int32(6), "{r:?}");
    let r = run(&mut m, r#"{"find":"ts","filter":{"m":{"$exists":true}}}"#);
    assert_eq!(batch(&r), Vec::<Document>::new());
    let r = run(&mut m, r#"{"count":"ts"}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(6));
}

#[test]
fn deletes_take_measurements_out_of_their_buckets() {
    let mut m = Mem::new();
    meta_series(&mut m);
    let r = run(&mut m, r#"{"delete":"ts","deletes":[{"q":{"v":{"$gte":0}},"limit":1}]}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    let r = run(&mut m, r#"{"delete":"ts","deletes":[{"q":{"m.s":"a"},"limit":0}]}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(3), "{r:?}");
    let r = run(&mut m, r#"{"findAndModify":"ts","query":{"_id":4},"remove":true}"#);
    assert_eq!(field(&r, "value._id"), &Value::Int32(4), "{r:?}");
    let r = run(&mut m, r#"{"find":"ts","projection":{"_id":1}}"#);
    let ids: Vec<Document> = batch(&r);
    assert_eq!(ids.len(), 1, "{ids:?}");
    // Empty buckets go: one bucket is left.
    let r = run(&mut m, r#"{"count":"ts","rawData":true}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(1));
    let r = run(&mut m, r#"{"delete":"ts","deletes":[{"q":{},"limit":0}]}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(1));
    let r = run(&mut m, r#"{"count":"ts","rawData":true}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(0));
}

fn index_list(m: &mut Mem, raw: bool) -> Vec<Document> {
    let r = m.run("m", Document::new().with("listIndexes", "ts").with("rawData", raw));
    batch(&r)
}

#[test]
fn indexes_live_on_the_buckets_and_list_as_asked() {
    let mut m = Mem::new();
    run(&mut m, r#"{"create":"ts","timeseries":{"timeField":"t","metaField":"m"}}"#);
    // `create` makes the index of the meta value and the time; no `_id_`.
    assert_eq!(
        index_list(&mut m, false),
        vec![json(r#"{"v":2,"key":{"m":1,"t":1},"name":"m_1_t_1"}"#)]
    );
    for (key, name) in [
        (r#"{"t":-1}"#, "t_-1"),
        (r#"{"m.s":1,"t":1}"#, "ms_t"),
        (r#"{"v":1}"#, "v_1"),
        (r#"{"v":-1,"m":1}"#, "v_m"),
    ] {
        let r = run(
            &mut m,
            &format!(r#"{{"createIndexes":"ts","indexes":[{{"key":{key},"name":"{name}"}}]}}"#),
        );
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{name}: {r:?}");
    }
    let r = run(
        &mut m,
        r#"{"createIndexes":"ts","indexes":[{"key":{"m":1},"name":"msp","sparse":true}]}"#,
    );
    assert_eq!(field(&r, "numIndexesBefore"), &Value::Int32(5), "{r:?}");
    assert_eq!(
        index_list(&mut m, false),
        [
            r#"{"v":2,"key":{"m":1,"t":1},"name":"m_1_t_1"}"#,
            r#"{"v":2,"key":{"t":-1},"name":"t_-1"}"#,
            r#"{"v":2,"key":{"m.s":1,"t":1},"name":"ms_t"}"#,
            r#"{"v":2,"key":{"v":1},"name":"v_1"}"#,
            r#"{"v":2,"key":{"v":-1,"m":1},"name":"v_m"}"#,
            r#"{"v":2,"key":{"m":1},"name":"msp","sparse":true}"#,
        ]
        .map(json)
    );
    assert_eq!(
        index_list(&mut m, true),
        [
            r#"{"v":2,"key":{"meta":1,"control.min.t":1,"control.max.t":1},"name":"m_1_t_1"}"#,
            r#"{"v":2,"key":{"control.max.t":-1,"control.min.t":-1},"name":"t_-1"}"#,
            r#"{"v":2,"key":{"meta.s":1,"control.min.t":1,"control.max.t":1},"name":"ms_t"}"#,
            r#"{"v":2,"key":{"control.min.v":1,"control.max.v":1},"name":"v_1","originalSpec":{"key":{"v":1},"name":"v_1","v":2}}"#,
            r#"{"v":2,"key":{"control.max.v":-1,"control.min.v":-1,"meta":1},"name":"v_m","originalSpec":{"key":{"v":-1,"m":1},"name":"v_m","v":2}}"#,
            r#"{"v":2,"key":{"meta":1},"name":"msp","sparse":true}"#,
        ]
        .map(json)
    );
    // Writes keep the bucket indexes, and a key of the measurements drops one.
    run(
        &mut m,
        &format!(r#"{{"insert":"ts","documents":[{{"t":{},"m":{{"s":1}},"v":1}}]}}"#, date(T0)),
    );
    let r = run(&mut m, r#"{"dropIndexes":"ts","index":{"t":-1}}"#);
    assert_eq!(field(&r, "nIndexesWas"), &Value::Int32(6), "{r:?}");
    let r = run(&mut m, r#"{"dropIndexes":"ts","index":"v_1"}"#);
    assert_eq!(field(&r, "nIndexesWas"), &Value::Int32(5), "{r:?}");
}

#[test]
fn indexes_a_time_series_refuses() {
    let mut m = Mem::new();
    run(&mut m, r#"{"create":"ts","timeseries":{"timeField":"t","metaField":"m"}}"#);
    let cases = [
        (
            r#"{"key":{"loc":"2d"},"name":"g"}"#,
            67,
            r#"BadValue: Invalid index spec for time-series collection: { loc: "2d" }. Indexes on measurement fields must be ascending or descending (numbers only), or '2dsphere': loc: "2d""#,
        ),
        (
            r#"{"key":{"x":"text"},"name":"x"}"#,
            72,
            "Text indexes are not supported on time-series collections",
        ),
        (
            r#"{"key":{"m":1},"name":"u","unique":true}"#,
            72,
            "Unique indexes are not supported on time-series collections",
        ),
        (
            r#"{"key":{"t":1},"name":"tt","expireAfterSeconds":5}"#,
            72,
            "TTL indexes on time-series collections require a partialFilterExpression on the metaField",
        ),
    ];
    for (index, code, msg) in cases {
        let r = run(&mut m, &format!(r#"{{"createIndexes":"ts","indexes":[{index}]}}"#));
        assert_eq!(
            (field(&r, "code"), field(&r, "errmsg")),
            (&Value::Int32(code), &Value::String(msg.into())),
            "{index}"
        );
    }
    // A partial filter keeps the index as asked.
    let r = run(
        &mut m,
        r#"{"createIndexes":"ts","indexes":[{"key":{"m":1},"name":"mp","partialFilterExpression":{"v":{"$gt":1}}}]}"#,
    );
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let listed = index_list(&mut m, false);
    assert_eq!(
        listed.last(),
        Some(&json(
            r#"{"v":2,"key":{"m":1},"name":"mp","partialFilterExpression":{"v":{"$gt":1}}}"#
        ))
    );
    // Without a meta field there is no default index.
    run(&mut m, r#"{"create":"nm","timeseries":{"timeField":"t"}}"#);
    let r = m.run("m", Document::new().with("listIndexes", "nm"));
    assert_eq!(batch(&r), Vec::<Document>::new());
}

#[test]
fn the_ttl_monitor_drops_the_buckets_whose_newest_measurement_expired() {
    let mut m = Mem::new();
    run(
        &mut m,
        r#"{"create":"ts","timeseries":{"timeField":"t","metaField":"m"},"expireAfterSeconds":10}"#,
    );
    let now = i64::try_from(m.now).expect("a time");
    // Two hours apart: separate buckets; the old one expired, the new one did not.
    let docs = [
        format!(r#"{{"t":{},"m":1}}"#, date(now - 7_200_000)),
        format!(r#"{{"t":{},"m":1}}"#, date(now - 7_190_000)),
        format!(r#"{{"t":{},"m":1}}"#, date(now)),
    ];
    run(&mut m, &format!(r#"{{"insert":"ts","documents":[{}]}}"#, docs.join(",")));
    let shared = std::mem::take(&mut m.shared);
    let deleted = teseladb_wire::commands::ttl::pass(&mut m, &shared, now).expect("a pass");
    m.shared = shared;
    assert_eq!(deleted, 1, "one bucket");
    let r = run(&mut m, r#"{"count":"ts"}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(1));
}

#[test]
fn a_bucket_geometry_index_refuses_what_is_no_geometry() {
    let mut m = Mem::new();
    run(&mut m, r#"{"create":"ts","timeseries":{"timeField":"t","metaField":"m"}}"#);
    let r = run(
        &mut m,
        r#"{"createIndexes":"ts","indexes":[{"key":{"loc":"2dsphere"},"name":"loc_2dsphere"}]}"#,
    );
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let ok = format!(
        r#"{{"insert":"ts","documents":[{{"_id":1,"t":{},"m":1,"loc":{{"type":"Point","coordinates":[1,2]}}}}]}}"#,
        date(T0)
    );
    assert_eq!(field(&run(&mut m, &ok), "n"), &Value::Int32(1));
    let bad =
        format!(r#"{{"insert":"ts","documents":[{{"_id":2,"t":{},"m":2,"loc":"x"}}]}}"#, date(T0));
    let r = run(&mut m, &bad);
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(511), "{r:?}");
    let Value::String(msg) = field(&r, "writeErrors.0.errmsg") else { panic!("{r:?}") };
    assert!(
        msg.starts_with(
            "Could not extract geo keys at path 'data.loc' for document with _id ObjectId('"
        ),
        "{msg}"
    );
    assert!(msg.ends_with("'): geo element must be an array or object, got string"), "{msg}");
    assert_eq!(
        field(&r, "writeErrors.0.errInfo.failingElement"),
        &Value::Document(json(r#"{"element":"x"}"#))
    );
    let null =
        format!(r#"{{"insert":"ts","documents":[{{"_id":3,"t":{},"m":2,"loc":null}}]}}"#, date(T0));
    assert_eq!(field(&run(&mut m, &null), "n"), &Value::Int32(1));
}

#[test]
fn bucket_namespaces_are_not_served() {
    let mut m = Mem::new();
    let r = run(&mut m, r#"{"find":"system.buckets.ts"}"#);
    assert_eq!(field(&r, "code"), &Value::Int32(491), "{r:?}");
    assert_eq!(
        field(&r, "errmsg"),
        &Value::String("Command find on namespace m.system.buckets.ts is not supported. Direct access to timeseries buckets namespaces is not allowed anymore. Please target the main timeseries namespace instead. Use the rawData API to directly read timeseries buckets data in raw format.".into())
    );
    let r = run(&mut m, r#"{"create":"system.buckets.x"}"#);
    let Value::String(msg) = field(&r, "errmsg") else { panic!("{r:?}") };
    assert!(
        msg.starts_with("Command _shardsvrCreateCollection on namespace m.system.buckets.x"),
        "{msg}"
    );
    // Renamed, a time series keeps its measurements.
    run(&mut m, r#"{"create":"ts","timeseries":{"timeField":"t"}}"#);
    run(&mut m, &format!(r#"{{"insert":"ts","documents":[{{"t":{}}}]}}"#, date(T0)));
    let r = m.run("admin", json(r#"{"renameCollection":"m.ts","to":"m.ts9"}"#));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let r = run(&mut m, r#"{"count":"ts9"}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(1));
}

/// The bucket filter of each partial filter, as a `refrouter` 9.0.4 lists it with `rawData`.
#[test]
fn partial_filters_list_over_the_buckets_as_refdb_writes_them() {
    let mut m = Mem::new();
    run(&mut m, r#"{"create":"ts","timeseries":{"timeField":"t","metaField":"m"}}"#);
    let t = r#"{"$date":"2026-01-01T00:00:00Z"}"#;
    let cases = [
        (r#"{"v":{"$gt":1}}"#.to_owned(), r#"{"control.max.v":{"$_internalExprGt":1}}"#.to_owned()),
        (r#"{"v":{"$gte":1}}"#.into(), r#"{"control.max.v":{"$_internalExprGte":1}}"#.into()),
        (r#"{"v":{"$lt":1}}"#.into(), r#"{"control.min.v":{"$_internalExprLt":1}}"#.into()),
        (r#"{"v":{"$lte":1}}"#.into(), r#"{"control.min.v":{"$_internalExprLte":1}}"#.into()),
        (r#"{"v":1}"#.into(), r#"{"$and":[{"control.min.v":{"$_internalExprLte":1}},{"control.max.v":{"$_internalExprGte":1}}]}"#.into()),
        (r#"{"v":{"$gt":1,"$lt":5}}"#.into(), r#"{"$and":[{"control.max.v":{"$_internalExprGt":1}},{"control.min.v":{"$_internalExprLt":5}}]}"#.into()),
        (r#"{"m":1}"#.into(), r#"{"meta":{"$eq":1}}"#.into()),
        (r#"{"m.s":{"$gt":1}}"#.into(), r#"{"meta.s":{"$gt":1}}"#.into()),
        (r#"{"m":{"$in":[1,2]}}"#.into(), r#"{"meta":{"$in":[1,2]}}"#.into()),
        (r#"{"v.w":{"$gt":1}}"#.into(), r#"{"control.max.v.w":{"$_internalExprGt":1}}"#.into()),
        (format!(r#"{{"t":{{"$gt":{t}}}}}"#), r#"{"$and":[{"control.max.t":{"$_internalExprGt":{"$date":"2026-01-01T00:00:00Z"}}},{"control.min.t":{"$_internalExprGt":{"$date":"2025-12-31T23:00:00Z"}}}]}"#.into()),
        (format!(r#"{{"t":{{"$lte":{t}}}}}"#), r#"{"$and":[{"control.min.t":{"$_internalExprLte":{"$date":"2026-01-01T00:00:00Z"}}},{"control.max.t":{"$_internalExprLte":{"$date":"2026-01-01T01:00:00Z"}}}]}"#.into()),
        (r#"{"v":{"$exists":true}}"#.into(), r#"{"$and":[{"control.min.v":{"$exists":true}},{"control.max.v":{"$exists":true}}]}"#.into()),
        (r#"{"$and":[{"v":{"$gt":1}},{"m":2}]}"#.into(), r#"{"meta":{"$eq":2},"control.max.v":{"$_internalExprGt":1}}"#.into()),
        (r#"{"v":{"$gt":1},"m":2}"#.into(), r#"{"meta":{"$eq":2},"control.max.v":{"$_internalExprGt":1}}"#.into()),
        (r#"{"v":{"$in":[1,2]}}"#.into(), r#"{"$or":[{"$and":[{"control.min.v":{"$_internalExprLte":1}},{"control.max.v":{"$_internalExprGte":1}}]},{"$and":[{"control.min.v":{"$_internalExprLte":2}},{"control.max.v":{"$_internalExprGte":2}}]}]}"#.into()),
        (r#"{"$or":[{"v":1},{"m":1}]}"#.into(), r#"{"$or":[{"$and":[{"control.min.v":{"$_internalExprLte":1}},{"control.max.v":{"$_internalExprGte":1}}]},{"meta":{"$eq":1}}]}"#.into()),
    ];
    for (i, (asked, want)) in cases.iter().enumerate() {
        let r = run(
            &mut m,
            &format!(
                r#"{{"createIndexes":"ts","indexes":[{{"key":{{"m":1,"k{i}":1}},"name":"i{i}","partialFilterExpression":{asked}}}]}}"#
            ),
        );
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{asked}: {r:?}");
        let listed = index_list(&mut m, true);
        let got = listed
            .iter()
            .find(|x| x.get("name") == Some(&Value::String(format!("i{i}"))))
            .expect("listed");
        assert_eq!(field(got, "partialFilterExpression"), &Value::Document(json(want)), "{asked}");
    }
    // On a coarser time series the time bounds move by its span.
    run(
        &mut m,
        r#"{"create":"day","timeseries":{"timeField":"t","metaField":"m","granularity":"minutes"}}"#,
    );
    let r = run(
        &mut m,
        &format!(
            r#"{{"createIndexes":"day","indexes":[{{"key":{{"m":1,"x":1}},"name":"e","partialFilterExpression":{{"t":{t}}}}}]}}"#
        ),
    );
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let r = m.run("m", Document::new().with("listIndexes", "day").with("rawData", true));
    let got =
        batch(&r).into_iter().find(|x| x.get("name") == Some(&Value::from("e"))).expect("listed");
    assert_eq!(
        field(&got, "partialFilterExpression"),
        &Value::Document(json(
            r#"{"$and":[{"control.min.t":{"$_internalExprLte":{"$date":"2026-01-01T00:00:00Z"}}},{"control.min.t":{"$_internalExprGte":{"$date":"2025-12-31T00:00:00Z"}}},{"control.max.t":{"$_internalExprGte":{"$date":"2026-01-01T00:00:00Z"}}},{"control.max.t":{"$_internalExprLte":{"$date":"2026-01-02T00:00:00Z"}}}]}"#
        ))
    );
    let refused = [
        (r#"{"v":{"$ne":1}}"#, "can't handle this predicate: { v: { $not: { $eq: 1 } } }"),
        (r#"{"v":{"$nin":[1]}}"#, "can't handle this predicate: { v: { $not: { $in: [ 1 ] } } }"),
        (
            r#"{"v":{"$exists":false}}"#,
            "can't handle this predicate: { v: { $not: { $exists: true } } }",
        ),
        (r#"{"v":null}"#, "can't handle comparison to null: { v: { $eq: null } }"),
        (r#"{"$nor":[{"v":1}]}"#, "can't handle this predicate: { $nor: [ { v: { $eq: 1 } } ] }"),
        (
            r#"{"v":{"$elemMatch":{"a":1}}}"#,
            "can't handle this predicate: { v: { $elemMatch: { a: { $eq: 1 } } } }",
        ),
        (r#"{"v":{"$type":"int"}}"#, "can't handle this predicate: { v: { $type: [ 16 ] } }"),
        // Measured on a `refnode` 9.0.4 on 02-11-2026: the types once each, sorted, `number` first;
        // the operator refused alone; an `$elemMatch` one predicate each.
        (
            r#"{"v":{"$type":["string","int","string"]}}"#,
            "can't handle this predicate: { v: { $type: [ 2, 16 ] } }",
        ),
        (
            r#"{"v":{"$type":["number",1,"minKey"]}}"#,
            r#"can't handle this predicate: { v: { $type: [ "number", -1, 1 ] } }"#,
        ),
        (r#"{"v":{"$type":2}}"#, "can't handle this predicate: { v: { $type: [ 2 ] } }"),
        (
            r#"{"v":{"$exists":true,"$ne":1}}"#,
            "can't handle this predicate: { v: { $not: { $eq: 1 } } }",
        ),
        (
            r#"{"v":{"$nin":[2],"$ne":1}}"#,
            "can't handle this predicate: { v: { $not: { $in: [ 2 ] } } }",
        ),
        (
            r#"{"v":{"$gt":1,"$type":"int"}}"#,
            "can't handle this predicate: { v: { $type: [ 16 ] } }",
        ),
        (
            r#"{"v":{"$elemMatch":{"$gt":5,"$lt":9}}}"#,
            "can't handle this predicate: { v: { $elemMatch: { $gt: 5, $lt: 9 } } }",
        ),
        (
            r#"{"v":{"$elemMatch":{"a":{"$ne":1}}}}"#,
            "can't handle this predicate: { v: { $elemMatch: { a: { $not: { $eq: 1 } } } } }",
        ),
        (
            r#"{"v":{"$elemMatch":{"a":1,"b":{"$gt":2,"$lt":5}}}}"#,
            "can't handle this predicate: { v: { $elemMatch: { $and: [ { a: { $eq: 1 } }, { b: { $gt: 2 } }, { b: { $lt: 5 } } ] } } }",
        ),
    ];
    for (bad, code, msg) in [
        (r#"{"v":{"$in":5}}"#, 2, "$in needs an array"),
        (r#"{"$and":[1]}"#, 2, "$and argument's entries must be objects"),
        (r#"{"$or":[{"v":1},2]}"#, 2, "$or argument's entries must be objects"),
    ] {
        let r = run(
            &mut m,
            &format!(
                r#"{{"createIndexes":"ts","indexes":[{{"key":{{"m":1,"z":1}},"name":"z","partialFilterExpression":{bad}}}]}}"#
            ),
        );
        assert_eq!(field(&r, "code"), &Value::Int32(code), "{bad}: {r:?}");
        assert_eq!(field(&r, "errmsg"), &Value::from(msg), "{bad}");
    }
    for (asked, msg) in refused {
        let r = run(
            &mut m,
            &format!(
                r#"{{"createIndexes":"ts","indexes":[{{"key":{{"m":1,"z":1}},"name":"z","partialFilterExpression":{asked}}}]}}"#
            ),
        );
        assert_eq!(field(&r, "code"), &Value::Int32(5_916_301), "{asked}: {r:?}");
        assert_eq!(
            field(&r, "errmsg"),
            &Value::String(format!(
                "Error translating non-metadata time-series predicate to operate on buckets: {msg}"
            )),
            "{asked}"
        );
    }
    // A bucket index with a partial filter keeps exactly the buckets that may hold a match.
    run(
        &mut m,
        &format!(
            r#"{{"insert":"ts","documents":[{{"t":{t},"m":1,"v":5}},{{"t":{t},"m":2,"v":0}}]}}"#
        ),
    );
    let r = run(&mut m, r#"{"count":"ts","query":{"v":{"$gt":1}}}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(1));
}

/// The bucket counters of `collStats`, after the steps a `refrouter` 9.0.4 counted on 31-10-2026.
#[test]
fn coll_stats_counts_the_buckets_as_refdb_does() {
    let mut m = Mem::new();
    run(&mut m, r#"{"create":"ts","timeseries":{"timeField":"t","metaField":"m"}}"#);
    let at = |ms: i64| date(T0 + ms);
    let insert = |m: &mut Mem, docs: Vec<String>| {
        let r = run(m, &format!(r#"{{"insert":"ts","documents":[{}]}}"#, docs.join(",")));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    };
    let stats = |m: &mut Mem| -> Document {
        let r = run(m, r#"{"collStats":"ts"}"#);
        let Value::Document(ts) = field(&r, "timeseries") else { panic!("{r:?}") };
        ts.clone()
    };
    let check = |ts: &Document, want: &[(&str, i32)]| {
        for (k, v) in want {
            assert_eq!(ts.get(k), Some(&Value::Int32(*v)), "{k} in {ts:?}");
        }
    };
    insert(&mut m, (0..5).map(|i| format!(r#"{{"t":{},"m":1,"v":{i}}}"#, at(i * 1000))).collect());
    check(
        &stats(&mut m),
        &[
            ("bucketCount", 1),
            ("numBucketInserts", 1),
            ("numBucketUpdates", 0),
            ("numBucketsOpenedDueToMetadata", 1),
            ("numCommits", 1),
            ("numMeasurementsCommitted", 5),
            ("avgNumMeasurementsPerCommit", 5),
        ],
    );
    insert(&mut m, vec![format!(r#"{{"t":{},"m":1,"v":9}}"#, at(10_000))]);
    check(
        &stats(&mut m),
        &[
            ("numBucketUpdates", 1),
            ("numCommits", 2),
            ("numMeasurementsCommitted", 6),
            ("avgNumMeasurementsPerCommit", 3),
        ],
    );
    insert(&mut m, vec![format!(r#"{{"t":{},"m":1,"v":9}}"#, at(7_200_000))]);
    check(
        &stats(&mut m),
        &[
            ("bucketCount", 2),
            ("numBucketInserts", 2),
            ("numBucketsClosedDueToTimeForward", 1),
            ("numCommits", 3),
        ],
    );
    insert(&mut m, vec![format!(r#"{{"t":{},"m":1,"v":9}}"#, at(-7_200_000))]);
    check(
        &stats(&mut m),
        &[
            ("bucketCount", 3),
            ("numBucketInserts", 3),
            ("numBucketsArchivedDueToTimeBackward", 1),
            ("numCommits", 4),
        ],
    );
    insert(
        &mut m,
        (0..1000).map(|i| format!(r#"{{"t":{},"m":2,"v":{i}}}"#, at(20_000 + i))).collect(),
    );
    check(
        &stats(&mut m),
        &[
            ("bucketCount", 4),
            ("numBucketInserts", 4),
            ("numBucketsOpenedDueToMetadata", 2),
            ("numCommits", 5),
            ("numMeasurementsCommitted", 1008),
            ("avgNumMeasurementsPerCommit", 201),
        ],
    );
    insert(&mut m, vec![format!(r#"{{"t":{},"m":2,"v":1}}"#, at(30_000))]);
    check(
        &stats(&mut m),
        &[
            ("bucketCount", 5),
            ("numBucketInserts", 5),
            ("numBucketsClosedDueToCount", 1),
            ("numCommits", 6),
        ],
    );
    insert(&mut m, vec![format!(r#"{{"t":{},"m":3,"s":"{}"}}"#, at(30_000), "x".repeat(130_000))]);
    insert(&mut m, vec![format!(r#"{{"t":{},"m":3,"s":"y"}}"#, at(30_001))]);
    check(
        &stats(&mut m),
        &[
            ("bucketCount", 6),
            ("numBucketInserts", 6),
            ("numBucketUpdates", 2),
            ("numBucketsKeptOpenDueToLargeMeasurements", 1),
            ("numBucketsOpenedDueToMetadata", 3),
            ("numCommits", 8),
            ("numMeasurementsCommitted", 1011),
            ("avgNumMeasurementsPerCommit", 126),
        ],
    );
}
