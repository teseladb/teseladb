//! Whole 64-bit integers read from numbers as the reference server reads them: exactly, with the
//! messages of its `exactInt64` parse (`$pop`, `maxTimeMS`, `count`'s `limit`), or rounded and
//! saturated into a "safe" integer field (`skip`, `limit`, `batchSize`).

use teseladb_bson::{Decimal, Decimal128, RawValue};

use crate::fmt::{double, shell};

/// The bounds of `i64` as doubles; both are exact powers of two.
const I64_LOW: f64 = -9_223_372_036_854_775_808.0;
const I64_HIGH: f64 = 9_223_372_036_854_775_808.0;

/// `v` as an `i64` when it is a number equal to one; otherwise the reference server's message,
/// naming the element `field`.
pub fn exact_int64(v: RawValue<'_>, field: &str) -> Result<i64, String> {
    let not_int64 =
        |shown: String| format!("Cannot represent as a 64-bit integer: {field}: {shown}");
    match v {
        RawValue::Int32(n) => Ok(i64::from(n)),
        RawValue::Int64(n) => Ok(n),
        RawValue::Double(x) if x.is_nan() => {
            Err(format!("Expected an integer, but found NaN in: {field}: nan"))
        }
        RawValue::Double(x) if x.is_finite() && x.fract() != 0.0 => {
            Err(format!("Expected an integer: {field}: {}", double(x)))
        }
        // In range, `as` is exact for a whole double.
        RawValue::Double(x) if (I64_LOW..I64_HIGH).contains(&x) => Ok(x as i64),
        RawValue::Double(x) => Err(not_int64(double(x))),
        RawValue::Decimal128(d) => decimal_exact(d).ok_or_else(|| not_int64(d.to_string())),
        other => Err(format!("Expected a number in: {field}: {}", shell(other))),
    }
}

/// `±coefficient × 10^exponent` as an exact `i128` quotient and remainder of the division by
/// `10^-exponent` (`None` when the product overflows); the remainder's divisor comes with it.
fn split_decimal(coefficient: u128, exponent: i32) -> Option<(u128, u128, u128)> {
    if exponent >= 0 {
        let scale = 10u128.checked_pow(exponent.unsigned_abs())?;
        return Some((coefficient.checked_mul(scale)?, 0, 1));
    }
    match 10u128.checked_pow(exponent.unsigned_abs()) {
        Some(div) => Some((coefficient / div, coefficient % div, div)),
        // A divisor past `u128` is above any coefficient (at most 34 digits).
        None => Some((0, coefficient, u128::MAX)),
    }
}

/// A signed whole number from its magnitude, saturating at the bounds of `i64`.
fn saturate(negative: bool, magnitude: u128) -> i64 {
    let m = i128::try_from(magnitude).unwrap_or(i128::MAX);
    let v = if negative { -m } else { m };
    i64::try_from(v).unwrap_or(if negative { i64::MIN } else { i64::MAX })
}

/// A decimal128 as an `i64` when it is one exactly (`2.00`, `1E+1`), as the reference server's
/// `exactInt64` reads it.
pub fn decimal_exact(d: Decimal128) -> Option<i64> {
    let Decimal::Finite { negative, coefficient, exponent } = d.classify() else {
        return None;
    };
    let (whole, rest, _) = split_decimal(coefficient, exponent)?;
    let m = i128::try_from(whole).ok().filter(|_| rest == 0)?;
    i64::try_from(if negative { -m } else { m }).ok()
}

/// A decimal128 rounded half to even and saturated to `i64`, NaN as 0: how the reference server
/// reads it into a "safe" 64-bit integer field (`skip`, `limit`, `batchSize`).
pub fn decimal_saturating(d: Decimal128) -> i64 {
    match d.classify() {
        Decimal::NaN => 0,
        Decimal::Infinity { negative } => saturate(negative, u128::MAX),
        Decimal::Finite { negative, coefficient, exponent } => {
            let Some((whole, rest, div)) = split_decimal(coefficient, exponent) else {
                return saturate(negative, u128::MAX);
            };
            // `rest < div`, so comparing `rest` with `div - rest` never overflows.
            let up = match rest.cmp(&(div - rest)) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Equal => whole % 2 == 1,
                std::cmp::Ordering::Less => false,
            };
            saturate(negative, whole + u128::from(up))
        }
    }
}
