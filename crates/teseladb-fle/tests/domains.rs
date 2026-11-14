//! Range domains, precision checks and the corners of `encryptedFields` in `create` (017 FR-002,
//! FR-006): what each one accepts and the code it refuses with, from inputs built here.

use teseladb_bson::types as t;
use teseladb_bson::{Decimal128, Document, RawDoc, Value};
use teseladb_fle::fields::{self, EncryptedFieldConfig};
use teseladb_fle::{FleError, range, values};

fn bin(sub: u8, b: &[u8]) -> Value {
    Value::Binary { subtype: sub, bytes: b.to_vec() }
}

fn dec(s: &str) -> Decimal128 {
    let digits: u128 = s.trim_start_matches('-').parse().expect("digits");
    Decimal128::from_parts(s.starts_with('-'), digits, 0).expect("in range")
}

#[test]
fn integral_decimals_and_server_order() {
    assert_eq!(values::decimal_to_i64(dec("42")), Some(42));
    assert_eq!(values::decimal_to_i64(dec("-42")), Some(-42));
    assert_eq!(
        values::decimal_to_i64(Decimal128::from_parts(false, 420, -1).expect("ok")),
        Some(42)
    );
    assert_eq!(values::decimal_to_i64(Decimal128::from_parts(false, 425, -1).expect("ok")), None);
    assert_eq!(values::decimal_to_i64(Decimal128::from_parts(false, 4, 2).expect("ok")), Some(400));
    assert_eq!(values::decimal_to_i64(Decimal128::from_parts(false, 1, 30).expect("ok")), None);
    assert_eq!(values::decimal_to_f64(Decimal128::infinity(false)), None);
    assert_eq!(values::decimal_to_f64(dec("-3")), Some(-3.0));
    assert_eq!(values::compare(&Value::Int32(1), &Value::Double(2.0)), std::cmp::Ordering::Less);
}

fn bits(ty: u8, min: Option<Value>, max: Option<Value>, precision: Option<i32>) -> u32 {
    range::domain_bits(ty, min.as_ref(), max.as_ref(), precision)
}

#[test]
fn range_domains_take_the_bits_their_bounds_and_precision_need() {
    assert_eq!(bits(t::INT32, None, None, None), 32);
    assert_eq!(bits(t::INT64, Some(Value::Int64(0)), Some(Value::Int64(8)), None), 4);
    assert_eq!(bits(t::INT32, Some(Value::Int32(5)), Some(Value::Int32(5)), None), 32);
    assert_eq!(bits(t::DOUBLE, Some(Value::Double(0.0)), Some(Value::Double(1.0)), Some(2)), 8);
    assert_eq!(bits(t::DOUBLE, Some(Value::Double(0.0)), Some(Value::Double(0.5)), Some(0)), 64);
    assert_eq!(bits(t::DOUBLE, Some(Value::Double(2.0)), Some(Value::Double(1.0)), Some(1)), 64);
    assert_eq!(bits(t::DOUBLE, Some(Value::Double(0.0)), Some(Value::Double(1e20)), Some(1)), 64);
    assert_eq!(bits(t::DOUBLE, Some(Value::Double(0.0)), Some(Value::Double(1e14)), Some(2)), 64);
    assert_eq!(bits(t::DOUBLE, Some(Value::Double(0.0)), Some(Value::Double(3.0)), Some(0)), 2);
    assert_eq!(
        bits(
            t::DECIMAL128,
            Some(Value::Decimal128(dec("0"))),
            Some(Value::Decimal128(dec("1"))),
            Some(1)
        ),
        5
    );
    assert_eq!(bits(t::STRING, None, None, None), 64);
    assert_eq!(bits(t::INT32, Some(Value::String("a".into())), None, None), 32);
}

#[test]
fn precision_checks_accept_exact_bounds_and_refuse_a_nan() {
    let ints = (&Value::Int32(0), &Value::Int32(1));
    assert!(range::check_precision(t::INT32, ints, 1, "a").is_ok());
    let zero = Value::Decimal128(Decimal128::from_parts(false, 0, -3).expect("ok"));
    let tenth = Value::Decimal128(Decimal128::from_parts(false, 10, -2).expect("ok"));
    assert!(range::check_precision(t::DECIMAL128, (&zero, &tenth), 1, "a").is_ok());
    let nan = Value::Decimal128(Decimal128::nan());
    assert!(range::check_precision(t::DECIMAL128, (&nan, &tenth), 1, "a").is_err());
    assert!(!fields::is_metadata("c") && fields::is_metadata("enxcol_.c.esc"));
}

fn create(ef: &Document) -> Result<EncryptedFieldConfig, FleError> {
    let b = ef.to_bytes().expect("bytes");
    fields::for_create("c", RawDoc::from_bytes(&b).expect("raw"))
}

fn codes(ef: &Document) -> Result<(), i32> {
    create(ef).map(|_| ()).map_err(|e| e.code)
}

fn field(path: &str, bson_type: &str, q: Option<Document>) -> Value {
    let len = u8::try_from(path.len()).expect("short path");
    let mut f = Document::new()
        .with("keyId", bin(t::BINARY_UUID, &[len; 16]))
        .with("path", path)
        .with("bsonType", bson_type);
    if let Some(q) = q {
        f.push("queries", q);
    }
    Value::Document(f)
}

fn one(f: Value) -> Document {
    Document::new().with("fields", vec![f])
}

fn eq(c: Value) -> Option<Document> {
    Some(Document::new().with("queryType", "equality").with("contention", c))
}

#[test]
fn contention_must_be_integral() {
    assert_eq!(codes(&one(field("a", "string", eq(Value::Double(f64::NAN))))), Err(9));
    assert_eq!(codes(&one(field("a", "string", eq(Value::Double(2.5))))), Err(9));
    let fraction = Value::Decimal128(Decimal128::from_parts(false, 25, -1).expect("ok"));
    assert_eq!(codes(&one(field("a", "string", eq(fraction)))), Err(9));
}

#[test]
fn create_refuses_bad_keys_paths_and_metadata_names() {
    let prec = Some(
        Document::new()
            .with("queryType", "range")
            .with("min", 0.0)
            .with("max", 1.0)
            .with("precision", Value::Int64(1 << 40)),
    );
    assert!(create(&one(field("a", "double", prec))).is_err());
    let short_key = Document::new().with("keyId", bin(t::BINARY_UUID, &[1; 15])).with("path", "a");
    assert_eq!(codes(&one(Value::Document(short_key))), Err(14));
    for (k, v) in [
        ("escCollection", "enxcol_.c.x"),
        ("ecocCollection", "enxcol_.c.x"),
        ("escCollection", "x.esc"),
        ("ecocCollection", "x.ecoc"),
    ] {
        let ef = Document::new().with(k, v).with("fields", Vec::<Value>::new());
        assert!(matches!(codes(&ef), Err(7_406_900 | 7_406_902)), "{k} {v}");
    }
    let given = Document::new()
        .with("escCollection", "enxcol_.c.esc")
        .with("ecocCollection", "enxcol_.c.ecoc")
        .with("fields", Vec::<Value>::new());
    assert!(create(&given).is_ok());
    assert_eq!(codes(&one(field("_id.x", "string", None))), Err(6_316_403));
    let two = Document::new()
        .with("fields", vec![field("a", "string", None), field("ab", "string", None)]);
    assert!(create(&two).is_ok());
}

#[test]
fn create_refuses_half_ranges_and_short_substrings() {
    let min_only = Some(Document::new().with("queryType", "range").with("min", 0.0));
    assert_eq!(codes(&one(field("a", "double", min_only))), Err(6_967_100));
    let sub = Some(
        Document::new()
            .with("queryType", "substring")
            .with("strMaxLength", 10)
            .with("strMinQueryLength", 1)
            .with("strMaxQueryLength", 3)
            .with("caseSensitive", true)
            .with("diacriticSensitive", true),
    );
    assert_eq!(codes(&one(field("a", "string", sub))), Err(12_860_001));
    let plain = Document::new()
        .with("keyId", bin(t::BINARY_UUID, &[1; 16]))
        .with("path", "a")
        .with("bsonType", "string");
    let cfg = create(&Document::new().with("fields", vec![Value::Document(plain)])).expect("ok");
    assert_eq!(cfg.fields[0].query(), None);
}
