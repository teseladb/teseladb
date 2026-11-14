//! The trigonometric and hyperbolic functions of 010's expressions on decimals (`$sin` … `$tanh`,
//! `$atan2`, `$degreesToRadians`, `$radiansToDegrees`, `$sigmoid`), computed in fixed point with
//! [`PRECISION`] digits and rounded to 34 digits half to even, the arguments of `$sin`, `$cos` and
//! `$tan` reduced exactly with π to 6 750 digits (enough for the largest decimal).
//!
//! The reference server's decimal library (measured on a reference router 9.0.4) computes through a
//! binary format: its last digit is sometimes another (DV-062), and a result that rounds to 1, −1
//! or 0.5 without being exact comes out in that short form (`cos(1E-20)` is `1`, while `cos(0)` is
//! `1.000000000000000000000000000000000`). TeselaDB gives the correctly rounded value with
//! the reference server's form: the short form in those cases, the reference server's exponents for
//! a zero argument, `0E-6176` for a result that underflows, and the exact quotient `y/x` for
//! `$atan2` of a ratio below 10^−36.
//!
//! An argument outside a function's domain is the caller's error: these functions give `None`.

use std::cmp::Ordering;
use std::sync::OnceLock;

use teseladb_bson::{Decimal, Decimal128, Value};

use crate::arith::{DecOp, dec_binary};
use crate::decimal_math::{
    Big, Fixed, PRECISION, exp_fixed, integer_decimal, ln_fixed, ln_of, normalized, one, parts,
    power_of_ten, to_decimal,
};

/// The largest exponent (of a number written with one digit before the point) a fixed-point
/// value holds; past it the argument is reduced or the result is its limit.
const MAX_FIXED_EXPONENT: i64 = 60;
/// Below 10^−40 an odd function's value is its argument to every digit kept, and an even one's
/// is 1.
const TINY_EXPONENT: i64 = -41;
/// Digits of the reciprocal of 2π used to reduce arguments: past the largest exponent (6 111)
/// by more than 34 + [`PRECISION`] digits.
const REDUCTION_DIGITS: usize = 6_750;
/// From this exponent of `y/x` down the reference server's `$atan2` gives the quotient itself.
const ATAN2_QUOTIENT_EXPONENT: i64 = -37;
/// Below this exponent `$sigmoid` is computed by its series, whose third term is past every
/// fixed-point digit.
const SIGMOID_SERIES_EXPONENT: i64 = -20;
/// The smallest exponent of a decimal.
const MIN_EXPONENT: i32 = -6176;

fn fixed_one() -> Fixed {
    Fixed::new(false, one())
}

fn small(n: u32) -> Fixed {
    Fixed::new(false, Big::from_u128(u128::from(n)).shl10(PRECISION))
}

/// `d` in fixed point, with its sign, coefficient, exponent and normalized exponent `q`
/// (`d = m × 10^q`, `1 ≤ m < 10`); `None` for NaN or an infinity.
struct Arg {
    neg: bool,
    c: u128,
    e: i32,
    q: i64,
}

fn arg_of(d: Decimal128) -> Option<Arg> {
    let (neg, c, e) = parts(d)?;
    let q = if c == 0 { i64::MIN } else { normalized(c, e).1 };
    Some(Arg { neg, c, e, q })
}

/// `a` in fixed point, for `a.q ≤` [`MAX_FIXED_EXPONENT`].
fn fixed_of(a: &Arg) -> Fixed {
    if a.c == 0 {
        return Fixed::new(false, Big::zero());
    }
    let (m, q) = normalized(a.c, a.e);
    let mut x = m;
    for _ in 0..q.max(0) {
        x = Fixed::new(false, x.mag.mul_small(10));
    }
    for _ in 0..(-q).max(0) {
        x = x.div_small(10);
    }
    Fixed::new(a.neg, x.mag)
}

/// `atan x` for |x| ≤ 1: halvings bring it near 0, then the series.
fn atan_small(x: &Fixed) -> Fixed {
    let mut y = x.clone();
    let mut doublings = 0u32;
    let tenth = Big::from_u128(1).shl10(PRECISION - 1);
    while y.mag.cmp(&tenth) == Ordering::Greater {
        // atan y = 2·atan(y / (1 + √(1 + y²))).
        let root = fixed_one().add(&y.mul(&y)).sqrt();
        y = y.div(&fixed_one().add(&root));
        doublings += 1;
    }
    let y2 = y.mul(&y);
    let (mut term, mut sum, mut k, mut negative) = (y.clone(), y.clone(), 1u32, true);
    while !term.mag.is_zero() {
        term = term.mul(&y2);
        k += 2;
        let t = term.div_small(k);
        sum = sum.add(&if negative { t.neg() } else { t });
        negative = !negative;
    }
    for _ in 0..doublings {
        sum = sum.add(&sum);
    }
    sum
}

/// π × 10^`n` as an integer, by Machin's formula `16·atan(1/5) − 4·atan(1/239)`.
fn pi_digits(n: usize) -> Big {
    let guard = n + 10;
    let atan_inv = |k: u32| -> Big {
        let k2 = k * k;
        let mut term = Big::from_u128(1).shl10(guard).div_u32(k);
        let mut sum = term.clone();
        let mut i = 1u32;
        loop {
            term = term.div_u32(k2);
            if term.is_zero() {
                return sum;
            }
            let t = term.div_u32(2 * i + 1);
            sum = if i % 2 == 1 { sum.sub(&t) } else { sum.add(&t) };
            i += 1;
        }
    };
    let pi = atan_inv(5).mul_small(16).sub(&atan_inv(239).mul_small(4));
    pi.divmod(&Big::from_u128(10_000_000_000)).0
}

/// π in fixed point.
fn pi() -> Fixed {
    static PI: OnceLock<Big> = OnceLock::new();
    Fixed::new(false, PI.get_or_init(|| pi_digits(PRECISION)).clone())
}

/// `⌊10^REDUCTION_DIGITS / 2π⌋`.
fn inverse_two_pi() -> &'static Big {
    static INVERSE: OnceLock<Big> = OnceLock::new();
    INVERSE.get_or_init(|| {
        let extra = 20;
        let two_pi = pi_digits(REDUCTION_DIGITS + extra).mul_small(2);
        Big::from_u128(1).shl10(2 * REDUCTION_DIGITS + extra).divmod(&two_pi).0
    })
}

/// `|a| mod 2π` in fixed point, in [0, 2π): `|a|/2π`'s fractional part from the reciprocal of 2π
/// to [`REDUCTION_DIGITS`] digits, times 2π.
fn reduce(a: &Arg) -> Fixed {
    let n = Big::from_u128(a.c).mul(inverse_two_pi());
    // |a| / 2π = n / 10^k.
    let k = REDUCTION_DIGITS as i64 - i64::from(a.e);
    let digits = n.digits();
    let take = usize::try_from(k).unwrap_or(0).min(digits.len());
    let fraction = &digits[digits.len() - take..];
    // The fraction's first PRECISION digits after the point.
    let mut scaled: Vec<u8> = vec![0; usize::try_from(k).unwrap_or(0) - take];
    scaled.extend_from_slice(fraction);
    scaled.resize(scaled.len().max(PRECISION), 0);
    scaled.truncate(PRECISION);
    let frac = Fixed::new(false, Big::from_digits(&scaled));
    let two_pi = Fixed::new(false, pi().mag.mul_small(2));
    frac.mul(&two_pi)
}

/// `(sin x, cos x)` of `a` by their series after reducing it.
fn sin_cos(a: &Arg) -> (Fixed, Fixed) {
    let mut r = if a.q >= 0 { reduce(a) } else { Fixed::new(false, fixed_of(a).mag) };
    let p = pi();
    if r.mag.cmp(&p.mag) == Ordering::Greater {
        r = r.add(&Fixed::new(true, p.mag.mul_small(2)));
    }
    let r2 = r.mul(&r);
    let (mut s_term, mut s) = (r.clone(), r.clone());
    let (mut c_term, mut c) = (fixed_one(), fixed_one());
    let mut k = 0u32;
    while !s_term.mag.is_zero() || !c_term.mag.is_zero() {
        c_term = c_term.mul(&r2).div_small((2 * k + 1) * (2 * k + 2)).neg();
        s_term = s_term.mul(&r2).div_small((2 * k + 2) * (2 * k + 3)).neg();
        c = c.add(&c_term);
        s = s.add(&s_term);
        k += 1;
    }
    (if a.neg { s.neg() } else { s }, c)
}

/// `atan x` for any `x` in fixed point.
fn atan_fixed(x: &Fixed) -> Fixed {
    if x.mag.cmp(&one()) != Ordering::Greater {
        return atan_small(x);
    }
    let half_pi = pi().div_small(2);
    let inverse = atan_small(&fixed_one().div(&Fixed::new(false, x.mag.clone())));
    let r = half_pi.add(&inverse.neg());
    if x.neg { r.neg() } else { r }
}

/// A fixed-point result as a decimal, in the reference server's form: rounded to 34 digits, the
/// short `1`, `-1` or `0.5` when it rounds to one of them without being it.
fn result(v: &Fixed) -> Decimal128 {
    if v.mag.is_zero() {
        return integer_decimal(0);
    }
    let d = to_decimal(v, 0);
    // A result here is never exactly 1, −1 or 0.5 (those come from the zero arguments).
    let short = match d.to_string().as_str() {
        "1.000000000000000000000000000000000" => Some("1"),
        "-1.000000000000000000000000000000000" => Some("-1"),
        "0.5000000000000000000000000000000000" => Some("0.5"),
        _ => None,
    };
    short.and_then(|s| s.parse().ok()).unwrap_or(d)
}

/// `e^x` in fixed point for |x| ≤ 10^60 (as a value, which may lose its small digits).
fn exp_value(x: &Fixed) -> Fixed {
    let (m, n) = exp_fixed(x);
    let mut v = m;
    for _ in 0..n.max(0) {
        v = Fixed::new(false, v.mag.mul_small(10));
    }
    for _ in 0..(-n).max(0) {
        v = v.div_small(10);
    }
    v
}

/// A trigonometric or hyperbolic function on decimals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trig {
    /// `$sin`.
    Sin,
    /// `$cos`.
    Cos,
    /// `$tan`.
    Tan,
    /// `$asin`.
    Asin,
    /// `$acos`.
    Acos,
    /// `$atan`.
    Atan,
    /// `$sinh`.
    Sinh,
    /// `$cosh`.
    Cosh,
    /// `$tanh`.
    Tanh,
    /// `$asinh`.
    Asinh,
    /// `$acosh`.
    Acosh,
    /// `$atanh`.
    Atanh,
    /// `$degreesToRadians`.
    DegreesToRadians,
    /// `$radiansToDegrees`.
    RadiansToDegrees,
    /// `$sigmoid`.
    Sigmoid,
}

/// The value of `f` at an infinity.
fn at_infinity(f: Trig, neg: bool) -> Option<Decimal128> {
    match f {
        Trig::Sin | Trig::Cos | Trig::Tan | Trig::Asin | Trig::Acos | Trig::Atanh => None,
        Trig::Acosh if neg => None,
        Trig::Atan => {
            let h = pi().div_small(2);
            Some(result(&if neg { h.neg() } else { h }))
        }
        Trig::Tanh => Some(integer_decimal(if neg { -1 } else { 1 })),
        Trig::Cosh => Some(Decimal128::infinity(false)),
        Trig::Sigmoid if neg => zero(false, MIN_EXPONENT),
        Trig::Sigmoid => Some(integer_decimal(1)),
        _ => Some(Decimal128::infinity(neg)),
    }
}

fn zero(neg: bool, e: i32) -> Option<Decimal128> {
    Decimal128::from_parts(neg, 0, e.clamp(MIN_EXPONENT, 6111))
}

/// The value of `f` at a zero of exponent `e`, in the reference server's form.
fn at_zero(f: Trig, neg: bool, e: i32, d: Decimal128) -> Option<Decimal128> {
    match f {
        Trig::Sin | Trig::Atan | Trig::Tanh => Some(d),
        Trig::Tan | Trig::Asin | Trig::Sinh => zero(neg, e - 40),
        Trig::Asinh | Trig::Atanh => zero(neg, MIN_EXPONENT),
        Trig::DegreesToRadians => zero(neg, e - 35),
        Trig::RadiansToDegrees => zero(neg, e - 32),
        Trig::Cos | Trig::Cosh => Some(to_decimal(&fixed_one(), 0)),
        Trig::Acos => Some(result(&pi().div_small(2))),
        Trig::Sigmoid => "0.5".parse().ok(),
        Trig::Acosh => None,
    }
}

/// `c × 10^e × k` rounded to 34 digits, for a constant `k` in fixed point.
fn times(a: &Arg, k: &Fixed) -> Decimal128 {
    to_decimal(&Fixed::new(a.neg, k.mag.mul(&Big::from_u128(a.c))), i64::from(a.e))
}

/// `f(d)`, `None` outside its domain.
pub fn trig(f: Trig, d: Decimal128) -> Option<Decimal128> {
    match d.classify() {
        Decimal::NaN => return Some(d),
        Decimal::Infinity { negative } => return at_infinity(f, negative),
        Decimal::Finite { .. } => {}
    }
    let a = arg_of(d)?;
    if a.c == 0 {
        return at_zero(f, a.neg, a.e, d);
    }
    let deg = || pi().div_small(180);
    let rad = || small(180).div(&pi());
    match f {
        Trig::DegreesToRadians => return Some(times(&a, &deg())),
        Trig::RadiansToDegrees => return Some(times(&a, &rad())),
        _ => {}
    }
    // The reference server writes `cosh` of an argument up to 10^−40 as 1 with 34 digits.
    if f == Trig::Cosh && (a.q < -40 || a.q == -40 && power_of_ten(a.c, 0).is_some()) {
        return Some(to_decimal(&fixed_one(), 0));
    }
    if a.q <= TINY_EXPONENT {
        return tiny(f, &a);
    }
    if a.q > MAX_FIXED_EXPONENT && !matches!(f, Trig::Sin | Trig::Cos | Trig::Tan) {
        return huge(f, &a);
    }
    let one_f = fixed_one();
    let half_pi = || pi().div_small(2);
    Some(match f {
        Trig::Sin => result(&sin_cos(&a).0),
        Trig::Cos => result(&sin_cos(&a).1),
        Trig::Tan => {
            let (s, c) = sin_cos(&a);
            result(&s.div(&c))
        }
        _ => {
            let x = fixed_of(&a);
            let in_unit = x.mag.cmp(&one()) != Ordering::Greater;
            match f {
                Trig::Asin | Trig::Acos => {
                    if !in_unit {
                        return None;
                    }
                    let rest = one_f.add(&x.mul(&x).neg());
                    let asin = if rest.mag.is_zero() {
                        if x.neg { half_pi().neg() } else { half_pi() }
                    } else {
                        atan_fixed(&x.div(&rest.sqrt()))
                    };
                    if f == Trig::Asin {
                        result(&asin)
                    } else {
                        result(&half_pi().add(&asin.neg()))
                    }
                }
                Trig::Atan => result(&atan_fixed(&x)),
                Trig::Asinh => {
                    let m = Fixed::new(false, x.mag.clone());
                    let v = ln_fixed(&m.add(&m.mul(&m).add(&one_f).sqrt()));
                    result(&if x.neg { v.neg() } else { v })
                }
                Trig::Acosh => {
                    if x.neg || x.mag.cmp(&one()) == Ordering::Less {
                        return None;
                    }
                    result(&ln_fixed(&x.add(&x.mul(&x).add(&one_f.neg()).sqrt())))
                }
                Trig::Atanh => {
                    if !in_unit {
                        return None;
                    }
                    if x.mag.cmp(&one()) == Ordering::Equal {
                        return Some(Decimal128::infinity(x.neg));
                    }
                    let ratio = one_f.add(&x).div(&one_f.add(&x.neg()));
                    result(&ln_fixed(&ratio).div_small(2))
                }
                Trig::Sigmoid if a.q < SIGMOID_SERIES_EXPONENT => sigmoid_tiny(&a),
                Trig::Sigmoid => sigmoid(&x),
                _ => hyperbolic(f, &x),
            }
        }
    })
}

/// `f` of an argument below 10^−40.
fn tiny(f: Trig, a: &Arg) -> Option<Decimal128> {
    let itself =
        || to_decimal(&Fixed::new(a.neg, Big::from_u128(a.c).shl10(PRECISION)), i64::from(a.e));
    Some(match f {
        Trig::Acosh => return None,
        Trig::Cos => integer_decimal(1),
        Trig::Cosh => to_decimal(&fixed_one(), 0),
        Trig::Acos => result(&pi().div_small(2)),
        Trig::Sigmoid => "0.5".parse().ok()?,
        _ => itself(),
    })
}

/// `f` of an argument past 10^60, other than `sin`, `cos` and `tan`.
fn huge(f: Trig, a: &Arg) -> Option<Decimal128> {
    Some(match f {
        Trig::Asin | Trig::Acos | Trig::Atanh => return None,
        Trig::Acosh if a.neg => return None,
        Trig::Atan => {
            let h = pi().div_small(2);
            result(&if a.neg { h.neg() } else { h })
        }
        Trig::Tanh => integer_decimal(if a.neg { -1 } else { 1 }),
        Trig::Sigmoid if a.neg => return zero(false, MIN_EXPONENT),
        Trig::Sigmoid => integer_decimal(1),
        Trig::Sinh => Decimal128::infinity(a.neg),
        Trig::Cosh => Decimal128::infinity(false),
        _ => {
            // asinh and acosh: ln(2|x|), the rest below every digit kept.
            let two = Fixed::new(false, Big::from_u128(2).shl10(PRECISION));
            let v = ln_of(a.c, a.e).add(&ln_fixed(&two));
            result(&if a.neg { v.neg() } else { v })
        }
    })
}

/// `1 / (1 + e^−x)`, `0E-6176` when it underflows.
fn sigmoid(x: &Fixed) -> Decimal128 {
    // Past 20 000 the result is 1 or below the smallest decimal the reference server gives.
    if x.mag.cmp(&Big::from_u128(20_000).shl10(PRECISION)) == Ordering::Greater {
        return if x.neg {
            zero(false, MIN_EXPONENT).unwrap_or_else(Decimal128::nan)
        } else {
            integer_decimal(1)
        };
    }
    if x.neg {
        // e^x / (1 + e^x), computed with e^x's exponent kept.
        let (m, n) = exp_fixed(x);
        let e = exp_value(x);
        let denom = fixed_one().add(&e);
        let d = to_decimal(&m.div(&denom), n);
        if matches!(d.classify(), Decimal::Finite { coefficient: 0, .. }) {
            return zero(false, MIN_EXPONENT).unwrap_or(d);
        }
        return d;
    }
    let e = exp_value(&x.neg());
    if e.mag.is_zero() {
        // e^−x is below every digit kept: the value rounds to 1 without being 1.
        return integer_decimal(1);
    }
    result(&fixed_one().div(&fixed_one().add(&e)))
}

/// `sigmoid(c × 10^e)` for a tiny argument, as `1/2 + x/4 − x³/48` with integers, so that a value
/// a hair from halfway between two decimals rounds the right way (`sigmoid(1E-33)` is
/// `0.5000000000000000000000000000000002`, not `…0003`).
fn sigmoid_tiny(a: &Arg) -> Decimal128 {
    let scale = usize::try_from(-3 * i64::from(a.e) + 60).unwrap_or(0);
    let half = Big::from_u128(5).shl10(scale - 1);
    let quarter = Big::from_u128(a.c)
        .shl10(scale.saturating_add_signed(isize::try_from(a.e).unwrap_or(0)))
        .div_u32(4);
    let c = Big::from_u128(a.c);
    let cube = c.mul(&c).mul(&c).shl10(60).div_u32(48);
    let v = if a.neg { half.sub(&quarter).add(&cube) } else { half.add(&quarter).sub(&cube) };
    let d = to_decimal(&Fixed::new(false, v), PRECISION as i64 - scale as i64);
    // Never exactly a half: a rounding to it is written short.
    if d.to_string() == "0.5000000000000000000000000000000000" {
        return "0.5".parse().unwrap_or(d);
    }
    d
}

/// `sinh`, `cosh` or `tanh` of `x`.
fn hyperbolic(f: Trig, x: &Fixed) -> Decimal128 {
    let big = Big::from_u128(50).shl10(PRECISION);
    if x.mag.cmp(&big) == Ordering::Greater {
        // e^−|x| is below every digit kept: sinh and cosh are e^|x|/2, tanh is ±1.
        if f == Trig::Tanh {
            return integer_decimal(if x.neg { -1 } else { 1 });
        }
        let (m, n) = exp_fixed(&Fixed::new(false, x.mag.clone()));
        let half = m.div_small(2);
        let neg = f == Trig::Sinh && x.neg;
        return to_decimal(&Fixed::new(neg, half.mag), n);
    }
    let (ep, en) = (exp_value(x), exp_value(&x.neg()));
    match f {
        Trig::Sinh => result(&ep.add(&en.neg()).div_small(2)),
        Trig::Cosh => result(&ep.add(&en).div_small(2)),
        _ => result(&ep.add(&en.neg()).div(&ep.add(&en))),
    }
}

/// `atan2(y, x)` on decimals.
pub fn atan2(y: Decimal128, x: Decimal128) -> Option<Decimal128> {
    let (Some(ay), Some(ax)) = (arg_of(y), arg_of(x)) else {
        return Some(atan2_infinite(y, x));
    };
    let p = pi();
    if ay.c == 0 {
        return if ax.neg { Some(result(&p)) } else { zero(ay.neg, MIN_EXPONENT) };
    }
    if ax.c == 0 {
        let h = p.div_small(2);
        return Some(result(&if ay.neg { h.neg() } else { h }));
    }
    let quotient = dec_binary(DecOp::Div, &Value::Decimal128(y), &Value::Decimal128(x))?;
    let tiny_ratio = arg_of(quotient).is_some_and(|q| q.q <= ATAN2_QUOTIENT_EXPONENT);
    if tiny_ratio && !ax.neg {
        return Some(quotient);
    }
    // atan(y/x) with the quadrant, from the ratio in fixed point (or its limit).
    let ratio_q = ay.q - ax.q;
    let base = if ratio_q > MAX_FIXED_EXPONENT {
        let h = p.div_small(2);
        if ay.neg != ax.neg { h.neg() } else { h }
    } else if ratio_q <= TINY_EXPONENT {
        Fixed::new(false, Big::zero())
    } else {
        atan_fixed(&scaled_ratio(&ay, &ax))
    };
    let r = if !ax.neg {
        base
    } else if ay.neg {
        base.add(&p.neg())
    } else {
        base.add(&p)
    };
    Some(result(&r))
}

/// `y / x` in fixed point, computed from the coefficients so that large exponents cancel.
fn scaled_ratio(y: &Arg, x: &Arg) -> Fixed {
    let shift = i64::from(y.e) - i64::from(x.e);
    let num = Big::from_u128(y.c);
    let den = Big::from_u128(x.c);
    let mag = if shift >= 0 {
        num.shl10(PRECISION + shift as usize).divmod(&den).0
    } else {
        let s = (-shift) as usize;
        if s > PRECISION {
            num.shl10(PRECISION).divmod(&den.shl10(s)).0
        } else {
            num.shl10(PRECISION - s).divmod(&den).0
        }
    };
    Fixed::new(y.neg != x.neg, mag)
}

/// `atan2` with an infinity (or NaN) among its arguments, as IEEE 754 defines it.
fn atan2_infinite(y: Decimal128, x: Decimal128) -> Decimal128 {
    let side = |d: Decimal128| match d.classify() {
        Decimal::NaN => None,
        Decimal::Infinity { negative } => Some((negative, true)),
        Decimal::Finite { negative, .. } => Some((negative, false)),
    };
    let (Some((yn, yi)), Some((xn, xi))) = (side(y), side(x)) else { return Decimal128::nan() };
    let p = pi();
    let signed = |v: Fixed| result(&if yn { v.neg() } else { v });
    match (yi, xi, xn) {
        (true, false, _) => signed(p.div_small(2)),
        (true, true, false) => signed(p.div_small(4)),
        (true, true, true) => signed(Fixed::new(false, p.mag.mul_small(3)).div_small(4)),
        (false, true, false) => zero(yn, MIN_EXPONENT).unwrap_or_else(Decimal128::nan),
        _ => signed(p),
    }
}
