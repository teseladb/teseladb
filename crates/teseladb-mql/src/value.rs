//! Owned values viewed as BSON (so that they compare with [`teseladb_bson::compare`]) and the
//! numeric helpers the operators share.

use std::cmp::Ordering;

use teseladb_bson::{
    Collator, Decimal, Document, Limits, MAX_COMMAND_DEPTH, RawDoc, RawValue, Value, compare,
};

use crate::error::{MqlError, Result};

/// Limits of the values and documents an operator holds: a value of a command may sit as deep as
/// a command allows (one level more inside its holder), and the result of an update may pass a
/// stored document's size, which the server refuses with its own words when it stores it.
pub const VALUE_LIMITS: Limits = Limits { max_size: 64 << 20, max_depth: MAX_COMMAND_DEPTH + 1 };

/// An owned value kept as the BSON of `{"": value}`, so that it can be read as a [`RawValue`].
#[derive(Clone, Debug, PartialEq)]
pub struct OwnedValue {
    bytes: Vec<u8>,
}

impl OwnedValue {
    /// The value `v`.
    pub fn new(v: Value) -> Result<OwnedValue> {
        let bytes = Document::new().with("", v).to_bytes_with(VALUE_LIMITS).map_err(bson_error)?;
        Ok(OwnedValue { bytes })
    }

    /// A copy of `v`.
    pub fn from_raw(v: RawValue<'_>) -> Result<OwnedValue> {
        OwnedValue::new(Value::from(v))
    }

    /// The value.
    pub fn get(&self) -> RawValue<'_> {
        // The bytes were written by `to_bytes` from a document with one field named "".
        match self.holder().iter().next() {
            Some((_, v)) => v,
            None => RawValue::Null,
        }
    }

    /// The document `{"": value}` that holds the value.
    fn holder(&self) -> RawDoc<'_> {
        RawDoc::from_bytes_with(&self.bytes, VALUE_LIMITS).unwrap_or_else(|_| empty_doc())
    }

    /// The BSON of `{"": value}`, as `Document::to_bytes` wrote it.
    pub(crate) fn holder_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The value as an owned [`Value`].
    pub fn to_value(&self) -> Value {
        Value::from(self.get())
    }
}

/// An owned document kept as BSON, readable as a [`RawDoc`].
#[derive(Clone, Debug, PartialEq)]
pub struct OwnedDoc {
    bytes: Vec<u8>,
}

impl OwnedDoc {
    /// The document `d`.
    pub fn new(d: &Document) -> Result<OwnedDoc> {
        let bytes = d.to_bytes_with(VALUE_LIMITS).map_err(bson_error)?;
        Ok(OwnedDoc { bytes })
    }

    /// The document.
    pub fn get(&self) -> RawDoc<'_> {
        RawDoc::from_bytes_with(&self.bytes, VALUE_LIMITS).unwrap_or_else(|_| empty_doc())
    }

    /// Its bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// The empty document.
pub(crate) fn empty_doc() -> RawDoc<'static> {
    const EMPTY: [u8; 5] = [5, 0, 0, 0, 0];
    match RawDoc::from_bytes(&EMPTY) {
        Ok(d) => d,
        Err(_) => unreachable!("five bytes are the empty document"),
    }
}

/// A BSON error with its RefDb code.
pub(crate) fn bson_error(e: teseladb_bson::BsonError) -> MqlError {
    let (code, name) = e.code();
    MqlError::new(crate::ErrorCode { code, name }, e.to_string())
}

/// `a` compared with `b` in RefDb's order, strings by `collation`.
pub(crate) fn cmp(a: RawValue<'_>, b: RawValue<'_>, collation: Option<&dyn Collator>) -> Ordering {
    compare(a, b, collation)
}

/// Whether two values are equal in RefDb's order (also across numeric types).
pub(crate) fn equal(a: RawValue<'_>, b: RawValue<'_>, collation: Option<&dyn Collator>) -> bool {
    cmp(a, b, collation) == Ordering::Equal
}

/// Whether the value is a number.
pub(crate) fn is_number(v: &RawValue<'_>) -> bool {
    matches!(
        v,
        RawValue::Int32(_) | RawValue::Int64(_) | RawValue::Double(_) | RawValue::Decimal128(_)
    )
}

/// Whether the value is a NaN of either floating type.
pub(crate) fn is_nan(v: &RawValue<'_>) -> bool {
    match v {
        RawValue::Double(x) => x.is_nan(),
        RawValue::Decimal128(d) => d.classify() == Decimal::NaN,
        _ => false,
    }
}

/// The value as an `f64`, for numbers (decimals rounded).
pub(crate) fn to_f64(v: &RawValue<'_>) -> Option<f64> {
    match *v {
        RawValue::Int32(n) => Some(f64::from(n)),
        RawValue::Int64(n) => Some(n as f64),
        RawValue::Double(x) => Some(x),
        // The canonical text ("1.5E+3", "NaN", "-Infinity") is what `f64` reads.
        RawValue::Decimal128(d) => d.to_string().parse::<f64>().ok(),
        _ => None,
    }
}

/// A number truncated toward zero to an `i64`; `None` for NaN, an infinity, a value out of range
/// or something that is not a number.
pub(crate) fn truncate_i64(v: &RawValue<'_>) -> Option<i64> {
    match *v {
        RawValue::Int32(n) => Some(i64::from(n)),
        RawValue::Int64(n) => Some(n),
        RawValue::Double(x) => f64_to_i64(x.trunc()),
        RawValue::Decimal128(d) => match d.classify() {
            Decimal::Finite { negative, coefficient, exponent } => {
                decimal_trunc_i64(negative, coefficient, exponent)
            }
            _ => None,
        },
        _ => None,
    }
}

/// The number as an `i64` only if it is an integer that fits.
pub(crate) fn exact_i64(v: &RawValue<'_>) -> Option<i64> {
    let n = truncate_i64(v)?;
    let whole = match *v {
        RawValue::Double(x) => x.fract() == 0.0,
        RawValue::Decimal128(d) => match d.classify() {
            Decimal::Finite { coefficient, exponent, .. } => {
                exponent >= 0 || coefficient % 10u128.pow(exponent.unsigned_abs().min(38)) == 0
            }
            _ => false,
        },
        _ => true,
    };
    whole.then_some(n)
}

/// An `f64` without fraction as an `i64`, if it is in range.
pub(crate) fn f64_to_i64(x: f64) -> Option<i64> {
    // 2^63 is the first double out of range; -2^63 is in range.
    const LIMIT: f64 = 9_223_372_036_854_775_808.0;
    (x.is_finite() && (-LIMIT..LIMIT).contains(&x)).then_some(x as i64)
}

fn decimal_trunc_i64(negative: bool, coefficient: u128, exponent: i32) -> Option<i64> {
    let magnitude = if exponent >= 0 {
        coefficient.checked_mul(10u128.checked_pow(exponent.unsigned_abs())?)?
    } else if exponent < -38 {
        0
    } else {
        coefficient / 10u128.pow(exponent.unsigned_abs())
    };
    if negative {
        0i128.checked_sub(i128::try_from(magnitude).ok()?).and_then(|n| i64::try_from(n).ok())
    } else {
        i64::try_from(magnitude).ok()
    }
}

/// Whether the value is "true" as RefDb reads flags: numbers other than zero, `true`, and
/// every other type except `false`, null and undefined.
pub(crate) fn truthy(v: &RawValue<'_>) -> bool {
    match *v {
        RawValue::Boolean(b) => b,
        RawValue::Null | RawValue::Undefined => false,
        RawValue::Int32(n) => n != 0,
        RawValue::Int64(n) => n != 0,
        RawValue::Double(x) => x != 0.0,
        RawValue::Decimal128(d) => !matches!(d.classify(), Decimal::Finite { coefficient: 0, .. }),
        _ => true,
    }
}

/// A value written for an error message, in relaxed Extended JSON.
pub(crate) fn display(v: RawValue<'_>) -> String {
    match OwnedValue::from_raw(v) {
        Ok(o) => {
            let text = teseladb_bson::extjson::to_relaxed(o.holder());
            // `{"": value}` without its wrapper.
            text.strip_prefix("{\"\":")
                .and_then(|s| s.strip_suffix('}'))
                .map_or(text.clone(), |s| s.trim().to_owned())
        }
        Err(e) => e.message,
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn damaged_bytes_read_as_empty() {
        let v = OwnedValue { bytes: vec![1, 2, 3] };
        assert_eq!(v.get(), RawValue::Null);
        let d = OwnedDoc { bytes: vec![1, 2, 3] };
        assert!(d.get().is_empty());
        assert_eq!(OwnedDoc::new(&Document::new()).unwrap().as_bytes(), [5, 0, 0, 0, 0]);
    }

    #[test]
    fn values_that_are_not_bson_fail() {
        let mut deep = Value::Null;
        for _ in 0..MAX_COMMAND_DEPTH + 2 {
            deep = Value::Array(vec![deep]);
        }
        assert!(OwnedValue::new(deep).is_err());
        // A UUID of another length is stored, as RefDb stores it (measured on 9.0.4).
        assert!(OwnedValue::new(Value::Binary { subtype: 4, bytes: vec![1] }).is_ok());
        assert!(
            display(RawValue::Binary { subtype: 4, bytes: &[1] }).contains("\"subType\":\"04\"")
        );
    }

    #[test]
    fn numbers() {
        let dec = |s: &str| RawValue::Decimal128(s.parse().unwrap());
        assert_eq!(truncate_i64(&dec("-7.9")), Some(-7));
        assert_eq!(truncate_i64(&dec("1E+3")), Some(1000));
        assert_eq!(truncate_i64(&dec("1E-40")), Some(0));
        assert_eq!(truncate_i64(&dec("1E+30")), None);
        assert_eq!(truncate_i64(&dec("-1E+30")), None);
        assert_eq!(truncate_i64(&dec("NaN")), None);
        assert_eq!(truncate_i64(&RawValue::String("1")), None);
        assert_eq!(exact_i64(&dec("2.0")), Some(2));
        assert_eq!(exact_i64(&dec("2.5")), None);
        assert_eq!(exact_i64(&dec("Infinity")), None);
        assert_eq!(exact_i64(&dec("2E+1")), Some(20));
        assert_eq!(to_f64(&dec("-Infinity")), Some(f64::NEG_INFINITY));
        assert_eq!(to_f64(&RawValue::Int64(3)), Some(3.0));
        assert!(to_f64(&dec("NaN")).is_some_and(f64::is_nan));
        assert!(is_nan(&dec("NaN")));
        assert!(!truthy(&dec("0")) && truthy(&dec("1")) && !truthy(&RawValue::Undefined));
        assert!(
            truthy(&RawValue::Int64(1))
                && !truthy(&RawValue::Double(0.0))
                && truthy(&RawValue::MinKey)
        );
        assert_eq!(f64_to_i64(9.3e18), None);
    }
}
