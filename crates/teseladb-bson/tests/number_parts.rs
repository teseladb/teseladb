//! A number's exact decimal parts: the digits an order-keeping key encoding writes, the same
//! for the same value whatever its type, and a double's whole exact expansion.

use teseladb_bson::{Decimal128, NumberParts, RawValue, number_parts};

fn parts(v: RawValue<'_>) -> NumberParts {
    number_parts(&v).expect("a number")
}

fn finite(negative: bool, digits: &str, exponent: i32) -> NumberParts {
    NumberParts::Finite { negative, digits: digits.bytes().map(|b| b - b'0').collect(), exponent }
}

fn dec(s: &str) -> RawValue<'static> {
    RawValue::Decimal128(s.parse::<Decimal128>().expect("parse succeeds"))
}

#[test]
fn the_same_value_gives_the_same_parts() {
    // 120 is 0.12 × 10^3 as int32, int64, double and decimal (with or without its zeros).
    let expected = finite(false, "12", 3);
    for v in [
        RawValue::Int32(120),
        RawValue::Int64(120),
        RawValue::Double(120.0),
        dec("120"),
        dec("1.20E+2"),
    ] {
        assert_eq!(parts(v), expected, "{v:?}");
    }
    assert_eq!(parts(RawValue::Int64(-5)), finite(true, "5", 1));
    assert_eq!(parts(dec("-0.0015")), finite(true, "15", -2));
    assert_eq!(parts(RawValue::Int64(i64::MIN)), finite(true, "9223372036854775808", 19));
    for zero in [RawValue::Int32(0), RawValue::Double(-0.0), dec("-0E+7")] {
        assert_eq!(parts(zero), NumberParts::Zero);
    }
    assert_eq!(parts(RawValue::Double(f64::NAN)), NumberParts::NaN);
    assert_eq!(parts(dec("NaN")), NumberParts::NaN);
    assert_eq!(
        parts(RawValue::Double(f64::NEG_INFINITY)),
        NumberParts::Infinity { negative: true }
    );
    assert_eq!(parts(dec("Infinity")), NumberParts::Infinity { negative: false });
    assert_eq!(number_parts(&RawValue::Null), None);
}

#[test]
fn a_double_is_worth_its_whole_binary_fraction() {
    // The double nearest 0.1, all its digits.
    assert_eq!(
        parts(RawValue::Double(0.1)),
        finite(false, "1000000000000000055511151231257827021181583404541015625", 0)
    );
    // 2^-1074, the smallest subnormal: 751 significant digits, 0.494… × 10^-323.
    let NumberParts::Finite { digits, exponent, .. } = parts(RawValue::Double(f64::from_bits(1)))
    else {
        panic!()
    };
    assert_eq!((digits.len(), exponent), (751, -323));
    assert_eq!(&digits[..10], &[4, 9, 4, 0, 6, 5, 6, 4, 5, 8]);
    // 2^1023 × (2 − 2^−52), the largest double: 309 digits.
    let NumberParts::Finite { digits, exponent, .. } = parts(RawValue::Double(f64::MAX)) else {
        panic!()
    };
    assert_eq!((digits.len(), exponent), (309, 309));
    assert_eq!(&digits[..6], &[1, 7, 9, 7, 6, 9]);
}
