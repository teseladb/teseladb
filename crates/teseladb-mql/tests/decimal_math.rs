//! The decimal functions of 010's expressions at their edges (015 and 010 T007): NaN and the
//! infinities, zero and negative arguments, and the sign of a power of a negative base.

use teseladb_mql::decimal_math::{exp, ln, log, log10, pow, sqrt};
use teseladb_bson::Decimal128;

fn d(s: &str) -> Decimal128 {
    s.parse().unwrap_or_else(|e| panic!("{s}: {e:?}"))
}

fn text(v: Option<Decimal128>) -> Option<String> {
    v.map(|x| x.to_string())
}

#[test]
fn special_values_pass_through_or_fail() {
    assert_eq!(text(sqrt(d("NaN"))), Some("NaN".into()));
    assert_eq!(text(sqrt(d("Infinity"))), Some("Infinity".into()));
    assert_eq!(text(sqrt(d("-Infinity"))), Some("NaN".into()));
    assert_eq!(sqrt(d("-4")), None);
    assert_eq!(text(sqrt(d("0E-7"))), Some("0.0000".into()));
    assert_eq!(exp(d("-Infinity")).to_string(), "0");
    assert_eq!(exp(d("Infinity")).to_string(), "Infinity");
    assert_eq!(exp(d("-1E+10")).to_string(), "0E-6176");
    assert_eq!(ln(d("0")), None);
    assert_eq!(ln(d("-1")), None);
    assert_eq!(ln(d("-Infinity")), None);
    assert_eq!(text(ln(d("Infinity"))), Some("Infinity".into()));
    assert_eq!(text(ln(d("1.000"))), Some("0".into()));
    assert_eq!(log10(d("-5")), None);
    assert_eq!(log10(d("-Infinity")), None);
    assert_eq!(text(log10(d("NaN"))), Some("NaN".into()));
    assert_eq!(log(d("0"), d("2")), None);
}

#[test]
fn powers_keep_34_digits_and_the_sign_of_odd_exponents() {
    assert_eq!(pow(d("NaN"), d("2")).to_string(), "NaN");
    assert_eq!(pow(d("0"), d("2")).to_string(), "0");
    assert_eq!(pow(d("0"), d("-2")).to_string(), "Infinity");
    assert_eq!(pow(d("-2"), d("2")).to_string(), "4.000000000000000000000000000000000");
    assert_eq!(pow(d("-2"), d("3.0")).to_string(), "-8.000000000000000000000000000000000");
    assert_eq!(pow(d("-2"), d("20")).to_string(), "1048576.000000000000000000000000000");
    assert_eq!(pow(d("-2"), d("2.5")).to_string(), "NaN");
    assert_eq!(pow(d("4"), d("0.5")).to_string(), "2.000000000000000000000000000000000");
}

#[test]
fn doubles_enter_decimals_with_their_exact_value() {
    use teseladb_mql::decimal_math::from_f64_exact;
    assert_eq!(from_f64_exact(1.0).to_string(), "1");
    assert_eq!(from_f64_exact(0.1).to_string(), "0.1000000000000000055511151231257827");
    assert_eq!(from_f64_exact(-2.5).to_string(), "-2.5");
    assert_eq!(from_f64_exact(f64::NAN).to_string(), "NaN");
    assert_eq!(from_f64_exact(f64::NEG_INFINITY).to_string(), "-Infinity");
    assert_eq!(from_f64_exact(1e300).to_string(), "1.00000000000000005250476025520442E+300");
}

#[test]
fn exact_and_inexact_roots_and_logarithms() {
    assert_eq!(text(sqrt(d("100"))), Some("10".into()));
    assert_eq!(text(sqrt(d("1E+4"))), Some("1E+2".into()));
    assert_eq!(text(sqrt(d("1E-21"))), Some("3.162277660168379331998893544432719E-11".into()));
    assert_eq!(text(sqrt(d("2.25"))), Some("1.5".into()));
    assert_eq!(exp(d("1E+10")).to_string(), "Infinity");
    assert_eq!(exp(d("0E+3")).to_string(), "1");
    assert_eq!(text(ln(d("10.0"))).map(|s| s.starts_with("2.302585")), Some(true));
    assert_eq!(text(ln(d("1.0"))), Some("0".into()));
    assert_eq!(text(log10(d("0.0010"))), Some("-3".into()));
    assert_eq!(text(log10(d("20"))).map(|s| s.starts_with("1.301029995663981")), Some(true));
    assert_eq!(log10(d("0")), None);
    assert_eq!(text(log(d("8"), d("2"))), Some("3.000000000000000000000000000000000".into()));
}

#[test]
fn powers_with_integral_decimal_exponents() {
    assert_eq!(pow(d("2"), d("1E+1")).to_string(), "1024.000000000000000000000000000000");
    assert_eq!(pow(d("-2"), d("1E+1")).to_string(), "1024.000000000000000000000000000000");
    assert_eq!(pow(d("-2"), d("5")).to_string(), "-32.00000000000000000000000000000000");
    assert_eq!(pow(d("-2"), d("0.5")).to_string(), "NaN");
    assert_eq!(pow(d("4"), d("-0.5")).to_string(), "0.5000000000000000000000000000000000");
    assert_eq!(pow(d("Infinity"), d("2")).to_string(), "NaN");
}
