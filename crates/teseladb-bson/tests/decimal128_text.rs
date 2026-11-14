//! The text of a decimal128 (FR-015): how each kind of value prints, and how text that needs
//! rounding or that is not a number is refused.

use teseladb_bson::{Decimal, Decimal128};

fn text(s: &str) -> String {
    s.parse::<Decimal128>().unwrap_or_else(|e| panic!("{s}: {e:?}")).to_string()
}

#[test]
fn each_kind_of_value_prints_in_its_canonical_text() {
    assert_eq!(Decimal128::infinity(false).to_string(), "Infinity");
    assert_eq!(Decimal128::infinity(true).to_string(), "-Infinity");
    assert_eq!(Decimal128::nan().to_string(), "NaN");
    assert_eq!(text("15"), "15");
    assert_eq!(text("-15"), "-15");
    assert_eq!(text("1.5"), "1.5");
    assert_eq!(text("0.0015"), "0.0015");
    assert_eq!(text("1.5E+10"), "1.5E+10");
    assert_eq!(text("1E-10"), "1E-10");
    assert_eq!(text("-Inf"), "-Infinity");
    assert_eq!(text("nan"), "NaN");
}

#[test]
fn a_coefficient_that_needs_more_than_113_bits_is_worth_zero() {
    // The two high bits of the combination at 11, and neither NaN nor an infinity.
    let v: u128 = 0b11 << 125;
    let d = Decimal128::from_le_bytes(v.to_le_bytes());
    match d.classify() {
        Decimal::Finite { negative, coefficient, .. } => {
            assert_eq!((negative, coefficient), (false, 0));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn text_that_needs_rounding_or_is_not_a_number_is_refused() {
    for bad in ["", ".", "1.x", "x1", "1e", "1e+", "1ex", "12345678901234567890123456789012345"] {
        assert!(bad.parse::<Decimal128>().is_err(), "{bad:?}");
    }
    // Trailing zeros past 34 digits go into the exponent.
    let wide = format!("1{}", "0".repeat(40));
    assert_eq!(text(&wide), "1.000000000000000000000000000000000E+40");
    // An exponent too small loses trailing zeros until it fits: 10E-6176.
    assert_eq!(text("1000E-6178"), "1.0E-6175");
    // An exponent too large gains zeros until it fits.
    assert_eq!(text("1E+6112"), "1.0E+6112");
}
