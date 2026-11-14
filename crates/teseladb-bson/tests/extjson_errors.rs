//! Extended JSON that must be refused, wrapper by wrapper, beyond the corpus's
//! parse errors: fields of the wrong JSON type or out of range, base64 and hexadecimal that are
//! not, and integers that do not fit. Plain JSON numbers past `i64` become doubles.

use teseladb_bson::{RawDoc, RawValue, extjson};

fn refused(text: &str) {
    assert!(extjson::parse(text).is_err(), "{text} was accepted");
}

#[test]
fn every_wrapper_refuses_what_is_not_its_shape() {
    let oid = "56e1fc72e0c917e9c4714161";
    for text in [
        // $dbPointer: $ref not a string, $id not an $oid.
        format!(r#"{{"a":{{"$dbPointer":{{"$ref":1,"$id":{{"$oid":"{oid}"}}}}}}}}"#),
        r#"{"a":{"$dbPointer":{"$ref":"b","$id":1}}}"#.to_owned(),
        r#"{"a":{"$dbPointer":{"$ref":"b","$id":{"x":1}}}}"#.to_owned(),
        // $date: not ISO-8601, not $numberLong, $numberLong not a string.
        r#"{"a":{"$date":"yesterday"}}"#.to_owned(),
        r#"{"a":{"$date":{"x":"1"}}}"#.to_owned(),
        r#"{"a":{"$date":{"$numberLong":1}}}"#.to_owned(),
        // $timestamp: a field missing, a value past 32 bits or negative.
        r#"{"a":{"$timestamp":{"t":1,"x":2}}}"#.to_owned(),
        r#"{"a":{"$timestamp":{"t":4294967296,"i":1}}}"#.to_owned(),
        r#"{"a":{"$timestamp":{"t":-1,"i":1}}}"#.to_owned(),
        r#"{"a":{"$timestamp":1}}"#.to_owned(),
        // $binary: subType empty, too long, not hexadecimal; data not base64.
        r#"{"a":{"$binary":{"base64":"","subType":""}}}"#.to_owned(),
        r#"{"a":{"$binary":{"base64":"","subType":"001"}}}"#.to_owned(),
        r#"{"a":{"$binary":{"base64":"","subType":"zz"}}}"#.to_owned(),
        r#"{"a":{"$binary":{"base64":"abc","subType":"00"}}}"#.to_owned(),
        // $uuid and $oid that are not hexadecimal or have another length.
        r#"{"a":{"$uuid":"zzzzzzzz-44b3-4c69-90e8-e7d1dfc035d4"}}"#.to_owned(),
        r#"{"a":{"$oid":"abc"}}"#.to_owned(),
        r#"{"a":{"$oid":"zz e1fc72e0c917e9c471416"}}"#.to_owned(),
        r#"{"a":{"$oid":"56e1fc72e0c917e9c47141"}}"#.to_owned(),
        // Integers without digits or out of range; doubles that are not numbers.
        r#"{"a":{"$numberInt":""}}"#.to_owned(),
        r#"{"a":{"$numberInt":"-"}}"#.to_owned(),
        r#"{"a":{"$numberInt":"12a"}}"#.to_owned(),
        r#"{"a":{"$numberInt":"99999999999"}}"#.to_owned(),
        r#"{"a":{"$numberLong":"99999999999999999999"}}"#.to_owned(),
        r#"{"a":{"$numberDouble":"1.2.3"}}"#.to_owned(),
        r#"{"a":{"$numberDouble":"inf"}}"#.to_owned(),
        r#"{"a":{"$numberDecimal":"x"}}"#.to_owned(),
        // $minKey, $maxKey and $undefined with another value; $symbol not a string.
        r#"{"a":{"$maxKey":2}}"#.to_owned(),
        r#"{"a":{"$undefined":false}}"#.to_owned(),
        r#"{"a":{"$symbol":1}}"#.to_owned(),
        r#"{"a":{"$regularExpression":1}}"#.to_owned(),
    ] {
        refused(&text);
    }
}

#[test]
#[expect(
    clippy::cast_precision_loss,
    reason = "the expected double is the nearest one to u64::MAX"
)]
fn plain_numbers_past_i64_become_doubles() {
    let big = "123456789012345678901234567890";
    for (text, want) in [
        (r#"{"a":18446744073709551615}"#.to_owned(), u64::MAX as f64),
        (format!(r#"{{"a":{big}}}"#), big.parse::<f64>().expect("parse succeeds")),
    ] {
        let bytes =
            extjson::parse(&text).expect("parse succeeds").to_bytes().expect("to_bytes succeeds");
        assert_eq!(
            RawDoc::from_bytes(&bytes).expect("from_bytes succeeds").get("a"),
            Some(RawValue::Double(want)),
            "{text}"
        );
    }
    // Inside the ranges: int32 and int64.
    let bytes = extjson::parse(r#"{"a":5,"b":5000000000}"#)
        .expect("parse succeeds")
        .to_bytes()
        .expect("to_bytes succeeds");
    let raw = RawDoc::from_bytes(&bytes).expect("from_bytes succeeds");
    assert_eq!(
        (raw.get("a"), raw.get("b")),
        (Some(RawValue::Int32(5)), Some(RawValue::Int64(5_000_000_000)))
    );
}
