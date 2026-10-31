//! Arithmetic of `$inc` and `$mul` with RefDb's type promotion: `int` widens to `long` on
//! overflow, a `long` that overflows fails, a `double` operand gives a `double` and a `decimal`
//! operand gives a `decimal` computed exactly and rounded to 34 digits, half to even. A double
//! becomes a decimal with 15 significant digits, as RefDb converts it.

use teseladb_bson::{Decimal, Decimal128, MAX_DIGITS, MAX_EXPONENT, MIN_EXPONENT, Value};

/// `+` or `×`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Add,
    Mul,
}

/// `a op b` for two numbers; `None` when a `long` overflows.
pub(crate) fn apply(op: Op, a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::Decimal128(_), _) | (_, Value::Decimal128(_)) => {
            Some(Value::Decimal128(decimal(op, to_decimal(a)?, to_decimal(b)?)))
        }
        (Value::Double(_), _) | (_, Value::Double(_)) => {
            let (x, y) = (to_f64(a)?, to_f64(b)?);
            Some(Value::Double(if op == Op::Add { x + y } else { x * y }))
        }
        (Value::Int32(x), Value::Int32(y)) => {
            let r = if op == Op::Add { x.checked_add(*y) } else { x.checked_mul(*y) };
            Some(match r {
                Some(n) => Value::Int32(n),
                None => Value::Int64(long(op, i64::from(*x), i64::from(*y))?),
            })
        }
        _ => Some(Value::Int64(long(op, to_i64(a)?, to_i64(b)?)?)),
    }
}

/// Zero of the type of `v` (what `$mul` sets on a missing field).
pub(crate) fn zero_like(v: &Value) -> Value {
    match v {
        Value::Int64(_) => Value::Int64(0),
        Value::Double(_) => Value::Double(0.0),
        Value::Decimal128(_) => {
            Value::Decimal128(Decimal128::from_parts(false, 0, 0).unwrap_or_else(Decimal128::nan))
        }
        _ => Value::Int32(0),
    }
}

fn long(op: Op, x: i64, y: i64) -> Option<i64> {
    if op == Op::Add { x.checked_add(y) } else { x.checked_mul(y) }
}

fn to_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Int32(n) => Some(i64::from(*n)),
        Value::Int64(n) => Some(*n),
        _ => None,
    }
}

fn to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int32(n) => Some(f64::from(*n)),
        Value::Int64(n) => Some(*n as f64),
        Value::Double(x) => Some(*x),
        _ => None,
    }
}

/// A decimal's value: NaN, an infinity or `±digits × 10^exp` (digits most significant first).
#[derive(Clone, Debug, PartialEq)]
enum Dec {
    NaN,
    Inf(bool),
    Fin { neg: bool, digits: Vec<u8>, exp: i32 },
}

fn digits_of(n: u128) -> Vec<u8> {
    n.to_string().bytes().map(|b| b - b'0').collect()
}

fn to_decimal(v: &Value) -> Option<Dec> {
    Some(match v {
        Value::Int32(n) => {
            Dec::Fin { neg: *n < 0, digits: digits_of(u128::from(n.unsigned_abs())), exp: 0 }
        }
        Value::Int64(n) => {
            Dec::Fin { neg: *n < 0, digits: digits_of(u128::from(n.unsigned_abs())), exp: 0 }
        }
        Value::Double(x) => from_double(*x),
        Value::Decimal128(d) => match d.classify() {
            Decimal::NaN => Dec::NaN,
            Decimal::Infinity { negative } => Dec::Inf(negative),
            Decimal::Finite { negative, coefficient, exponent } => {
                Dec::Fin { neg: negative, digits: digits_of(coefficient), exp: exponent }
            }
        },
        _ => return None,
    })
}

/// A double with 15 significant digits.
fn from_double(x: f64) -> Dec {
    if x.is_nan() {
        return Dec::NaN;
    }
    if x.is_infinite() {
        return Dec::Inf(x < 0.0);
    }
    if x == 0.0 {
        return Dec::Fin { neg: x.is_sign_negative(), digits: vec![0], exp: 0 };
    }
    // `d.dddddddddddddde±x`: 15 significant digits.
    let text = format!("{:.14e}", x.abs());
    let (mantissa, e) = text.split_once('e').unwrap_or((&text, "0"));
    let digits: Vec<u8> = mantissa.bytes().filter(u8::is_ascii_digit).map(|b| b - b'0').collect();
    let e: i32 = e.parse().unwrap_or(0);
    Dec::Fin { neg: x < 0.0, digits, exp: e - 14 }
}

fn decimal(op: Op, a: Dec, b: Dec) -> Decimal128 {
    let r = match (a, b) {
        (Dec::NaN, _) | (_, Dec::NaN) => Dec::NaN,
        (Dec::Inf(n1), Dec::Inf(n2)) => match op {
            Op::Add if n1 != n2 => Dec::NaN,
            Op::Add => Dec::Inf(n1),
            Op::Mul => Dec::Inf(n1 != n2),
        },
        (Dec::Inf(n), Dec::Fin { neg, digits, .. })
        | (Dec::Fin { neg, digits, .. }, Dec::Inf(n)) => match op {
            Op::Add => Dec::Inf(n),
            Op::Mul if is_zero(&digits) => Dec::NaN,
            Op::Mul => Dec::Inf(n != neg),
        },
        (Dec::Fin { neg: n1, digits: d1, exp: e1 }, Dec::Fin { neg: n2, digits: d2, exp: e2 }) => {
            match op {
                Op::Mul => Dec::Fin { neg: n1 != n2, digits: mul_digits(&d1, &d2), exp: e1 + e2 },
                Op::Add => add(n1, d1, e1, n2, d2, e2),
            }
        }
    };
    encode(r)
}

fn is_zero(d: &[u8]) -> bool {
    d.iter().all(|x| *x == 0)
}

fn add(n1: bool, d1: Vec<u8>, e1: i32, n2: bool, d2: Vec<u8>, e2: i32) -> Dec {
    let exp = e1.min(e2);
    let scale = |mut d: Vec<u8>, e: i32| {
        d.extend(std::iter::repeat_n(0, usize::try_from(e - exp).unwrap_or(0)));
        d
    };
    let (a, b) = (scale(d1, e1), scale(d2, e2));
    if n1 == n2 {
        return Dec::Fin { neg: n1, digits: add_digits(&a, &b), exp };
    }
    match cmp_digits(&a, &b) {
        std::cmp::Ordering::Equal => Dec::Fin { neg: false, digits: vec![0], exp },
        std::cmp::Ordering::Greater => Dec::Fin { neg: n1, digits: sub_digits(&a, &b), exp },
        std::cmp::Ordering::Less => Dec::Fin { neg: n2, digits: sub_digits(&b, &a), exp },
    }
}

fn trim(mut d: Vec<u8>) -> Vec<u8> {
    let lead = d.iter().take_while(|x| **x == 0).count().min(d.len().saturating_sub(1));
    d.drain(..lead);
    d
}

fn cmp_digits(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let (a, b) = (trim(a.to_vec()), trim(b.to_vec()));
    a.len().cmp(&b.len()).then_with(|| a.cmp(&b))
}

fn add_digits(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut i, mut j, mut carry) = (a.len(), b.len(), 0u8);
    while i > 0 || j > 0 || carry > 0 {
        let x = if i > 0 {
            i -= 1;
            a[i]
        } else {
            0
        };
        let y = if j > 0 {
            j -= 1;
            b[j]
        } else {
            0
        };
        let s = x + y + carry;
        out.push(s % 10);
        carry = s / 10;
    }
    out.reverse();
    trim(out)
}

/// `a − b` with `a ≥ b`.
fn sub_digits(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut i, mut j, mut borrow) = (a.len(), b.len(), 0i8);
    while i > 0 {
        i -= 1;
        let y = if j > 0 {
            j -= 1;
            b[j] as i8
        } else {
            0
        };
        let mut s = a[i] as i8 - y - borrow;
        borrow = 0;
        if s < 0 {
            s += 10;
            borrow = 1;
        }
        out.push(s as u8);
    }
    out.reverse();
    trim(out)
}

fn mul_digits(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut acc = vec![0u32; a.len() + b.len()];
    for (i, x) in a.iter().enumerate().rev() {
        for (j, y) in b.iter().enumerate().rev() {
            acc[i + j + 1] += u32::from(*x) * u32::from(*y);
        }
    }
    for k in (1..acc.len()).rev() {
        let carry = acc[k] / 10;
        acc[k] %= 10;
        acc[k - 1] += carry;
    }
    trim(acc.into_iter().map(|d| d as u8).collect())
}

/// Rounds to 34 digits, half to even, and to the exponent range.
fn encode(d: Dec) -> Decimal128 {
    let (neg, mut digits, mut exp) = match d {
        Dec::NaN => return Decimal128::nan(),
        Dec::Inf(n) => return Decimal128::infinity(n),
        Dec::Fin { neg, digits, exp } => (neg, trim(digits), exp),
    };
    // Too many digits or an exponent below the minimum: drop digits with rounding.
    let excess = digits
        .len()
        .saturating_sub(MAX_DIGITS)
        .max(usize::try_from(MIN_EXPONENT - exp).unwrap_or(0));
    if excess > 0 {
        let n = digits.len();
        // The first dropped digit decides; the ones after it break ties.
        let (first, rest_zero) = if excess > n {
            (0, is_zero(&digits))
        } else {
            (digits[n - excess], is_zero(&digits[n - excess + 1..]))
        };
        digits.truncate(n.saturating_sub(excess));
        exp += i32::try_from(excess).unwrap_or(i32::MAX);
        let last_odd = digits.last().is_some_and(|x| x % 2 == 1);
        if first > 5 || (first == 5 && (!rest_zero || last_odd)) {
            digits = add_digits(&digits, &[1]);
            if digits.len() > MAX_DIGITS {
                digits.pop();
                exp += 1;
            }
        }
        if digits.is_empty() {
            digits.push(0);
        }
    }
    // An exponent above the maximum: pad the coefficient with zeros while it fits.
    while exp > MAX_EXPONENT && digits.len() < MAX_DIGITS && !is_zero(&digits) {
        digits.push(0);
        exp -= 1;
    }
    if is_zero(&digits) {
        exp = exp.clamp(MIN_EXPONENT, MAX_EXPONENT);
    }
    if exp > MAX_EXPONENT {
        return Decimal128::infinity(neg);
    }
    let coefficient = digits.iter().fold(0u128, |acc, d| acc * 10 + u128::from(*d));
    Decimal128::from_parts(neg, coefficient, exp).unwrap_or_else(Decimal128::nan)
}

/// An operation on decimals of the expressions of 010 (`$add`, `$subtract`, `$multiply`,
/// `$divide`, `$mod`), exact and rounded to 34 digits, half to even.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecOp {
    /// `+`.
    Add,
    /// `−`.
    Sub,
    /// `×`.
    Mul,
    /// `÷`.
    Div,
    /// The remainder of the division truncated toward zero.
    Rem,
}

/// `a op b` as decimals; `None` when an operand is not a number.
pub fn dec_binary(op: DecOp, a: &Value, b: &Value) -> Option<Decimal128> {
    let (x, y) = (to_decimal(a)?, to_decimal(b)?);
    Some(match op {
        DecOp::Add => decimal(Op::Add, x, y),
        DecOp::Mul => decimal(Op::Mul, x, y),
        DecOp::Sub => decimal(Op::Add, x, negate(y)),
        DecOp::Div => encode(divide(x, y)),
        DecOp::Rem => encode(remainder(x, y)),
    })
}

/// `v` as a decimal (a double with 15 significant digits); `None` when it is not a number.
pub fn to_decimal128(v: &Value) -> Option<Decimal128> {
    to_decimal(v).map(encode)
}

/// The double nearest to decimal `d`.
pub fn decimal_to_f64(d: Decimal128) -> f64 {
    d.to_string().parse().unwrap_or(f64::NAN)
}

fn negate(d: Dec) -> Dec {
    match d {
        Dec::Fin { neg, digits, exp } => Dec::Fin { neg: !neg, digits, exp },
        Dec::Inf(n) => Dec::Inf(!n),
        Dec::NaN => Dec::NaN,
    }
}

/// `a ÷ b` for digit strings: the quotient digits (at least `MAX_DIGITS + 1` significant ones
/// when it does not end) and whether a remainder was left.
fn long_division(a: &[u8], b: &[u8], digits: usize) -> (Vec<u8>, bool) {
    let mut q = Vec::new();
    let mut rem: Vec<u8> = vec![0];
    let mut i = 0;
    let mut significant = 0;
    while i < a.len() || (significant < digits && !is_zero(&rem)) {
        rem.push(if i < a.len() { a[i] } else { 0 });
        rem = trim(rem);
        let mut d = 0;
        while cmp_digits(&rem, b) != std::cmp::Ordering::Less {
            rem = sub_digits(&rem, b);
            d += 1;
        }
        if d > 0 || significant > 0 {
            significant += 1;
        }
        q.push(d);
        i += 1;
    }
    (q, !is_zero(&rem))
}

fn divide(a: Dec, b: Dec) -> Dec {
    match (a, b) {
        (Dec::NaN, _) | (_, Dec::NaN) | (Dec::Inf(_), Dec::Inf(_)) => Dec::NaN,
        (Dec::Inf(n), Dec::Fin { neg, .. }) => Dec::Inf(n != neg),
        (Dec::Fin { neg, .. }, Dec::Inf(n)) => Dec::Fin { neg: neg != n, digits: vec![0], exp: 0 },
        (Dec::Fin { neg: n1, digits: d1, exp: e1 }, Dec::Fin { neg: n2, digits: d2, exp: e2 }) => {
            if is_zero(&d2) {
                return if is_zero(&d1) { Dec::NaN } else { Dec::Inf(n1 != n2) };
            }
            let (mut q, inexact) = long_division(&d1, &d2, MAX_DIGITS + 2);
            // Digits past the dividend's are fractional; a remainder left is a sticky digit.
            let extra = q.len().saturating_sub(d1.len());
            if inexact {
                q.push(1);
            }
            let shift = i32::try_from(extra + usize::from(inexact)).unwrap_or(i32::MAX);
            Dec::Fin { neg: n1 != n2, digits: trim(q), exp: e1 - e2 - shift }
        }
    }
}

fn remainder(a: Dec, b: Dec) -> Dec {
    match (a, b) {
        (Dec::NaN, _) | (_, Dec::NaN) | (Dec::Inf(_), _) => Dec::NaN,
        (f @ Dec::Fin { .. }, Dec::Inf(_)) => f,
        (Dec::Fin { neg: n1, digits: d1, exp: e1 }, Dec::Fin { digits: d2, exp: e2, .. }) => {
            if is_zero(&d2) {
                return Dec::NaN;
            }
            let exp = e1.min(e2);
            let scale = |mut d: Vec<u8>, e: i32| {
                d.extend(std::iter::repeat_n(0, usize::try_from(e - exp).unwrap_or(0)));
                d
            };
            let (x, y) = (scale(d1, e1), scale(d2, e2));
            let mut rem: Vec<u8> = vec![0];
            for digit in &x {
                rem.push(*digit);
                rem = trim(rem);
                while cmp_digits(&rem, &y) != std::cmp::Ordering::Less {
                    rem = sub_digits(&rem, &y);
                }
            }
            Dec::Fin { neg: n1, digits: rem, exp }
        }
    }
}

/// `±digits × 10^exp` rounded to 34 digits, half to even, keeping the digits it keeps (a
/// coefficient of 34 digits when it had more).
pub(crate) fn encode_digits(neg: bool, digits: Vec<u8>, exp: i32) -> Decimal128 {
    encode(Dec::Fin { neg, digits, exp })
}
