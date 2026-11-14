//! Hashed index keys (009 T025, FR-014): every vector measured on a real reference server
//! (`tests/data/hashed-vectors.json`, with the image digest and the date) comes out the same.

use teseladb_bson::{RawDoc, RawValue, Value, extjson};
use teseladb_index::keygen::hashed_key;

const VECTORS: &str = include_str!("data/hashed-vectors.json");

fn text(v: Option<&Value>) -> &str {
    match v {
        Some(Value::String(s)) => s,
        other => panic!("not a string: {other:?}"),
    }
}

#[test]
fn every_measured_value_hashes_as_the_reference_does() {
    let file = extjson::parse(VECTORS).unwrap();
    let Some(Value::Array(vectors)) = file.get("vectors") else { panic!("no vectors") };
    assert!(vectors.len() > 100);
    let mut wrong = Vec::new();
    for v in vectors {
        let Value::Document(v) = v else { panic!("a vector is a document") };
        let input = text(v.get("v"));
        let expected: i64 = text(v.get("h")).parse().unwrap();
        let bytes = extjson::parse(input).unwrap().to_bytes().unwrap();
        let doc = RawDoc::from_bytes(&bytes).unwrap();
        let value = doc.get("v").unwrap();
        if hashed_key(value) != expected {
            wrong.push(input.to_string());
        }
    }
    assert!(wrong.is_empty(), "{} vectors differ: {wrong:#?}", wrong.len());
}

#[test]
fn numbers_hash_by_their_integer_value() {
    let one = hashed_key(RawValue::Int32(1));
    assert_eq!(hashed_key(RawValue::Int64(1)), one);
    assert_eq!(hashed_key(RawValue::Double(1.9)), one, "a double is truncated");
    assert_eq!(hashed_key(RawValue::Double(f64::NAN)), hashed_key(RawValue::Int32(0)));
    assert_eq!(hashed_key(RawValue::Double(f64::INFINITY)), hashed_key(RawValue::Int64(i64::MAX)));
    assert_eq!(
        hashed_key(RawValue::Double(9_223_372_036_854_775_808.0)),
        hashed_key(RawValue::Int64(i64::MIN))
    );
}

#[test]
fn undefined_and_db_pointers_have_classes_of_their_own() {
    // Not measured: the reference shell sends `undefined` as null and cannot build a DBPointer (009
    // research R6). They hash with their own type class, apart from null and from strings.
    assert_ne!(hashed_key(RawValue::Undefined), hashed_key(RawValue::Null));
    let p = RawValue::DbPointer { namespace: "db.c", id: [7; 12] };
    assert_ne!(hashed_key(p), hashed_key(RawValue::String("db.c")));
}

#[test]
fn decimals_round_half_to_even_and_saturate() {
    use teseladb_bson::Decimal128;
    let dec = |neg, c, e| RawValue::Decimal128(Decimal128::from_parts(neg, c, e).unwrap());
    let int = |i: i64| hashed_key(RawValue::Int64(i));
    assert_eq!(hashed_key(dec(false, 25, -1)), int(2));
    assert_eq!(hashed_key(dec(false, 35, -1)), int(4));
    assert_eq!(hashed_key(dec(true, 25, -1)), int(-2));
    assert_eq!(hashed_key(dec(false, 0, 5)), int(0));
    assert_eq!(hashed_key(dec(false, 12, 3)), int(12_000));
    assert_eq!(hashed_key(dec(false, 1, 6000)), int(i64::MAX));
    assert_eq!(hashed_key(dec(true, 1, 30)), int(i64::MIN));
    assert_eq!(hashed_key(dec(false, 5, -60)), int(0), "far below one half");
}
