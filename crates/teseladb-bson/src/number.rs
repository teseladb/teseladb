//! Numbers compared by exact value across int32, int64, double and decimal128 (FR-006): NaN equals
//! NaN and goes before every number, `-0` equals `0`, and a double is worth exactly the binary
//! fraction it holds.

use std::cmp::Ordering;

use crate::bignum::Big;
use crate::decimal128::{Decimal, Decimal128};

/// A number reduced to what comparing needs.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Num {
    NaN,
    Infinity {
        negative: bool,
    },
    /// `±coefficient × 10^exponent` (integers and decimals).
    Decimal {
        negative: bool,
        coefficient: u128,
        exponent: i32,
    },
    /// `±mantissa × 2^exponent` (finite doubles).
    Binary {
        negative: bool,
        mantissa: u64,
        exponent: i32,
    },
}

impl Num {
    pub(crate) fn int(n: i64) -> Num {
        Num::Decimal { negative: n < 0, coefficient: u128::from(n.unsigned_abs()), exponent: 0 }
    }

    pub(crate) fn double(x: f64) -> Num {
        if x.is_nan() {
            return Num::NaN;
        }
        if x.is_infinite() {
            return Num::Infinity { negative: x < 0.0 };
        }
        let bits = x.to_bits();
        let negative = bits >> 63 == 1;
        let biased = ((bits >> 52) & 0x7FF) as i32;
        let fraction = bits & ((1 << 52) - 1);
        // Subnormals have no hidden bit and the exponent of the smallest normal.
        let (mantissa, exponent) =
            if biased == 0 { (fraction, -1074) } else { (fraction | (1 << 52), biased - 1075) };
        Num::Binary { negative, mantissa, exponent }
    }

    pub(crate) fn decimal(d: Decimal128) -> Num {
        match d.classify() {
            Decimal::NaN => Num::NaN,
            Decimal::Infinity { negative } => Num::Infinity { negative },
            Decimal::Finite { negative, coefficient, exponent } => {
                Num::Decimal { negative, coefficient, exponent }
            }
        }
    }

    /// NaN, −∞, negatives, zero, positives, +∞.
    fn rank(self) -> u8 {
        match self {
            Num::NaN => 0,
            Num::Infinity { negative: true } => 1,
            Num::Decimal { coefficient: 0, .. } | Num::Binary { mantissa: 0, .. } => 3,
            Num::Decimal { negative: true, .. } | Num::Binary { negative: true, .. } => 2,
            Num::Decimal { .. } | Num::Binary { .. } => 4,
            Num::Infinity { negative: false } => 5,
        }
    }
}

/// Compares two numbers by value.
pub(crate) fn compare(a: Num, b: Num) -> Ordering {
    let (ra, rb) = (a.rank(), b.rank());
    if ra != rb {
        return ra.cmp(&rb);
    }
    if ra != 2 && ra != 4 {
        return Ordering::Equal;
    }
    let m = magnitude(a, b);
    if ra == 2 { m.reverse() } else { m }
}

/// Compares the magnitudes of two non-zero finite numbers.
fn magnitude(a: Num, b: Num) -> Ordering {
    match (a, b) {
        (
            Num::Decimal { coefficient: c1, exponent: e1, .. },
            Num::Decimal { coefficient: c2, exponent: e2, .. },
        ) => decimal_magnitude(c1, e1, c2, e2),
        (
            Num::Binary { mantissa: m1, exponent: e1, .. },
            Num::Binary { mantissa: m2, exponent: e2, .. },
        ) => binary_magnitude(m1, e1, m2, e2),
        (
            Num::Decimal { coefficient, exponent, .. },
            Num::Binary { mantissa, exponent: e2, .. },
        ) => decimal_binary(coefficient, exponent, mantissa, e2),
        (
            Num::Binary { mantissa, exponent, .. },
            Num::Decimal { coefficient, exponent: e1, .. },
        ) => decimal_binary(coefficient, e1, mantissa, exponent).reverse(),
        _ => unreachable!("finite and non-zero"),
    }
}

/// Compares `c1 × 10^e1` with `c2 × 10^e2`, both coefficients non-zero and of at most 34 digits.
pub(crate) fn decimal_magnitude(c1: u128, e1: i32, c2: u128, e2: i32) -> Ordering {
    let digits = |c: u128| c.ilog10().cast_signed() + 1;
    let (a1, a2) = (e1 + digits(c1), e2 + digits(c2));
    if a1 != a2 {
        return a1.cmp(&a2);
    }
    // Same adjusted exponent: scaled to the smaller exponent, each keeps at most 34 digits.
    let low = e1.min(e2);
    let scale = |c: u128, e: i32| c * 10u128.pow((e - low).cast_unsigned());
    scale(c1, e1).cmp(&scale(c2, e2))
}

/// Compares `m1 × 2^e1` with `m2 × 2^e2`, both mantissas non-zero and of at most 53 bits.
fn binary_magnitude(m1: u64, e1: i32, m2: u64, e2: i32) -> Ordering {
    let bits = |m: u64| (64 - m.leading_zeros()).cast_signed();
    let (p1, p2) = (e1 + bits(m1), e2 + bits(m2));
    if p1 != p2 {
        return p1.cmp(&p2);
    }
    // Same top bit: the exponents differ by at most 52 bits, which fits in a `u128`.
    let low = e1.min(e2);
    let scale = |m: u64, e: i32| u128::from(m) << (e - low);
    scale(m1, e1).cmp(&scale(m2, e2))
}

/// Compares `c × 10^k` with `m × 2^e`, both non-zero.
#[expect(
    clippy::many_single_char_names,
    reason = "c × 10^k against m × 2^e, in the names of the comparison"
)]
fn decimal_binary(c: u128, k: i32, m: u64, e: i32) -> Ordering {
    const LOG2_10: f64 = std::f64::consts::LOG2_10;
    // The decimal is in [10^(k + digits − 1), 10^(k + digits)), the double in [2^(p − 1), 2^p);
    // a margin of a bit covers the rounding of the logarithms.
    let dc = c.ilog10().cast_signed() + 1;
    let p = e + (64 - m.leading_zeros()).cast_signed();
    if f64::from(k + dc) * LOG2_10 < f64::from(p - 1) - 1.0 {
        return Ordering::Less;
    }
    if f64::from(k + dc - 1) * LOG2_10 > f64::from(p) + 1.0 {
        return Ordering::Greater;
    }
    let decimal = Scaled { value: c, pow10: k, pow2: 0 };
    let binary = Scaled { value: u128::from(m), pow10: 0, pow2: e };
    exact(decimal, binary)
}

/// `value × 10^pow10 × 2^pow2`.
#[derive(Clone, Copy)]
struct Scaled {
    value: u128,
    pow10: i32,
    pow2: i32,
}

/// Compares two scaled values with integers of any size: a negative exponent moves to the other
/// side as a positive one.
#[expect(
    clippy::many_single_char_names,
    reason = "a and b are the two numbers and x, y their scaled integers"
)]
fn exact(a: Scaled, b: Scaled) -> Ordering {
    let (mut x, mut y) = (Big::from_u128(a.value), Big::from_u128(b.value));
    let (k, e) = (a.pow10 - b.pow10, a.pow2 - b.pow2);
    if k >= 0 {
        x.mul_pow10(k.cast_unsigned());
    } else {
        y.mul_pow10(k.unsigned_abs());
    }
    if e >= 0 {
        x.shl(e.cast_unsigned());
    } else {
        y.shl(e.unsigned_abs());
    }
    x.cmp(&y)
}

/// A number as decimal digits, for keys that keep its order (an index key encoding, for instance).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NumberParts {
    /// Not a number.
    NaN,
    /// An infinity.
    Infinity {
        /// Negative.
        negative: bool,
    },
    /// Zero, of either sign.
    Zero,
    /// `±0.d₁d₂…dₙ × 10^exponent`, with `d₁` and `dₙ` not zero.
    Finite {
        /// Negative.
        negative: bool,
        /// Digits, 0 to 9 each.
        digits: Vec<u8>,
        /// Exponent.
        exponent: i32,
    },
}

/// The decimal parts of a number's exact value; `None` for anything else.
#[must_use]
pub fn number_parts(v: &crate::RawValue<'_>) -> Option<NumberParts> {
    let num = match *v {
        crate::RawValue::Int32(n) => Num::int(i64::from(n)),
        crate::RawValue::Int64(n) => Num::int(n),
        crate::RawValue::Double(x) => Num::double(x),
        crate::RawValue::Decimal128(d) => Num::decimal(d),
        _ => return None,
    };
    Some(match num {
        Num::NaN => NumberParts::NaN,
        Num::Infinity { negative } => NumberParts::Infinity { negative },
        Num::Decimal { coefficient: 0, .. } | Num::Binary { mantissa: 0, .. } => NumberParts::Zero,
        Num::Decimal { negative, coefficient, exponent } => {
            let digits: Vec<u8> = coefficient.to_string().bytes().map(|b| b - b'0').collect();
            finite(negative, digits, exponent)
        }
        Num::Binary { negative, mantissa, exponent } => {
            // m × 2^e is m × 2^e (an integer) for e ≥ 0 and m × 5^−e × 10^e for e < 0.
            let mut big = Big::from_u128(u128::from(mantissa));
            let ten = if exponent >= 0 {
                big.shl(exponent.cast_unsigned());
                0
            } else {
                big.mul_pow5(exponent.unsigned_abs());
                exponent
            };
            finite(negative, big.decimal_digits(), ten)
        }
    })
}

/// `±digits × 10^ten` (an integer's digits) as `0.d₁… × 10^exponent` without trailing zeros.
fn finite(negative: bool, mut digits: Vec<u8>, ten: i32) -> NumberParts {
    // A number has a few thousand digits at most.
    let exponent = ten + i32::try_from(digits.len()).unwrap_or(i32::MAX);
    while digits.last() == Some(&0) {
        digits.pop();
    }
    NumberParts::Finite { negative, digits, exponent }
}
