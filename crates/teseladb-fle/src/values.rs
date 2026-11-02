//! Small helpers over BSON values: comparison in RefDb's order and integral decimals.

use std::cmp::Ordering;

use teseladb_bson::{Decimal, Decimal128, Document, RawDoc, Value};

/// Compares two values in RefDb's order (binary comparison of strings).
pub fn compare(a: &Value, b: &Value) -> Ordering {
    // Two values in one document; one that cannot be written compares equal.
    let doc = Document::new().with("a", a.clone()).with("b", b.clone());
    doc.to_bytes()
        .ok()
        .and_then(|bytes| {
            let raw = RawDoc::from_bytes(&bytes).ok()?;
            Some(teseladb_bson::compare(raw.get("a")?, raw.get("b")?, None))
        })
        .unwrap_or(Ordering::Equal)
}

/// The integer a decimal is worth, if it is integral and fits.
pub fn decimal_to_i64(d: Decimal128) -> Option<i64> {
    let Decimal::Finite { negative, coefficient, exponent } = d.classify() else { return None };
    let mut c = i128::try_from(coefficient).ok()?;
    let mut e = exponent;
    while e < 0 {
        if c % 10 != 0 {
            return None;
        }
        c /= 10;
        e += 1;
    }
    while e > 0 {
        c = c.checked_mul(10)?;
        e -= 1;
    }
    i64::try_from(if negative { -c } else { c }).ok()
}

/// A double of a decimal, for the arithmetic of a domain's size.
pub fn decimal_to_f64(d: Decimal128) -> Option<f64> {
    let Decimal::Finite { negative, coefficient, exponent } = d.classify() else { return None };
    let v = coefficient as f64 * 10f64.powi(exponent);
    Some(if negative { -v } else { v })
}
