//! Time series buckets (015 T012, FR-006): measurements packed into a bucket with compressed
//! columns, its `control` as a `refrouter` 9.0.4 writes it (measured with `rawData: true` on
//! 31-10-2026), and unpacked again.

use teseladb_mql::timeseries_write::{BucketSpec, Full, MAX_MEASUREMENTS, TsError};
use teseladb_bson::{Document, RawDoc, Value};

const T0: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z

fn spec() -> BucketSpec {
    BucketSpec {
        time_field: "t".into(),
        meta_field: Some("m".into()),
        max_span_seconds: 3600,
        rounding_seconds: 60,
    }
}

/// The measurements of the measurement: `_id`, `t`, `m`, `v`, `dd`, `s`, `o` and `z` on odd ones.
fn measurement(i: i32) -> Document {
    let mut d = Document::new()
        .with("_id", i)
        .with("t", Value::DateTime(T0 + i64::from(i) * 1000))
        .with("m", "a")
        .with("v", i * 3)
        .with("dd", f64::from(i) * 0.5)
        .with("s", if i % 3 == 0 { "x" } else { "y" })
        .with("o", Document::new().with("p", i));
    if i % 2 == 1 {
        d.push("z", true);
    }
    d
}

fn fill(n: i32) -> Document {
    let s = spec();
    let id = s.bucket_id(T0, Some(&Value::from("a")), 0).expect("an id");
    let mut bucket: Option<Vec<u8>> = None;
    let mut last = Document::new();
    for i in 0..n {
        let raw = bucket.as_deref().map(|b| RawDoc::from_bytes(b).expect("a bucket"));
        last = s.append(id, raw, &measurement(i)).expect("appended");
        bucket = Some(last.to_bytes().expect("a bucket"));
    }
    last
}

#[test]
fn a_bucket_has_the_control_refdb_writes() {
    let b = fill(10);
    let control = |k: &str| match b.get("control") {
        Some(Value::Document(c)) => c.get(k).cloned(),
        _ => None,
    };
    let min = Document::new()
        .with("_id", 0)
        .with("t", Value::DateTime(T0))
        .with("v", 0)
        .with("dd", 0.0)
        .with("s", "x")
        .with("o", Document::new().with("p", 0))
        .with("z", true);
    let max = Document::new()
        .with("_id", 9)
        .with("t", Value::DateTime(T0 + 9000))
        .with("v", 27)
        .with("dd", 4.5)
        .with("s", "y")
        .with("o", Document::new().with("p", 9))
        .with("z", true);
    assert_eq!(control("version"), Some(Value::Int32(2)));
    assert_eq!(control("min"), Some(Value::Document(min)));
    assert_eq!(control("max"), Some(Value::Document(max)));
    assert_eq!(control("count"), Some(Value::Int32(10)));
    assert_eq!(b.get("meta"), Some(&Value::from("a")));
    let keys: Vec<&str> = b.iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["_id", "control", "meta", "data"]);
}

#[test]
fn measurements_come_back_with_the_time_and_meta_first() {
    let b = fill(10);
    let bytes = b.to_bytes().expect("a bucket");
    let out = spec().unpack(RawDoc::from_bytes(&bytes).expect("a bucket")).expect("unpacked");
    assert_eq!(out.len(), 10);
    for (i, d) in out.iter().enumerate() {
        let mut want = measurement(i as i32);
        let t = want.remove("t").expect("t");
        want.remove("m");
        let mut expected = Document::new().with("t", t).with("m", "a");
        for (k, v) in want.iter() {
            expected.push(k, v.clone());
        }
        assert_eq!(d, &expected, "measurement {i}");
    }
    // Packed: far smaller than the measurements as documents.
    let docs: usize = (0..10).map(|i| measurement(i).to_bytes().expect("small").len()).sum();
    assert!(bytes.len() < docs, "{} bytes against {docs}", bytes.len());
}

#[test]
fn ids_name_the_window_the_meta_and_the_number() {
    let s = spec();
    let a = s.bucket_id(T0 + 59 * 60_000, Some(&Value::from("a")), 0).expect("an id");
    assert_eq!(&a[..4], &0x6955_b900u32.to_be_bytes(), "the window of 00:00");
    assert_eq!(s.bucket_id(T0, Some(&Value::from("a")), 0), Ok(a));
    assert_ne!(s.bucket_id(T0 + 3_600_000, Some(&Value::from("a")), 0), Ok(a), "next window");
    assert_ne!(s.bucket_id(T0, Some(&Value::from("b")), 0), Ok(a), "another meta");
    assert_ne!(s.bucket_id(T0, None, 0), Ok(a));
    let n = s.bucket_id(T0, Some(&Value::from("a")), 0x0001_0203).expect("an id");
    assert_eq!(&n[9..], &[1, 2, 3]);
    assert_eq!(s.window(-1), -3600, "before the epoch");
}

#[test]
fn a_full_bucket_or_a_measurement_without_time_is_refused() {
    let s = spec();
    let id = s.bucket_id(T0, None, 0).expect("an id");
    let no_time = Document::new().with("m", "a");
    assert_eq!(
        s.append(id, None, &no_time),
        Err(TsError::NoTime(
            "'t' must be present and contain a valid BSON UTC datetime value".into()
        ))
    );
    let rows: Vec<Document> = (0..MAX_MEASUREMENTS as i64)
        .map(|i| Document::new().with("t", Value::DateTime(T0 + i)))
        .collect();
    let full = s.build(id, None, &rows).expect("a bucket").to_bytes().expect("a bucket");
    let one = Document::new().with("t", Value::DateTime(T0));
    let raw = RawDoc::from_bytes(&full).expect("a bucket");
    assert_eq!(s.append(id, Some(raw), &one), Err(TsError::Full(Full::Count)));
    // A large measurement keeps its bucket open past its size while it has few measurements
    // (RefDb's `numBucketsKeptOpenDueToLargeMeasurements`).
    let big = Document::new().with("t", Value::DateTime(T0)).with("p", "x".repeat(130_000));
    let first = s.append(id, None, &big).expect("alone it fits").to_bytes().expect("a bucket");
    let raw = RawDoc::from_bytes(&first).expect("a bucket");
    let (_, taken, stop) = s.append_all(id, Some(raw), std::slice::from_ref(&one)).expect("open");
    assert_eq!((taken, stop.kept_open, stop.full), (1, true, None));
    // With ten measurements the bucket closes by size.
    let small: Vec<Document> =
        (0..10).map(|i| Document::new().with("t", Value::DateTime(T0 + i))).collect();
    let ten = s.build(id, None, &small).expect("a bucket").to_bytes().expect("a bucket");
    let raw = RawDoc::from_bytes(&ten).expect("a bucket");
    assert_eq!(s.append(id, Some(raw), &big), Err(TsError::Full(Full::Size)));
    // A bucket of 12 MiB closes whatever its count.
    let five = Document::new().with("t", Value::DateTime(T0)).with("p", "x".repeat(5_000_000));
    let one_big = s.append(id, None, &five).expect("alone it fits").to_bytes().expect("a bucket");
    let raw = RawDoc::from_bytes(&one_big).expect("a bucket");
    let more = Document::new().with("t", Value::DateTime(T0)).with("p", "y".repeat(7_600_000));
    assert_eq!(s.append(id, Some(raw), &more), Err(TsError::Full(Full::Size)));
}

#[test]
fn a_broken_bucket_is_refused() {
    let s = spec();
    for bad in [
        Document::new(),
        Document::new().with("control", Document::new()),
        Document::new().with("control", Document::new().with("count", 1)),
        Document::new()
            .with("control", Document::new().with("count", 1))
            .with("data", Document::new().with("t", 5)),
    ] {
        let bytes = bad.to_bytes().expect("small");
        let r = s.measurements(RawDoc::from_bytes(&bytes).expect("a document"));
        assert!(matches!(r, Err(TsError::Malformed(_))), "{bad:?}: {r:?}");
    }
}

/// A document's bytes as a raw document for the bucket readers.
fn read_both(
    s: &BucketSpec,
    bucket: &Document,
) -> (Result<usize, TsError>, Result<usize, TsError>, Result<usize, TsError>) {
    let bytes = bucket.to_bytes().expect("small");
    let raw = RawDoc::from_bytes(&bytes).expect("a document");
    (
        s.measurements(raw).map(|m| m.len()),
        s.unpack_raw(raw).map(|m| m.len()),
        s.unpack(raw).map(|m| m.len()),
    )
}

#[test]
fn malformed_buckets_are_refused_by_every_reader() {
    let s = spec();
    let control = |count: Value| Document::new().with("count", count);
    let cases = [
        Document::new(),
        Document::new().with("control", 1),
        Document::new().with("control", control(Value::String("x".into()))),
        Document::new().with("control", control(Value::Int32(-1))),
        Document::new().with("control", control(Value::Int32(1))),
        Document::new().with("control", control(Value::Int32(1))).with("data", 5),
        Document::new()
            .with("control", control(Value::Int32(1)))
            .with("data", Document::new().with("t", 5)),
        Document::new().with("control", control(Value::Int32(1))).with(
            "data",
            Document::new().with("t", Value::Binary { subtype: 7, bytes: vec![1, 2, 3] }),
        ),
    ];
    for bucket in cases {
        let (a, b, c) = read_both(&s, &bucket);
        assert!(matches!(a, Err(TsError::Malformed(_))), "{bucket:?}: {a:?}");
        assert!(matches!(b, Err(TsError::Malformed(_))), "{bucket:?}: {b:?}");
        assert!(matches!(c, Err(TsError::Malformed(_))), "{bucket:?}: {c:?}");
    }
}

#[test]
fn a_bucket_without_columns_unpacks_its_meta_alone() {
    let s = spec();
    let bucket = Document::new()
        .with("control", Document::new().with("count", Value::Int32(2)))
        .with("meta", "a")
        .with("data", Document::new());
    let bytes = bucket.to_bytes().expect("small");
    let raw = RawDoc::from_bytes(&bytes).expect("a document");
    let rows = s.unpack(raw).expect("two measurements");
    assert_eq!(rows, vec![Document::new().with("m", "a"), Document::new().with("m", "a")]);
    let m = Document::new().with("m", 3).with("t", Value::DateTime(T0));
    assert_eq!(s.meta_of(&m), Some(&Value::Int32(3)));
    let plain = BucketSpec { meta_field: None, ..spec() };
    assert_eq!(plain.meta_of(&m), None);
}

/// A meta value is kept with the fields of its documents sorted, also inside arrays, so that
/// the same value in another field order goes to the same bucket (measured on a `refnode` 9.0.4
/// on 02-11-2026: `{b: 1, a: {d: 1, c: 2}}` and `{a: {c: 2, d: 1}, b: 1}` share one bucket,
/// whose `meta` is `{a: {c: 2, d: 1}, b: 1}`; `[{b: 1, a: 2}, 3]` comes back `[{a: 2, b: 1}, 3]`).
#[test]
fn a_meta_value_is_kept_with_its_fields_sorted() {
    let s = spec();
    let inner = |first: (&str, i32), second: (&str, i32)| {
        Value::Document(Document::new().with(first.0, first.1).with(second.0, second.1))
    };
    let one = Value::Document(Document::new().with("b", 1).with("a", inner(("d", 1), ("c", 2))));
    let other = Value::Document(Document::new().with("a", inner(("c", 2), ("d", 1))).with("b", 1));
    assert_eq!(s.bucket_id(T0, Some(&one), 0), s.bucket_id(T0, Some(&other), 0));
    let m = Document::new().with("t", Value::DateTime(T0)).with("m", one);
    let id = s.bucket_id(T0, s.meta_of(&m), 0).unwrap();
    let bucket = s.append(id, None, &m).unwrap();
    assert_eq!(bucket.get("meta"), Some(&other));
    let listed = Value::Array(vec![inner(("b", 1), ("a", 2)), Value::Int32(3)]);
    let sorted = Value::Array(vec![inner(("a", 2), ("b", 1)), Value::Int32(3)]);
    let built = s.build(id, Some(&listed), &[]).unwrap();
    assert_eq!(built.get("meta"), Some(&sorted));
}
