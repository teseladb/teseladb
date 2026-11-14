//! The domain of a range field: how many bits its values take once encoded (the reference
//! encryption library's `mc_getNumberOfBits`), which bounds the trim factor.

use teseladb_bson::Value;
use teseladb_bson::types as t;

use crate::values::decimal_to_f64;

/// The largest scaled double that precision mode keeps exact (2^53).
const SCALED_DOUBLE_BOUNDS: f64 = 9_007_199_254_740_992.0;
/// The bits from which precision mode is not used for doubles.
const DOUBLE_PRECISION_BITS: u32 = 53;

/// `ceil(log2(i))`, with a power of two giving its exponent.
fn ceil_log2(i: u128) -> u32 {
    let bits = 128 - i.leading_zeros();
    if i.is_power_of_two() { bits - 1 } else { bits }
}

fn int_of(v: Option<&Value>) -> Option<i128> {
    match v? {
        Value::Int32(n) => Some(i128::from(*n)),
        Value::Int64(n) | Value::DateTime(n) => Some(i128::from(*n)),
        _ => None,
    }
}

fn float_of(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Double(d) => Some(*d),
        Value::Decimal128(d) => decimal_to_f64(*d),
        _ => None,
    }
}

/// The bits of precision mode for floating bounds, if it can be used.
fn precision_bits(min: f64, max: f64, precision: i32, limit: u32) -> Option<u32> {
    let scale = 10f64.powi(precision);
    let (smax, smin) = (max * scale, min * scale);
    if min >= max || smax != smax.trunc() || smin != smin.trunc() {
        return None;
    }
    if smax.abs() >= SCALED_DOUBLE_BOUNDS || smin.abs() >= SCALED_DOUBLE_BOUNDS {
        return None;
    }
    let range = (smax as i128 - smin as i128) as u128;
    let bits = ceil_log2(range + scale as u128);
    (bits < limit).then_some(bits)
}

/// The bits of the encoded values of a range field of type `ty` with these options.
pub fn domain_bits(
    ty: u8,
    min: Option<&Value>,
    max: Option<&Value>,
    precision: Option<i32>,
) -> u32 {
    match ty {
        t::INT32 | t::INT64 | t::DATE_TIME => {
            let full = if ty == t::INT32 { 32 } else { 64 };
            match (int_of(min), int_of(max)) {
                (Some(lo), Some(hi)) if hi > lo => 128 - ((hi - lo) as u128).leading_zeros(),
                _ => full,
            }
        }
        t::DOUBLE | t::DECIMAL128 => {
            let (full, limit) =
                if ty == t::DOUBLE { (64, DOUBLE_PRECISION_BITS) } else { (128, 128) };
            match (float_of(min), float_of(max), precision) {
                (Some(lo), Some(hi), Some(p)) => precision_bits(lo, hi, p, limit).unwrap_or(full),
                _ => full,
            }
        }
        _ => 64,
    }
}

/// The largest scaled double bound (`i64::MAX`, as the message gives it).
const DOUBLE_BOUND: f64 = 9_223_372_036_854_775_807.0;
/// The largest scaled decimal bound (`i128::MAX`).
const DECIMAL_BOUND_TEXT: &str = "170141183460469231731687303715884105727";

/// Whether `v` has no more than `precision` decimals.
fn fits_precision(v: &Value, precision: i32) -> bool {
    match v {
        Value::Double(d) => {
            let scaled = d * 10f64.powi(precision);
            scaled == scaled.trunc()
        }
        Value::Decimal128(d) => match d.classify() {
            teseladb_bson::Decimal::Finite { coefficient, exponent, .. } => {
                let mut c = coefficient;
                let mut e = exponent;
                while e < 0 && c != 0 && c % 10 == 0 {
                    c /= 10;
                    e += 1;
                }
                c == 0 || e >= -precision
            }
            _ => false,
        },
        _ => true,
    }
}

/// Checks the bounds of a floating range field with a precision: no more decimals than it allows,
/// and scaled within what the encoding takes (measured on a reference replica set member 9.0.4).
pub fn check_precision(
    ty: u8,
    (min, max): (&Value, &Value),
    precision: i32,
    path: &str,
) -> Result<(), crate::FleError> {
    use crate::error::*;
    let (min_digits, max_digits) = match ty {
        t::DOUBLE => (DOUBLE_MIN_DIGITS, DOUBLE_MAX_DIGITS),
        t::DECIMAL128 => (DECIMAL_MIN_DIGITS, DECIMAL_MAX_DIGITS),
        _ => return Ok(()),
    };
    for (v, code, which) in [(min, min_digits, "minimum"), (max, max_digits, "maximum")] {
        if !fits_precision(v, precision) {
            return Err(err(
                code,
                format!(
                    "The number of decimal digits for {which} value of field '{path}' must be \
                     less than or equal to precision"
                ),
            ));
        }
    }
    let scale = 10f64.powi(precision);
    let scaled = |v: &Value| float_of(Some(v)).map_or(0.0, |f| (f * scale).abs());
    let (limit, upper, lower, name, shown) = if ty == t::DOUBLE {
        (DOUBLE_BOUND, DOUBLE_UPPER_BOUND, DOUBLE_LOWER_BOUND, "double", "9223372036854775807")
    } else {
        (2f64.powi(127), DECIMAL_UPPER_BOUND, DECIMAL_LOWER_BOUND, "Decimal128", DECIMAL_BOUND_TEXT)
    };
    for (v, code, which) in [(max, upper, "upper"), (min, lower, "lower")] {
        if scaled(v) >= limit {
            return Err(err(
                code,
                format!("Invalid {which} bounds for {name} precision, must be less than {shown}"),
            ));
        }
    }
    Ok(())
}
