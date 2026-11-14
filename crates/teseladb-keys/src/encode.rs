//! Encoding that keeps the reference server's order in the bytes (FR-008 to FR-010): a class byte
//! per type class and a content whose byte order is the order of FR-005 to FR-007. Every encoding
//! is self-delimiting, so several fields concatenate without ambiguity and a descending field is
//! its bytes inverted.
//!
//! - Numbers, one class for the four types: a sub-class (NaN, −∞, negative, zero, positive, +∞)
//!   and, for the finite ones, the decimal exponent and the exact digits two per byte, ending in
//!   `00`; negatives inverted. The same value gives the same bytes whatever its type.
//! - Strings, symbols, names, code and regular expressions: UTF-8 with `00` written `00 FF`,
//!   ending in `00 00`. In an index with a collation, strings and symbols at any depth are their
//!   sort key instead, written the same way; field names, patterns and code are not collated.
//! - Documents: per field its class byte, its name and its content, ending in `00`; arrays: per
//!   element its class byte and its content, ending in `00`.
//! - Binary data by stored length, subtype and bytes; dates with the sign bit flipped; the rest
//!   in big-endian.

use teseladb_bson::{Collator, NumberParts, RawDoc, RawValue, number_parts, type_class};

use crate::typebits::TypeBits;

/// The class byte of the lowest type class; below it only terminators.
const CLASS_BASE: u8 = 0x10;

/// Sub-classes of a number.
const NAN: u8 = 0x01;
const NEG_INFINITY: u8 = 0x02;
const NEGATIVE: u8 = 0x03;
const ZERO: u8 = 0x04;
const POSITIVE: u8 = 0x05;
const POS_INFINITY: u8 = 0x06;

/// Bias of a number's decimal exponent, written as a big-endian `u16`.
const EXPONENT_BIAS: i32 = 0x8000;

/// Order of a field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Ascending.
    Ascending,
    /// Descending: the field's bytes inverted.
    Descending,
}

/// Appends the encoding of `v` (class byte and content) in direction `dir`, and its types to
/// `bits`.
pub fn encode_value(out: &mut Vec<u8>, v: RawValue<'_>, dir: Direction, bits: &mut TypeBits) {
    encode_value_with(out, v, dir, bits, None);
}

/// As [`encode_value`], with the strings in `collation`'s order if there is one.
pub fn encode_value_with(
    out: &mut Vec<u8>,
    v: RawValue<'_>,
    dir: Direction,
    bits: &mut TypeBits,
    collation: Option<&dyn Collator>,
) {
    let start = out.len();
    out.push(class_byte(&v));
    content(out, v, bits, collation);
    if dir == Direction::Descending {
        for b in &mut out[start..] {
            *b = !*b;
        }
    }
}

/// The encoding of several fields, each in its direction, and their types.
pub fn encode(fields: &[(RawValue<'_>, Direction)]) -> (Vec<u8>, TypeBits) {
    let (mut out, mut bits) = (Vec::new(), TypeBits::default());
    for (v, dir) in fields {
        encode_value(&mut out, *v, *dir, &mut bits);
    }
    (out, bits)
}

/// The class byte of a value.
pub fn class_byte(v: &RawValue<'_>) -> u8 {
    CLASS_BASE + type_class(v)
}

fn content(out: &mut Vec<u8>, v: RawValue<'_>, bits: &mut TypeBits, c: Option<&dyn Collator>) {
    use RawValue as V;
    let whole = match v {
        V::Int32(n) => Some(i64::from(n)),
        V::Int64(n) => Some(n),
        _ => None,
    };
    if let Some(n) = whole {
        bits.number(&v);
        integer(out, n);
        return;
    }
    if let Some(parts) = number_parts(&v) {
        bits.number(&v);
        number(out, parts);
        return;
    }
    match v {
        V::String(s) => {
            bits.string(false);
            collated(out, s, c);
        }
        V::Symbol(s) => {
            bits.string(true);
            collated(out, s, c);
        }
        V::Document(d) => document(out, d, bits, c),
        V::Array(a) => {
            for item in a.iter() {
                out.push(class_byte(&item));
                content(out, item, bits, c);
            }
            out.push(0);
        }
        V::Binary { subtype, bytes } => {
            let stored =
                bytes.len() + if subtype == teseladb_bson::types::BINARY_OLD { 4 } else { 0 };
            out.extend_from_slice(&(stored as u32).to_be_bytes());
            out.push(subtype);
            out.extend_from_slice(bytes);
        }
        V::ObjectId(id) => out.extend_from_slice(&id),
        V::Boolean(b) => out.push(u8::from(b)),
        V::DateTime(ms) => out.extend_from_slice(&((ms as u64) ^ (1 << 63)).to_be_bytes()),
        V::Timestamp { time, increment } => {
            out.extend_from_slice(&time.to_be_bytes());
            out.extend_from_slice(&increment.to_be_bytes());
        }
        V::Regex { pattern, options } => {
            text(out, pattern);
            text(out, options);
        }
        V::DbPointer { namespace, id } => {
            out.extend_from_slice(&(namespace.len() as u32).to_be_bytes());
            out.extend_from_slice(namespace.as_bytes());
            out.extend_from_slice(&id);
        }
        V::JavaScript(code) => text(out, code),
        V::JavaScriptWithScope { code, scope } => {
            text(out, code);
            document(out, scope, bits, c);
        }
        // MinKey, undefined, null and MaxKey: the class byte is the whole value.
        _ => {}
    }
}

/// A document's fields: class byte, name and content of each, then `00`.
fn document(out: &mut Vec<u8>, d: RawDoc<'_>, bits: &mut TypeBits, c: Option<&dyn Collator>) {
    for (name, value) in d.iter() {
        out.push(class_byte(&value));
        text(out, name);
        content(out, value, bits, c);
    }
    out.push(0);
}

/// A string: its sort key in `collation` if there is one, else its UTF-8 bytes.
fn collated(out: &mut Vec<u8>, s: &str, collation: Option<&dyn Collator>) {
    match collation {
        Some(c) => escaped(out, &c.sort_key(s)),
        None => text(out, s),
    }
}

/// UTF-8 bytes with `00` as `00 FF`, ending in `00 00`.
pub(crate) fn text(out: &mut Vec<u8>, s: &str) {
    escaped(out, s.as_bytes());
}

/// Bytes with `00` as `00 FF`, ending in `00 00`: their order is kept and they end unambiguously.
fn escaped(out: &mut Vec<u8>, bytes: &[u8]) {
    if !bytes.contains(&0) {
        out.extend_from_slice(bytes);
        out.extend_from_slice(&[0, 0]);
        return;
    }
    for &b in bytes {
        out.push(b);
        if b == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0, 0]);
}

/// A number's sub-class and, if finite and not zero, its exponent and digits.
fn number(out: &mut Vec<u8>, parts: NumberParts) {
    match parts {
        NumberParts::NaN => out.push(NAN),
        NumberParts::Infinity { negative: true } => out.push(NEG_INFINITY),
        NumberParts::Zero => out.push(ZERO),
        NumberParts::Infinity { negative: false } => out.push(POS_INFINITY),
        NumberParts::Finite { negative, digits, exponent } => {
            finite(out, negative, &digits, exponent);
        }
    }
}

/// A whole number, the same bytes as through its parts but without building them: its digits
/// most significant first without the trailing zeros, and as many integer digits as exponent.
fn integer(out: &mut Vec<u8>, n: i64) {
    if n == 0 {
        out.push(ZERO);
        return;
    }
    let mut m = n.unsigned_abs();
    let mut reversed = [0u8; 20];
    let mut len = 0;
    while m > 0 {
        reversed[len] = (m % 10) as u8;
        m /= 10;
        len += 1;
    }
    let low = reversed.iter().position(|d| *d != 0).expect("not zero");
    let mut digits = [0u8; 20];
    for (i, d) in reversed[low..len].iter().rev().enumerate() {
        digits[i] = *d;
    }
    finite(out, n < 0, &digits[..len - low], len as i32);
}

/// The sub-class, exponent and digits of `±0.d₁…dₙ × 10^exponent`.
fn finite(out: &mut Vec<u8>, negative: bool, digits: &[u8], exponent: i32) {
    out.push(if negative { NEGATIVE } else { POSITIVE });
    let start = out.len();
    out.extend_from_slice(&((exponent + EXPONENT_BIAS) as u16).to_be_bytes());
    // Two digits per byte, 1 to 100, the last one padded with 0; then `00`.
    for pair in digits.chunks(2) {
        out.push(pair[0] * 10 + pair.get(1).copied().unwrap_or(0) + 1);
    }
    out.push(0);
    if negative {
        for b in &mut out[start..] {
            *b = !*b;
        }
    }
}
