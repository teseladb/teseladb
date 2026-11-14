//! Values written the way the server writes them inside error messages (`point not in interval of [
//! -180, 180 ] :: caused by :: { _id: 1, loc: [ 200, 2 ] }`, the `_id` of a document whose geometry
//! cannot be indexed, the rings of an invalid polygon). Measured on the reference server 9.0.4:
//! doubles with 16 significant digits and a `.0` when they read as an integer (`2.0`, `-0.0`,
//! `1000000000000000.0`), never after an exponent, `nan` or `inf` (7.0.45 wrote `1e+18.0`, `nan.0`
//! and `2`); strings and symbols in double quotes without escapes; documents as `{ a: 1 }` and
//! arrays as `[ 1, 2 ]`. Not measured (the reference shell cannot send them): `undefined` and
//! `DBPointer`.

use std::fmt::Write;

use crate::{RawDoc, RawValue};

/// Binary subtype of a UUID.
const UUID_SUBTYPE: u8 = 4;
/// Length of a UUID.
const UUID_LEN: usize = 16;
/// Significant digits of a double that is not printed as an integer.
const DIGITS: i32 = 16;

/// `v` as the server writes it.
#[must_use]
pub fn shell_value(v: RawValue<'_>) -> String {
    let mut out = String::new();
    write_value(&mut out, v);
    out
}

/// A document as the server writes it.
#[must_use]
pub fn shell_doc(d: RawDoc<'_>) -> String {
    shell_value(RawValue::Document(d))
}

fn write_value(out: &mut String, v: RawValue<'_>) {
    // Writing to a `String` cannot fail.
    let _ = match v {
        RawValue::Double(d) => write!(out, "{}", double(d)),
        RawValue::String(s) | RawValue::Symbol(s) => write!(out, "\"{s}\""),
        RawValue::Document(d) => {
            write_list(out, '{', '}', d.iter().map(|(k, v)| (Some(k), v)));
            Ok(())
        }
        RawValue::Array(a) => {
            write_list(out, '[', ']', a.iter().map(|v| (None, v)));
            Ok(())
        }
        RawValue::Binary { subtype, bytes }
            if subtype == UUID_SUBTYPE && bytes.len() == UUID_LEN =>
        {
            let h = hex(bytes, false);
            write!(
                out,
                "UUID(\"{}-{}-{}-{}-{}\")",
                &h[..8],
                &h[8..12],
                &h[12..16],
                &h[16..20],
                &h[20..]
            )
        }
        RawValue::Binary { subtype, bytes } => {
            write!(out, "BinData({subtype}, {})", hex(bytes, true))
        }
        RawValue::Undefined => write!(out, "undefined"),
        RawValue::ObjectId(id) => write!(out, "ObjectId('{}')", hex(&id, false)),
        RawValue::Boolean(b) => write!(out, "{b}"),
        RawValue::DateTime(ms) => write!(out, "new Date({ms})"),
        RawValue::Null => write!(out, "null"),
        RawValue::Regex { pattern, options } => write!(out, "/{pattern}/{options}"),
        RawValue::DbPointer { namespace, id } => {
            write!(out, "DBRef('{namespace}', {})", hex(&id, false))
        }
        RawValue::JavaScript(code) => write!(out, "{code}"),
        RawValue::JavaScriptWithScope { code, scope } => {
            write!(out, "CodeWScope( {code}, {})", shell_doc(scope))
        }
        RawValue::Int32(n) => write!(out, "{n}"),
        RawValue::Timestamp { time, increment } => write!(out, "Timestamp({time}, {increment})"),
        RawValue::Int64(n) => write!(out, "{n}"),
        RawValue::Decimal128(d) => write!(out, "{d}"),
        RawValue::MinKey => write!(out, "MinKey"),
        RawValue::MaxKey => write!(out, "MaxKey"),
    };
}

fn write_list<'a>(
    out: &mut String,
    open: char,
    close: char,
    items: impl Iterator<Item = (Option<&'a str>, RawValue<'a>)>,
) {
    out.push(open);
    let mut empty = true;
    for (i, (name, v)) in items.enumerate() {
        out.push_str(if i == 0 { " " } else { ", " });
        if let Some(name) = name {
            out.push_str(name);
            out.push_str(": ");
        }
        write_value(out, v);
        empty = false;
    }
    if !empty {
        out.push(' ');
    }
    out.push(close);
}

fn hex(bytes: &[u8], upper: bool) -> String {
    bytes.iter().map(|b| if upper { format!("{b:02X}") } else { format!("{b:02x}") }).collect()
}

/// A double as the server writes it: 16 significant digits, and `.0` when that reads as an
/// integer.
#[must_use]
pub fn double(d: f64) -> String {
    let mut s = general(d, DIGITS);
    if !s.contains(|c: char| c == '.' || c.is_ascii_alphabetic()) {
        s.push_str(".0");
    }
    s
}

/// `printf("%.*g", digits, d)`.
#[must_use]
pub fn general(d: f64, digits: i32) -> String {
    if d.is_nan() {
        return "nan".to_string();
    }
    if d.is_infinite() {
        return if d > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    let sci = format!("{:.*e}", usize::try_from(digits - 1).unwrap_or(0), d);
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if exp < -4 || exp >= digits {
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{}e{sign}{:02}", trim(mantissa), exp.abs());
    }
    trim(&format!("{:.*}", usize::try_from(digits - 1 - exp).unwrap_or(0), d)).to_string()
}

/// Without trailing zeros after the point, nor the point itself when nothing follows it.
fn trim(s: &str) -> &str {
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.') } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_as_measured() {
        let cases = [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (0.1, "0.1"),
            (1.5, "1.5"),
            (1e21, "1e+21"),
            (1e-7, "1e-07"),
            (123_456_789_012_345_680_000.0, "1.234567890123457e+20"),
            (2.0 / 3.0, "0.6666666666666666"),
            (f64::NAN, "nan"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (1e300, "1e+300"),
            (0.000_123, "0.000123"),
            (-3.0, "-3.0"),
            (1e18, "1e+18"),
            (9.2e18, "9.2e+18"),
            (-1e18, "-1e+18"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (12_345.678_901_234_567, "12345.67890123457"),
            (5e-324, "4.940656458412465e-324"),
        ];
        for (d, s) in cases {
            assert_eq!(double(d), s, "{d}");
        }
    }
}
