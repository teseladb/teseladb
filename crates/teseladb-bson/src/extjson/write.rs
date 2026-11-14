//! Writing Extended JSON v2, canonical (every type kept) or relaxed (numbers and dates as plain
//! JSON where that loses nothing a reader needs).

use super::{base64, date};
use crate::raw::{RawDoc, RawValue};
use std::fmt::Write as _;

/// Which form of Extended JSON.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Canonical,
    Relaxed,
    /// Relaxed, with a UUID (binary subtype 4 of 16 bytes) as `{"$uuid": "8-4-4-4-12"}`, as
    /// RefDb writes the OCSF audit log.
    RelaxedUuid,
}

pub(crate) fn document(out: &mut String, d: RawDoc<'_>, mode: Mode) {
    out.push('{');
    for (i, (k, v)) in d.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        string(out, k);
        out.push(':');
        value(out, v, mode);
    }
    out.push('}');
}

fn value(out: &mut String, v: RawValue<'_>, mode: Mode) {
    use RawValue as V;
    let relaxed = mode != Mode::Canonical;
    match v {
        V::Double(x) if !x.is_finite() => {
            let text = if x.is_nan() {
                "NaN"
            } else if x > 0.0 {
                "Infinity"
            } else {
                "-Infinity"
            };
            wrapped(out, "$numberDouble", |o| string(o, text));
        }
        V::Double(x) if relaxed => out.push_str(&double(x)),
        V::Double(x) => wrapped(out, "$numberDouble", |o| string(o, &double(x))),
        V::String(s) => string(out, s),
        V::Document(d) => document(out, d, mode),
        V::Array(a) => {
            out.push('[');
            for (i, item) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                value(out, item, mode);
            }
            out.push(']');
        }
        V::Binary { subtype: 4, bytes } if mode == Mode::RelaxedUuid && bytes.len() == 16 => {
            wrapped(out, "$uuid", |o| string(o, &uuid_text(bytes)));
        }
        V::Binary { subtype, bytes } => wrapped(out, "$binary", |o| {
            o.push_str("{\"base64\":");
            string(o, &base64::encode(bytes));
            let _ = write!(o, ",\"subType\":\"{subtype:02x}\"}}");
        }),
        V::Undefined => out.push_str("{\"$undefined\":true}"),
        V::ObjectId(id) => wrapped(out, "$oid", |o| string(o, &hex(&id))),
        V::Boolean(b) => out.push_str(if b { "true" } else { "false" }),
        V::DateTime(ms) => match date::to_iso(ms).filter(|_| relaxed) {
            Some(iso) => wrapped(out, "$date", |o| string(o, &iso)),
            None => wrapped(out, "$date", |o| {
                wrapped(o, "$numberLong", |o| string(o, &ms.to_string()));
            }),
        },
        V::Null => out.push_str("null"),
        V::Regex { pattern, options } => wrapped(out, "$regularExpression", |o| {
            o.push_str("{\"pattern\":");
            string(o, pattern);
            o.push_str(",\"options\":");
            let mut sorted: Vec<char> = options.chars().collect();
            sorted.sort_unstable();
            string(o, &sorted.into_iter().collect::<String>());
            o.push('}');
        }),
        V::DbPointer { namespace, id } => wrapped(out, "$dbPointer", |o| {
            o.push_str("{\"$ref\":");
            string(o, namespace);
            o.push_str(",\"$id\":");
            wrapped(o, "$oid", |o| string(o, &hex(&id)));
            o.push('}');
        }),
        V::JavaScript(c) => wrapped(out, "$code", |o| string(o, c)),
        V::Symbol(s) => wrapped(out, "$symbol", |o| string(o, s)),
        V::JavaScriptWithScope { code, scope } => {
            out.push_str("{\"$code\":");
            string(out, code);
            out.push_str(",\"$scope\":");
            document(out, scope, mode);
            out.push('}');
        }
        V::Int32(n) if relaxed => out.push_str(&n.to_string()),
        V::Int32(n) => wrapped(out, "$numberInt", |o| string(o, &n.to_string())),
        V::Timestamp { time, increment } => {
            let _ = write!(out, "{{\"$timestamp\":{{\"t\":{time},\"i\":{increment}}}}}");
        }
        V::Int64(n) if relaxed => out.push_str(&n.to_string()),
        V::Int64(n) => wrapped(out, "$numberLong", |o| string(o, &n.to_string())),
        V::Decimal128(d) => wrapped(out, "$numberDecimal", |o| string(o, &d.to_string())),
        V::MinKey => out.push_str("{\"$minKey\":1}"),
        V::MaxKey => out.push_str("{\"$maxKey\":1}"),
    }
}

/// `{"key":…}` with the inner value written by `inner`.
fn wrapped(out: &mut String, key: &str, inner: impl FnOnce(&mut String)) {
    out.push('{');
    string(out, key);
    out.push(':');
    inner(out);
    out.push('}');
}

/// A finite double in its shortest form that reads back the same, always with a fraction or an
/// exponent: `1.0`, `0.1`, `1.2345678921232E+18`, `-0.0`.
pub(crate) fn double(x: f64) -> String {
    let text = format!("{x:?}");
    match text.split_once('e') {
        None => text,
        Some((mantissa, exp)) => {
            let mantissa =
                if mantissa.contains('.') { mantissa.to_owned() } else { format!("{mantissa}.0") };
            let sign = if exp.starts_with('-') { "" } else { "+" };
            format!("{mantissa}E{sign}{exp}")
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// A JSON string, escaping quotes, backslashes and control characters.
fn string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A UUID's 16 bytes as `8-4-4-4-12` lowercase hexadecimal digits.
pub(crate) fn uuid_text(bytes: &[u8]) -> String {
    let hex = self::hex(bytes);
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}
