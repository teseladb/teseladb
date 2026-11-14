//! The decimal helpers at their edges: conversions to and from decimals and integers, remainders,
//! and the limits of the transcendental functions (010 T007).

use teseladb_bson::{Decimal128, Document, RawDoc, Value};
use teseladb_mql::decimal_math::{exp, from_f64_exact, log, pow};
use teseladb_mql::decimal_trig::{Trig, atan2, trig};
use teseladb_mql::{
    DecOp, dec_binary, decimal_exact, decimal_saturating, decimal_to_f64, exact_int64,
    to_decimal128,
};

fn d(s: &str) -> Decimal128 {
    s.parse().unwrap_or_else(|e| panic!("{s}: {e:?}"))
}

fn text(v: Option<Decimal128>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "None".into())
}

fn op(o: DecOp, a: &str, b: &str) -> String {
    text(dec_binary(o, &Value::Decimal128(d(a)), &Value::Decimal128(d(b))))
}

#[test]
fn conversions_between_numbers_and_decimals() {
    assert_eq!(text(to_decimal128(&Value::Double(0.1))), "0.100000000000000");
    assert_eq!(text(to_decimal128(&Value::Int32(7))), "7");
    assert_eq!(text(to_decimal128(&Value::String("x".into()))), "None");
    assert_eq!(decimal_to_f64(d("1.5")), 1.5);
    assert!(decimal_to_f64(d("NaN")).is_nan());
    assert_eq!(from_f64_exact(-0.5).to_string(), "-0.5");
    assert_eq!(from_f64_exact(123.0).to_string(), "123");
}

#[test]
fn remainders_and_differences() {
    assert_eq!(op(DecOp::Rem, "7", "2"), "1");
    assert_eq!(op(DecOp::Rem, "-7.5", "2"), "-1.5");
    assert_eq!(op(DecOp::Rem, "1", "0"), "NaN");
    assert_eq!(op(DecOp::Rem, "0", "0"), "NaN");
    assert_eq!(op(DecOp::Rem, "1E+5", "3"), "1");
    assert_eq!(op(DecOp::Rem, "0.001", "1E+3"), "0.001");
    assert_eq!(op(DecOp::Sub, "1", "Infinity"), "-Infinity");
    assert_eq!(op(DecOp::Sub, "NaN", "1"), "NaN");
    assert_eq!(op(DecOp::Div, "1", "0"), "Infinity");
    assert_eq!(op(DecOp::Div, "-1", "0"), "-Infinity");
    assert_eq!(op(DecOp::Div, "0", "0"), "NaN");
}

#[test]
fn decimals_as_64_bit_integers() {
    assert_eq!(decimal_exact(d("2.00")), Some(2));
    assert_eq!(decimal_exact(d("-1E+1")), Some(-10));
    assert_eq!(decimal_exact(d("2.5")), None);
    assert_eq!(decimal_exact(d("NaN")), None);
    assert_eq!(decimal_exact(d("1E+30")), None);
    assert_eq!(decimal_saturating(d("2.5")), 2);
    assert_eq!(decimal_saturating(d("3.5")), 4);
    assert_eq!(decimal_saturating(d("-2.6")), -3);
    assert_eq!(decimal_saturating(d("NaN")), 0);
    assert_eq!(decimal_saturating(d("Infinity")), i64::MAX);
    assert_eq!(decimal_saturating(d("-Infinity")), i64::MIN);
    assert_eq!(decimal_saturating(d("1E+40")), i64::MAX);
    assert_eq!(decimal_saturating(d("-1E+40")), i64::MIN);
    assert_eq!(decimal_saturating(d("1E-40")), 0);
    let holder = Document::new()
        .with("a", Value::Double(2.5))
        .with("b", Value::Double(f64::NAN))
        .with("c", Value::Double(1e30))
        .with("d", Value::Double(4.0))
        .with("e", Value::Decimal128(d("1E+30")))
        .with("f", "x");
    let bytes = holder.to_bytes().expect("small");
    let raw = RawDoc::from_bytes(&bytes).expect("a document");
    let get = |k: &str| exact_int64(raw.get(k).expect("a field"), k);
    assert!(get("a").is_err());
    assert!(get("b").is_err());
    assert!(get("c").is_err());
    assert_eq!(get("d"), Ok(4));
    assert!(get("e").is_err());
    assert!(get("f").is_err());
}

#[test]
fn logarithms_and_powers_at_their_edges() {
    assert_eq!(text(log(d("NaN"), d("2"))), "NaN");
    assert_eq!(log(d("-1"), d("2")), None);
    assert_eq!(log(d("8"), d("-2")), None);
    assert_eq!(log(d("0"), d("2")), None);
    assert_eq!(text(log(d("10"), d("100"))), "0.5");
    assert_eq!(text(log(d("100"), d("3"))), "4.191806548578769208593135044042803");
    assert_eq!(exp(d("5")).to_string(), "148.4131591025766034211155800405523");
    assert_eq!(exp(d("-1E+10")).to_string(), "0E-6176");
    assert_eq!(pow(d("9"), d("0.5")).to_string(), "3.000000000000000000000000000000000");
}

#[test]
fn transcendental_limits() {
    let s = |f: Trig, x: &str| text(trig(f, d(x)));
    assert_eq!(s(Trig::Atan, "-2"), "-1.107148717794090503017065460178537");
    assert_eq!(s(Trig::Atan, "Infinity"), "1.570796326794896619231321691639751");
    assert_eq!(s(Trig::Atan, "-Infinity"), "-1.570796326794896619231321691639751");
    assert_eq!(s(Trig::Tanh, "Infinity"), "1");
    assert_eq!(s(Trig::Tanh, "-Infinity"), "-1");
    assert_eq!(s(Trig::Sigmoid, "Infinity"), "1");
    assert_eq!(s(Trig::Cosh, "1E-45"), "1.000000000000000000000000000000000");
    assert_eq!(s(Trig::Atan, "-1E+70"), "-1.570796326794896619231321691639751");
    assert_eq!(s(Trig::Tanh, "1E+70"), "1");
    assert_eq!(s(Trig::Asinh, "-1E+70"), "-161.8741036901431431906766339493637");
    assert_eq!(s(Trig::Sigmoid, "30000"), "1");
    assert_eq!(s(Trig::Sigmoid, "-30000"), "0E-6176");
    assert_eq!(s(Trig::Sigmoid, "1000"), "1");
    assert_eq!(s(Trig::Sigmoid, "1E-35"), "0.5");
    assert_eq!(s(Trig::Tanh, "55"), "1");
    assert_eq!(s(Trig::Tanh, "-55"), "-1");
    assert_eq!(s(Trig::Cos, "-1"), "0.5403023058681397174009366074429766");
    let a = |y: &str, x: &str| text(atan2(d(y), d(x)));
    assert_eq!(a("-1", "0"), "-1.570796326794896619231321691639751");
    assert_eq!(a("1", "-1E+40"), "3.141592653589793238462643383279503");
    assert_eq!(a("-1E+70", "1"), "-1.570796326794896619231321691639751");
    assert_eq!(a("1E-50", "-1"), "3.141592653589793238462643383279503");
    assert_eq!(a("1E-100", "1"), "1E-100");
    assert_eq!(a("1", "1E+50"), "1E-50");
    assert_eq!(a("3E-60", "-7E-20"), "3.141592653589793238462643383279503");
    assert_eq!(a("-3", "-7E+20"), "-3.141592653589793238458357668993789");
}
