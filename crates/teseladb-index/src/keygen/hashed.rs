//! Keys of a hashed index (FR-014): the same 64-bit integer RefDb gives a value, so that a hashed
//! shard key and `$toHashedIndexKey` agree with it. RefDb does not document the function and its
//! source is not consulted (constitution, principle V): it was measured on a real server with
//! `convertShardKeyToHashed` (009 research R5, vectors in `tests/data/hashed-vectors.json`).
//!
//! The key is the first 8 bytes, little-endian, of the MD5 of a seed (`i32` 0) followed by the
//! value: its type class (`i32`) and its bytes. Numbers hash by their integer value (`i64`): a
//! double truncated, a decimal rounded half to even, both saturating, NaN as 0. Strings and
//! symbols are a length with the bytes and a zero; documents and arrays list each field as type
//! class, name and bytes, and end with a 0 class; code with scope gives its code and its scope's
//! fields.

use teseladb_bson::{Decimal, RawDoc, RawValue};
use teseladb_hash::md5;

/// The seed RefDb hashes with.
const SEED: i32 = 0;

/// Type classes of the hash.
const MIN_KEY: i32 = -1;
const UNDEFINED: i32 = 0;
const NULL: i32 = 5;
const NUMBER: i32 = 10;
const STRING: i32 = 15;
const DOCUMENT: i32 = 20;
const ARRAY: i32 = 25;
const BINARY: i32 = 30;
const OBJECT_ID: i32 = 35;
const BOOLEAN: i32 = 40;
const DATE: i32 = 45;
const TIMESTAMP: i32 = 47;
const REGEX: i32 = 50;
const DB_POINTER: i32 = 55;
const CODE: i32 = 60;
const CODE_WITH_SCOPE: i32 = 65;
const MAX_KEY: i32 = 127;
/// Binary subtype 2, stored with an inner length.
const OLD_BINARY: u8 = 2;
/// Class that ends a document.
const END: i32 = 0;

/// 2⁶³ as a double: the first double above every `i64`.
const TWO_63: f64 = 9_223_372_036_854_775_808.0;

/// The hashed index key of `v`.
pub fn hashed_key(v: RawValue<'_>) -> i64 {
    let mut out = SEED.to_le_bytes().to_vec();
    value(&mut out, v, None);
    let d = md5(&out);
    i64::from_le_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
}

/// A double as an `i64`: truncated; NaN is 0, above 2⁶³ the largest, below −2⁶³ the smallest, and
/// 2⁶³ itself the smallest (as measured).
fn double_to_i64(d: f64) -> i64 {
    if d.is_nan() {
        0
    } else if d > TWO_63 {
        i64::MAX
    } else if d == TWO_63 {
        i64::MIN
    } else {
        // Truncates; below −2⁶³ it saturates at the smallest.
        d as i64
    }
}

/// A decimal as an `i64`: rounded half to even, saturating; NaN is 0.
fn decimal_to_i64(x: Decimal) -> i64 {
    match x {
        Decimal::NaN => 0,
        Decimal::Infinity { negative } => {
            if negative {
                i64::MIN
            } else {
                i64::MAX
            }
        }
        Decimal::Finite { negative, coefficient, exponent } => {
            let magnitude = round_half_even(coefficient, exponent);
            let signed = if negative { -magnitude } else { magnitude };
            i64::try_from(signed).unwrap_or(if negative { i64::MIN } else { i64::MAX })
        }
    }
}

/// `coefficient × 10^exponent` rounded half to even, capped well above any `i64`.
fn round_half_even(coefficient: u128, exponent: i32) -> i128 {
    const CAP: u128 = 1 << 100;
    if coefficient == 0 {
        return 0;
    }
    if exponent >= 0 {
        let mut v = coefficient;
        for _ in 0..exponent {
            if v > CAP {
                return CAP as i128;
            }
            v *= 10;
        }
        return v.min(CAP) as i128;
    }
    let digits = exponent.unsigned_abs();
    // Past 38 digits the divisor overflows; the coefficient has at most 34, so the value is
    // below 0.5.
    let Some(divisor) = 10u128.checked_pow(digits) else { return 0 };
    let (q, r) = (coefficient / divisor, coefficient % divisor);
    let twice = r * 2;
    let up = twice > divisor || (twice == divisor && q % 2 == 1);
    (q + u128::from(up)) as i128
}

fn class(out: &mut Vec<u8>, c: i32) {
    out.extend_from_slice(&c.to_le_bytes());
}

fn string(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as i32 + 1).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

fn cstring(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

/// The fields of a document or an array and the class that ends them.
fn fields(out: &mut Vec<u8>, d: RawDoc<'_>) {
    for (name, v) in d.iter() {
        value(out, v, Some(name));
    }
    class(out, END);
}

/// A value's class, its name inside a document, and its bytes.
fn value(out: &mut Vec<u8>, v: RawValue<'_>, name: Option<&str>) {
    let c = match v {
        RawValue::MinKey => MIN_KEY,
        RawValue::MaxKey => MAX_KEY,
        RawValue::Undefined => UNDEFINED,
        RawValue::Null => NULL,
        RawValue::Int32(_) | RawValue::Int64(_) | RawValue::Double(_) | RawValue::Decimal128(_) => {
            NUMBER
        }
        RawValue::String(_) | RawValue::Symbol(_) => STRING,
        RawValue::Document(_) => DOCUMENT,
        RawValue::Array(_) => ARRAY,
        RawValue::Binary { .. } => BINARY,
        RawValue::ObjectId(_) => OBJECT_ID,
        RawValue::Boolean(_) => BOOLEAN,
        RawValue::DateTime(_) => DATE,
        RawValue::Timestamp { .. } => TIMESTAMP,
        RawValue::Regex { .. } => REGEX,
        RawValue::DbPointer { .. } => DB_POINTER,
        RawValue::JavaScript(_) => CODE,
        RawValue::JavaScriptWithScope { .. } => CODE_WITH_SCOPE,
    };
    class(out, c);
    if let Some(n) = name {
        cstring(out, n);
    }
    match v {
        RawValue::MinKey | RawValue::MaxKey | RawValue::Undefined | RawValue::Null => {}
        RawValue::Int32(i) => out.extend_from_slice(&i64::from(i).to_le_bytes()),
        RawValue::Int64(i) => out.extend_from_slice(&i.to_le_bytes()),
        RawValue::Double(d) => out.extend_from_slice(&double_to_i64(d).to_le_bytes()),
        RawValue::Decimal128(x) => {
            out.extend_from_slice(&decimal_to_i64(x.classify()).to_le_bytes());
        }
        RawValue::String(s) | RawValue::Symbol(s) | RawValue::JavaScript(s) => string(out, s),
        RawValue::Document(d) => fields(out, d),
        RawValue::Array(a) => fields(out, a.as_doc()),
        // As stored: subtype 2 keeps its inner length (as measured).
        RawValue::Binary { subtype: OLD_BINARY, bytes } => {
            out.extend_from_slice(&(bytes.len() as i32 + 4).to_le_bytes());
            out.push(OLD_BINARY);
            out.extend_from_slice(&(bytes.len() as i32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        RawValue::Binary { subtype, bytes } => {
            out.extend_from_slice(&(bytes.len() as i32).to_le_bytes());
            out.push(subtype);
            out.extend_from_slice(bytes);
        }
        RawValue::ObjectId(id) => out.extend_from_slice(&id),
        RawValue::Boolean(b) => out.push(u8::from(b)),
        RawValue::DateTime(ms) => out.extend_from_slice(&ms.to_le_bytes()),
        RawValue::Timestamp { time, increment } => {
            out.extend_from_slice(&((u64::from(time) << 32) | u64::from(increment)).to_le_bytes());
        }
        RawValue::Regex { pattern, options } => {
            cstring(out, pattern);
            cstring(out, options);
        }
        RawValue::DbPointer { namespace, id } => {
            string(out, namespace);
            out.extend_from_slice(&id);
        }
        RawValue::JavaScriptWithScope { code, scope } => {
            cstring(out, code);
            fields(out, scope);
        }
    }
}
