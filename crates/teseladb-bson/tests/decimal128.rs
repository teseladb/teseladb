//! Decimal128: exact comparison of values and their representations, and the parts and limits of a value. The
//! decimals of the specification's corpus run in the compatibility repository (D-170).

use std::cmp::Ordering;

use teseladb_bson::{Decimal, Decimal128, MAX_EXPONENT, MIN_EXPONENT};

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
