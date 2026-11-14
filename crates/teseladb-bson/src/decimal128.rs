//! Decimal128 (IEEE 754-2008, BID encoding; FR-001, FR-006): the 16 bytes BSON stores, their
//! value, the canonical text of RefDb's specification and parsing without rounding, and exact
//! comparison.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use crate::number::{self, Num};

/// Exponent bias of the encoding.
const BIAS: i32 = 6176;
/// Largest exponent.
pub const MAX_EXPONENT: i32 = 6111;
/// Smallest exponent.
pub const MIN_EXPONENT: i32 = -6176;
/// Largest number of significant digits.
pub const MAX_DIGITS: usize = 34;
/// Largest coefficient, `10^34 − 1`.
const MAX_COEFFICIENT: u128 = 9_999_999_999_999_999_999_999_999_999_999_999;

/// A decimal128 value as BSON stores it (little-endian).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Decimal128 {
    bytes: [u8; 16],
}

/// What a decimal128 is worth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decimal {
    /// Not a number (sign and payload ignored).
    NaN,
    /// An infinity.
    Infinity {
        /// Negative.
        negative: bool,
    },
    /// `±coefficient × 10^exponent`; a coefficient past `10^34 − 1` (not canonical) is 0.
    Finite {
        /// Negative (also for zero).
        negative: bool,
        /// Coefficient, at most `10^34 − 1`.
        coefficient: u128,
        /// Exponent, from [`MIN_EXPONENT`] to [`MAX_EXPONENT`].
        exponent: i32,
    },
}

/// Text that is not a decimal128, or that one cannot hold exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseDecimalError(&'static str);

impl fmt::Display for ParseDecimalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not a decimal128: {}", self.0)
    }
}

impl std::error::Error for ParseDecimalError {}

impl Decimal128 {
    /// From its 16 little-endian bytes.
    #[must_use]
    pub fn from_le_bytes(bytes: [u8; 16]) -> Decimal128 {
        Decimal128 { bytes }
    }

    /// Its 16 little-endian bytes.
    #[must_use]
    pub fn to_le_bytes(self) -> [u8; 16] {
        self.bytes
    }

    /// The canonical NaN.
    #[must_use]
    pub fn nan() -> Decimal128 {
        Decimal128::from_bits(0x7C00u128 << 112)
    }

    /// An infinity.
    #[must_use]
    pub fn infinity(negative: bool) -> Decimal128 {
        Decimal128::from_bits((0x7800u128 << 112) | (u128::from(negative) << 127))
    }

    /// `±coefficient × 10^exponent`, if it fits: at most 34 digits and the exponent in range.
    #[must_use]
    #[expect(
        clippy::cast_sign_loss,
        reason = "the exponent is checked to be in range, so the biased exponent is between 0 and 12 287"
    )]
    pub fn from_parts(negative: bool, coefficient: u128, exponent: i32) -> Option<Decimal128> {
        if coefficient > MAX_COEFFICIENT || !(MIN_EXPONENT..=MAX_EXPONENT).contains(&exponent) {
            return None;
        }
        let biased = (exponent + BIAS) as u128;
        Some(Decimal128::from_bits((u128::from(negative) << 127) | (biased << 113) | coefficient))
    }

    fn from_bits(v: u128) -> Decimal128 {
        Decimal128 { bytes: v.to_le_bytes() }
    }

    /// What it is worth.
    #[must_use]
    #[expect(clippy::cast_possible_truncation, reason = "the biased exponent has 14 bits")]
    pub fn classify(self) -> Decimal {
        let v = u128::from_le_bytes(self.bytes);
        let negative = v >> 127 == 1;
        match (v >> 122) & 0x1F {
            0x1F => return Decimal::NaN,
            0x1E => return Decimal::Infinity { negative },
            _ => {}
        }
        // With the two high bits of the combination at 11 the coefficient would need 114 bits,
        // more than 10^34 − 1: it is not canonical and is worth 0, with the exponent 2 bits lower.
        let (biased, coefficient) = if (v >> 125) & 0b11 == 0b11 {
            ((v >> 111) & 0x3FFF, 0)
        } else {
            ((v >> 113) & 0x3FFF, v & ((1u128 << 113) - 1))
        };
        let coefficient = if coefficient > MAX_COEFFICIENT { 0 } else { coefficient };
        Decimal::Finite { negative, coefficient, exponent: biased as i32 - BIAS }
    }

    /// Compares the values: NaN equals NaN and goes before every number, `-0` equals `0`, and
    /// `1.0` equals `1.00`.
    #[must_use]
    pub fn cmp_value(self, other: Decimal128) -> Ordering {
        number::compare(Num::decimal(self), Num::decimal(other))
    }
}

impl fmt::Display for Decimal128 {
    /// The canonical text of RefDb's decimal128 specification.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (negative, coefficient, exponent) = match self.classify() {
            Decimal::NaN => return f.write_str("NaN"),
            Decimal::Infinity { negative } => {
                return f.write_str(if negative { "-Infinity" } else { "Infinity" });
            }
            Decimal::Finite { negative, coefficient, exponent } => {
                (negative, coefficient, exponent)
            }
        };
        if negative {
            f.write_str("-")?;
        }
        let digits = coefficient.to_string();
        // At most 34 digits.
        let n = i32::try_from(digits.len()).unwrap_or(i32::MAX);
        let adjusted = exponent + n - 1;
        if exponent <= 0 && adjusted >= -6 {
            let point = n + exponent;
            if exponent == 0 {
                f.write_str(&digits)
            } else if point > 0 {
                write!(
                    f,
                    "{}.{}",
                    &digits[..usize::try_from(point).unwrap_or(0)],
                    &digits[usize::try_from(point).unwrap_or(0)..]
                )
            } else {
                write!(f, "0.{}{digits}", "0".repeat(usize::try_from(-point).unwrap_or(0)))
            }
        } else {
            let sign = if adjusted < 0 { '-' } else { '+' };
            match digits.split_at(1) {
                (first, "") => write!(f, "{first}E{sign}{}", adjusted.abs()),
                (first, rest) => write!(f, "{first}.{rest}E{sign}{}", adjusted.abs()),
            }
        }
    }
}

impl FromStr for Decimal128 {
    type Err = ParseDecimalError;

    /// Reads a number as RefDb's specification does: optional sign, digits with at most one
    /// point, optional exponent, or `Infinity`, `Inf` and `NaN` in any case. A value that would
    /// need rounding is refused.
    fn from_str(s: &str) -> Result<Decimal128, ParseDecimalError> {
        let (negative, rest) = match s.as_bytes().first() {
            Some(b'-') => (true, &s[1..]),
            Some(b'+') => (false, &s[1..]),
            _ => (false, s),
        };
        let word = rest.to_ascii_lowercase();
        if word == "infinity" || word == "inf" {
            return Ok(Decimal128::infinity(negative));
        }
        if word == "nan" {
            return Ok(Decimal128::nan());
        }
        let (mantissa, exp_text) = match rest.find(['e', 'E']) {
            Some(i) => (&rest[..i], Some(&rest[i + 1..])),
            None => (rest, None),
        };
        let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        let all_digits = |t: &str| t.bytes().all(|c| c.is_ascii_digit());
        if int.len() + frac.len() == 0 || !all_digits(int) || !all_digits(frac) {
            return Err(ParseDecimalError("digits with at most one point"));
        }
        let mut exponent = match exp_text {
            Some(t) => exponent_of(t)?,
            None => 0,
        } - i64::try_from(frac.len()).unwrap_or(i64::MAX);
        let all = format!("{int}{frac}");
        let mut digits = all.trim_start_matches('0').to_owned();
        if digits.is_empty() {
            // Zero takes any exponent: the closest one in range.
            let e = i32::try_from(exponent.clamp(i64::from(MIN_EXPONENT), i64::from(MAX_EXPONENT)))
                .unwrap_or(MAX_EXPONENT);
            return Ok(Decimal128::from_parts(negative, 0, e).expect("zero in range"));
        }
        // Too many digits: only trailing zeros may go.
        while digits.len() > MAX_DIGITS && digits.ends_with('0') {
            digits.pop();
            exponent += 1;
        }
        if digits.len() > MAX_DIGITS {
            return Err(ParseDecimalError("more than 34 significant digits"));
        }
        // An exponent too large is lowered by adding zeros, one too small raised by removing them.
        while exponent > i64::from(MAX_EXPONENT) && digits.len() < MAX_DIGITS {
            digits.push('0');
            exponent -= 1;
        }
        while exponent < i64::from(MIN_EXPONENT) && digits.ends_with('0') {
            digits.pop();
            exponent += 1;
        }
        let e = i32::try_from(exponent).map_err(|_| ParseDecimalError("exponent out of range"))?;
        let coefficient = digits.parse().expect("at most 34 digits");
        Decimal128::from_parts(negative, coefficient, e)
            .ok_or(ParseDecimalError("exponent out of range"))
    }
}

/// An exponent: optional sign and at least one digit; a huge one saturates, and is refused or
/// clamped later like any other out of range.
fn exponent_of(t: &str) -> Result<i64, ParseDecimalError> {
    let (negative, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return Err(ParseDecimalError("an exponent needs digits"));
    }
    let magnitude = digits
        .bytes()
        .fold(0i64, |acc, c| acc.saturating_mul(10).saturating_add(i64::from(c - b'0')));
    let capped = magnitude.min(1_000_000_000);
    Ok(if negative { -capped } else { capped })
}
