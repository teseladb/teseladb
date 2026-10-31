//! Bucket bounds of time series reads (015 T013, FR-007): the filter's conditions on the time and
//! meta fields skip the buckets that cannot match, and only those.

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_exec::timeseries_read::Bounds;
use teseladb_mql::timeseries_write::{BucketSpec, TsError};

const T0: i64 = 1_767_225_600_000;
const HOUR: i64 = 3_600_000;

fn spec() -> BucketSpec {
    BucketSpec {
        time_field: "t".into(),
        meta_field: Some("m".into()),
        max_span_seconds: 3600,
        rounding_seconds: 60,
    }
}

/// A bucket of meta `meta` with measurements at `T0 + offsets` (ms).
fn bucket(meta: &str, offsets: &[i64]) -> Vec<u8> {
    let s = spec();
    let rows: Vec<Document> =
        offsets.iter().map(|o| Document::new().with("t", Value::DateTime(T0 + o))).collect();
    let id = s.bucket_id(T0 + offsets[0], Some(&Value::from(meta)), 0).expect("an id");
    s.build(id, Some(&Value::from(meta)), &rows).expect("a bucket").to_bytes().expect("bytes")
}

fn filter(text: &str) -> Document {
    teseladb_bson::extjson::parse(text).expect("a filter")
}

fn matches(f: &str, b: &[u8]) -> bool {
    let bounds = Bounds::of(&spec(), &filter(f));
    bounds.may_match(&spec(), RawDoc::from_bytes(b).expect("a bucket")).expect("a bucket")
}

#[test]
fn an_hour_of_one_sensor_reads_only_the_buckets_that_overlap() {
    let first = bucket("a", &[1000, 5 * 60_000]);
    let second = bucket("a", &[HOUR + 1000]);
    let other = bucket("b", &[1000]);
    let hour = format!(
        r#"{{"m":"a","t":{{"$gte":{{"$date":{{"$numberLong":"{}"}}}},"$lt":{{"$date":{{"$numberLong":"{}"}}}}}}}}"#,
        T0,
        T0 + HOUR
    );
    assert!(matches(&hour, &first));
    assert!(!matches(&hour, &second));
    assert!(!matches(&hour, &other));
    // Without bounds every bucket may match.
    for b in [&first, &second, &other] {
        assert!(matches("{}", b));
        assert!(matches(r#"{"x":1}"#, b));
    }
}

#[test]
fn bounds_come_from_the_top_level_and_its_ands() {
    let at = |ms: i64| format!(r#"{{"$date":{{"$numberLong":"{ms}"}}}}"#);
    let f = format!(
        r#"{{"$and":[{{"t":{{"$gt":{}}}}},{{"t":{{"$lte":{}}}}}],"t":{{"$gte":{}}},"m":7}}"#,
        at(T0),
        at(T0 + 10),
        at(T0 + 5)
    );
    let b = Bounds::of(&spec(), &filter(&f));
    assert_eq!(
        b,
        Bounds { after: Some(T0 + 5), before: Some(T0 + 10), meta: Some(Value::Int32(7)) }
    );
    let strict = format!(r#"{{"t":{{"$gt":{},"$lt":{}}}}}"#, at(T0), at(T0 + 10));
    let b = Bounds::of(&spec(), &filter(&strict));
    assert_eq!((b.after, b.before), (Some(T0 + 1), Some(T0 + 9)));
    let eq = Bounds::of(&spec(), &filter(&format!(r#"{{"t":{}}}"#, at(T0))));
    assert_eq!((eq.after, eq.before), (Some(T0), Some(T0)));
    let eq = Bounds::of(&spec(), &filter(&format!(r#"{{"t":{{"$eq":{}}}}}"#, at(T0))));
    assert_eq!((eq.after, eq.before), (Some(T0), Some(T0)));
    // What the bounds cannot judge is left to the measurements.
    for f in [
        r#"{"t":5}"#,
        r#"{"t":{"$ne":1,"$in":[2]}}"#,
        r#"{"m":{"x":1}}"#,
        r#"{"m":[1]}"#,
        r#"{"m":{"$regularExpression":{"pattern":"a","options":""}}}"#,
        r#"{"$and":[1]}"#,
        r#"{"$and":5}"#,
    ] {
        assert_eq!(Bounds::of(&spec(), &filter(f)), Bounds::default(), "{f}");
    }
    let no_meta = BucketSpec { meta_field: None, ..spec() };
    assert_eq!(Bounds::of(&no_meta, &filter(r#"{"m":1}"#)), Bounds::default());
}

#[test]
fn a_bucket_without_control_is_refused() {
    let b = Bounds::default();
    for bad in [
        Document::new(),
        Document::new().with("control", Document::new()),
        Document::new().with("control", Document::new().with("min", Document::new())),
        Document::new().with("control", Document::new().with("min", 1)),
    ] {
        let bytes = bad.to_bytes().expect("small");
        let r = b.may_match(&spec(), RawDoc::from_bytes(&bytes).expect("a document"));
        assert!(matches!(r, Err(TsError::Malformed(_))), "{bad:?}");
    }
    // A bucket without meta never matches a meta bound.
    let s = BucketSpec { meta_field: None, ..spec() };
    let rows = vec![Document::new().with("t", Value::DateTime(T0))];
    let id = s.bucket_id(T0, None, 0).expect("an id");
    let bytes = s.build(id, None, &rows).expect("a bucket").to_bytes().expect("bytes");
    let meta = Bounds { meta: Some(Value::Int32(1)), ..Bounds::default() };
    assert_eq!(meta.may_match(&spec(), RawDoc::from_bytes(&bytes).expect("a bucket")), Ok(false));
}
