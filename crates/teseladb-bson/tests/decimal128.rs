//! Decimal128 against the corpus's decimal files: every valid value
//! prints its canonical text and reads back from it (and from its degenerate forms) to the same
//! bytes, unless the corpus marks it lossy; every parse error is refused. Then exact comparison.

mod common;

use std::cmp::Ordering;

use teseladb_bson::{Decimal, Decimal128, MAX_EXPONENT, MIN_EXPONENT};
use teseladb_json::Value as Json;

/// The decimal of a corpus document `{"d": decimal}` and the text of its Extended JSON.
fn decimal_of(bytes: &[u8]) -> Decimal128 {
    // Length, type 0x13, "d\0", then the 16 bytes.
    assert_eq!(&bytes[4..7], &[0x13, b'd', 0]);
    Decimal128::from_le_bytes(bytes[7..23].try_into().expect("try_into succeeds"))
}

fn text_of(extjson: &str) -> String {
    let j = teseladb_json::parse(extjson.as_bytes()).expect("Extended JSON");
    j.get("d")
        .and_then(|d| d.get("$numberDecimal"))
        .and_then(Json::as_str)
        .expect("$numberDecimal")
        .to_owned()
}

fn s<'a>(case: &'a Json, key: &str) -> Option<&'a str> {
    case.get(key).and_then(Json::as_str)
}

#[test]
fn the_corpus_decimals_print_and_parse() {
    let (mut valid, mut degenerate, mut errors) = (0, 0, 0);
    for f in common::corpus().iter().filter(|f| f.name.starts_with("decimal128-")) {
        for case in f.cases("valid") {
            let what = format!("{}: {}", f.name, s(&case, "description").expect("s succeeds"));
            let bytes = common::hex(s(&case, "canonical_bson").expect("s succeeds"));
            let d = decimal_of(&bytes);
            let text = text_of(s(&case, "canonical_extjson").expect("s succeeds"));
            assert_eq!(d.to_string(), text, "{what}");
            let lossy = case.get("lossy").and_then(Json::as_bool).unwrap_or(false);
            if !lossy {
                assert_eq!(text.parse::<Decimal128>(), Ok(d), "{what}");
            }
            if let Some(dj) = s(&case, "degenerate_extjson") {
                let parsed =
                    text_of(dj).parse::<Decimal128>().unwrap_or_else(|e| panic!("{what}: {e}"));
                if !lossy {
                    assert_eq!(parsed, d, "{what} (degenerate)");
                }
                degenerate += 1;
            }
            valid += 1;
        }
        for case in f.cases("parseErrors") {
            let text = s(&case, "string").expect("s succeeds");
            assert!(text.parse::<Decimal128>().is_err(), "{}: {text:?} was accepted", f.name);
            errors += 1;
        }
    }
    assert_eq!((valid, degenerate, errors), (605, 319, 131));
}

fn d(text: &str) -> Decimal128 {
    text.parse().expect("parse succeeds")
}

#[test]
fn values_compare_exactly() {
    let ordered = [
        "NaN",
        "-Infinity",
        "-1E+6111",
        "-10",
        "-1.5",
        "-0.0001",
        "0",
        "1E-6176",
        "0.5",
        "1",
        "1.000001",
        "99",
        "1E+2",
        "9.999999999999999999999999999999999E+6144",
        "Infinity",
    ];
    for (i, a) in ordered.iter().enumerate() {
        for (j, b) in ordered.iter().enumerate() {
            assert_eq!(d(a).cmp_value(d(b)), i.cmp(&j), "{a} against {b}");
        }
    }
    // Same value, other representation.
    for (a, b) in
        [("1.0", "1.00"), ("100", "1E+2"), ("-0", "0E+3"), ("NaN", "-NaN"), ("0.10", "1E-1")]
    {
        assert_eq!(d(a).cmp_value(d(b)), Ordering::Equal, "{a} = {b}");
    }
    assert_eq!(d("-2").cmp_value(d("-1.99")), Ordering::Less);
}

#[test]
fn parts_and_limits() {
    assert_eq!(
        d("-12.5").classify(),
        Decimal::Finite { negative: true, coefficient: 125, exponent: -1 }
    );
    assert_eq!(d("Inf").classify(), Decimal::Infinity { negative: false });
    assert_eq!(d("-infinity").classify(), Decimal::Infinity { negative: true });
    assert_eq!(Decimal128::nan().classify(), Decimal::NaN);
    let past = Decimal128::from_le_bytes(((6176u128 << 113) | 10u128.pow(34)).to_le_bytes());
    assert_eq!(past.classify(), Decimal::Finite { negative: false, coefficient: 0, exponent: 0 });
    let max =
        Decimal128::from_parts(false, 9_999_999_999_999_999_999_999_999_999_999_999, MAX_EXPONENT)
            .expect("from_parts succeeds");
    assert_eq!(max.to_string(), "9.999999999999999999999999999999999E+6144");
    assert!(Decimal128::from_parts(false, 10u128.pow(34), 0).is_none());
    assert!(Decimal128::from_parts(false, 1, MAX_EXPONENT + 1).is_none());
    assert!(Decimal128::from_parts(false, 1, MIN_EXPONENT - 1).is_none());
    // Zero takes the closest exponent in range; a huge exponent saturates and is refused.
    assert_eq!(d("0E+99999999999999999999").to_string(), "0E+6111");
    assert_eq!(d("0E-99999999999999999999").to_string(), "0E-6176");
    let e = "1E+99999999999999999999".parse::<Decimal128>().expect_err("parse fails");
    assert_eq!(e.to_string(), "not a decimal128: exponent out of range");
    assert!(
        "1E+"
            .parse::<Decimal128>()
            .expect_err("parse fails")
            .to_string()
            .contains("exponent needs digits")
    );
}
