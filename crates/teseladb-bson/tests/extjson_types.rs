//! Extended JSON v2 of every BSON type (FR-015): each one in canonical and relaxed form, read
//! back to the same bytes, and the wrappers that break the specification.

use teseladb_bson::{Decimal128, Document, RawDoc, Value, extjson};

const ID: [u8; 12] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
const ID_HEX: &str = "000000000000000000000001";

/// `{"v": v}` in canonical, relaxed and relaxed-with-UUID form.
fn forms(v: Value) -> (String, String, String) {
    let bytes = Document::new().with("v", v).to_bytes().expect("a document");
    let raw = RawDoc::from_bytes(&bytes).expect("its bytes");
    (extjson::to_canonical(raw), extjson::to_relaxed(raw), extjson::to_relaxed_uuid(raw))
}

/// A decimal128 from its text.
fn decimal(text: &str) -> Decimal128 {
    text.parse().expect("a decimal128")
}

/// The canonical and relaxed forms of `{"v": v}`, given as the text of `v` in each.
fn check(v: Value, canonical: &str, relaxed: &str) {
    let shown = format!("{v:?}");
    let (c, r, _) = forms(v);
    assert_eq!(c, format!("{{\"v\":{canonical}}}"), "canonical of {shown}");
    assert_eq!(r, format!("{{\"v\":{relaxed}}}"), "relaxed of {shown}");
}

#[test]
fn every_type_has_its_canonical_and_relaxed_form() {
    check(Value::Double(1.5), r#"{"$numberDouble":"1.5"}"#, "1.5");
    check(Value::Double(1e20), r#"{"$numberDouble":"1.0E+20"}"#, "1.0E+20");
    check(Value::Double(1.5e-7), r#"{"$numberDouble":"1.5E-7"}"#, "1.5E-7");
    for (x, text) in
        [(f64::NAN, "NaN"), (f64::INFINITY, "Infinity"), (f64::NEG_INFINITY, "-Infinity")]
    {
        let wrapped = format!("{{\"$numberDouble\":\"{text}\"}}");
        check(Value::Double(x), &wrapped, &wrapped);
    }
    check(
        Value::String("q\"b\\n\nr\rt\t\u{1}".into()),
        r#""q\"b\\n\nr\rt\t\u0001""#,
        r#""q\"b\\n\nr\rt\t\u0001""#,
    );
    check(
        Value::Array(vec![Value::Int32(1), Value::from("a")]),
        r#"[{"$numberInt":"1"},"a"]"#,
        r#"[1,"a"]"#,
    );
    let bin = r#"{"$binary":{"base64":"AQID","subType":"00"}}"#;
    check(Value::Binary { subtype: 0, bytes: vec![1, 2, 3] }, bin, bin);
    check(Value::Undefined, r#"{"$undefined":true}"#, r#"{"$undefined":true}"#);
    let oid = format!("{{\"$oid\":\"{ID_HEX}\"}}");
    check(Value::ObjectId(ID), &oid, &oid);
    check(Value::Boolean(true), "true", "true");
    check(Value::Boolean(false), "false", "false");
    check(
        Value::DateTime(1_500),
        r#"{"$date":{"$numberLong":"1500"}}"#,
        r#"{"$date":"1970-01-01T00:00:01.500Z"}"#,
    );
    // Before 1970 a relaxed date keeps its milliseconds.
    let before = r#"{"$date":{"$numberLong":"-1"}}"#;
    check(Value::DateTime(-1), before, before);
    check(Value::Null, "null", "null");
    let regex = r#"{"$regularExpression":{"pattern":"a","options":"im"}}"#;
    check(Value::Regex { pattern: "a".into(), options: "mi".into() }, regex, regex);
    let pointer = format!("{{\"$dbPointer\":{{\"$ref\":\"db.c\",\"$id\":{oid}}}}}");
    check(Value::DbPointer { namespace: "db.c".into(), id: ID }, &pointer, &pointer);
    check(Value::JavaScript("x".into()), r#"{"$code":"x"}"#, r#"{"$code":"x"}"#);
    check(Value::Symbol("s".into()), r#"{"$symbol":"s"}"#, r#"{"$symbol":"s"}"#);
    check(
        Value::JavaScriptWithScope { code: "x".into(), scope: Document::new().with("a", 1) },
        r#"{"$code":"x","$scope":{"a":{"$numberInt":"1"}}}"#,
        r#"{"$code":"x","$scope":{"a":1}}"#,
    );
    check(Value::Int32(-7), r#"{"$numberInt":"-7"}"#, "-7");
    check(Value::Int64(1 << 40), r#"{"$numberLong":"1099511627776"}"#, "1099511627776");
    let ts = r#"{"$timestamp":{"t":1,"i":2}}"#;
    check(Value::Timestamp { time: 1, increment: 2 }, ts, ts);
    let dec = r#"{"$numberDecimal":"1.5"}"#;
    check(Value::Decimal128(decimal("1.5")), dec, dec);
    check(Value::MinKey, r#"{"$minKey":1}"#, r#"{"$minKey":1}"#);
    check(Value::MaxKey, r#"{"$maxKey":1}"#, r#"{"$maxKey":1}"#);
}

#[test]
fn only_a_uuid_of_sixteen_bytes_is_written_as_uuid() {
    let (_, _, short) = forms(Value::Binary { subtype: 4, bytes: vec![1, 2, 3] });
    assert_eq!(short, r#"{"v":{"$binary":{"base64":"AQID","subType":"04"}}}"#);
    let (_, relaxed, uuid) = forms(Value::Binary { subtype: 4, bytes: (0..16).collect() });
    assert_eq!(uuid, r#"{"v":{"$uuid":"00010203-0405-0607-0809-0a0b0c0d0e0f"}}"#);
    assert!(relaxed.contains("$binary"), "{relaxed}");
}

#[test]
fn the_canonical_form_of_every_type_reads_back_the_same_bytes() {
    let d = Document::new()
        .with("d", 1.5)
        .with("s", "t")
        .with("o", Document::new().with("x", Value::Null))
        .with("a", Value::Array(vec![Value::Int32(1)]))
        .with("b", Value::Binary { subtype: 0x80, bytes: vec![9] })
        .with("u", Value::Undefined)
        .with("id", Value::ObjectId(ID))
        .with("t", true)
        .with("dt", Value::DateTime(-5))
        .with("re", Value::Regex { pattern: "p".into(), options: "i".into() })
        .with("ptr", Value::DbPointer { namespace: "n".into(), id: ID })
        .with("js", Value::JavaScript("f".into()))
        .with("sym", Value::Symbol("y".into()))
        .with("jss", Value::JavaScriptWithScope { code: "g".into(), scope: Document::new() })
        .with("i", 3)
        .with("ts", Value::Timestamp { time: 4, increment: 5 })
        .with("l", Value::Int64(6))
        .with("dec", Value::Decimal128(decimal("-0.25")))
        .with("min", Value::MinKey)
        .with("max", Value::MaxKey);
    let bytes = d.to_bytes().expect("a document");
    let text = extjson::to_canonical(RawDoc::from_bytes(&bytes).expect("its bytes"));
    let back = extjson::parse(&text).expect("its canonical form");
    assert_eq!(back.to_bytes().expect("the document read back"), bytes, "{text}");
}

#[test]
fn wrappers_that_break_the_specification_are_refused() {
    for bad in [
        "{\"a\\u0000b\": 1}",
        r#"{"v": {"$oid": "000000000000000000000001", "x": 1}}"#,
        r#"{"v": {"$regularExpression": {"pattern": "a\u0000", "options": ""}}}"#,
        r#"{"v": {"$regularExpression": {"pattern": "a", "options": "\u0000"}}}"#,
        r#"{"v": {"$regularExpression": {"pattern": "a", "options": 1}}}"#,
        r#"{"v": {"$regularExpression": {"pattern": "a"}}}"#,
        r#"{"v": {"$dbPointer": {"$ref": "n", "$id": {"x": "y"}}}}"#,
        r#"{"v": {"$dbPointer": {"$ref": "n", "$id": {"$oid": "000000000000000000000001", "x": 1}}}}"#,
        r#"{"v": {"$date": {"$numberLong": "1", "x": 1}}}"#,
        r#"{"v": {"$date": {"x": "1"}}}"#,
        r#"{"v": {"$binary": {"base64": 1, "subType": "00"}}}"#,
        r#"{"v": {"$uuid": "0001020304050607-0809-0a0b0c0d0e0f"}}"#,
        r#"{"v": {"$uuid": "0001020g-0405-0607-0809-0a0b0c0d0e0f"}}"#,
    ] {
        assert!(extjson::parse(bad).is_err(), "{bad}");
    }
}
