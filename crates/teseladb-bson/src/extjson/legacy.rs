//! The legacy strict JSON that RefDb writes in its audit log (013 FR-016): `{ "k" : v }` with
//! spaces, 32-bit integers and doubles as plain numbers, `$numberLong`, `$binary` with `$type`,
//! `$regex` with `$options`, and dates as `+00:00` ISO text from 1970 to 9999. Measured on
//! RefDb Enterprise 9.0.4's `--auditFormat JSON`.

use super::{base64, date};
use crate::raw::{RawDoc, RawValue};

pub(crate) fn document(out: &mut String, d: RawDoc<'_>) {
    let mut empty = true;
    for (k, v) in d.iter() {
        out.push_str(if empty { "{ " } else { ", " });
        empty = false;
        string(out, k);
        out.push_str(" : ");
        value(out, v);
    }
    out.push_str(if empty { "{}" } else { " }" });
}

fn value(out: &mut String, v: RawValue<'_>) {
    use RawValue as V;
    match v {
        V::Double(x) => out.push_str(&double(x)),
        V::String(s) => string(out, s),
        V::Document(d) => document(out, d),
        V::Array(a) => {
            let mut empty = true;
            for item in a.iter() {
                out.push_str(if empty { "[ " } else { ", " });
                empty = false;
                value(out, item);
            }
            out.push_str(if empty { "[]" } else { " ]" });
        }
        V::Binary { subtype, bytes } => {
            out.push_str("{ \"$binary\" : ");
            string(out, &base64::encode(bytes));
            out.push_str(&format!(", \"$type\" : \"{subtype:02x}\" }}"));
        }
        V::Undefined => out.push_str("{ \"$undefined\" : true }"),
        V::ObjectId(id) => wrapped(out, "$oid", |o| string(o, &hex(&id))),
        V::Boolean(b) => out.push_str(if b { "true" } else { "false" }),
        V::DateTime(ms) => match date::to_log_time(ms) {
            Some(text) => wrapped(out, "$date", |o| string(o, &text)),
            None => wrapped(out, "$date", |o| {
                wrapped(o, "$numberLong", |o| string(o, &ms.to_string()));
            }),
        },
        V::Null => out.push_str("null"),
        V::Regex { pattern, options } => {
            out.push_str("{ \"$regex\" : ");
            string(out, pattern);
            out.push_str(", \"$options\" : ");
            string(out, options);
            out.push_str(" }");
        }
        V::DbPointer { namespace, id } => {
            out.push_str("{ \"$ref\" : ");
            string(out, namespace);
            out.push_str(", \"$id\" : ");
            string(out, &hex(&id));
            out.push_str(" }");
        }
        V::JavaScript(c) => wrapped(out, "$code", |o| string(o, c)),
        V::Symbol(s) => wrapped(out, "$symbol", |o| string(o, s)),
        V::JavaScriptWithScope { code, scope } => {
            out.push_str("{ \"$code\" : ");
            string(out, code);
            out.push_str(", \"$scope\" : ");
            document(out, scope);
            out.push_str(" }");
        }
        V::Int32(n) => out.push_str(&n.to_string()),
        V::Timestamp { time, increment } => {
            out.push_str(&format!(
                "{{ \"$timestamp\" : {{ \"t\" : {time}, \"i\" : {increment} }} }}"
            ));
        }
        V::Int64(n) => wrapped(out, "$numberLong", |o| string(o, &n.to_string())),
        V::Decimal128(d) => wrapped(out, "$numberDecimal", |o| string(o, &d.to_string())),
        V::MinKey => out.push_str("{ \"$minKey\" : 1 }"),
        V::MaxKey => out.push_str("{ \"$maxKey\" : 1 }"),
    }
}

/// `{ "key" : … }` with the inner value written by `inner`.
fn wrapped(out: &mut String, key: &str, inner: impl FnOnce(&mut String)) {
    out.push_str("{ ");
    string(out, key);
    out.push_str(" : ");
    inner(out);
    out.push_str(" }");
}

/// A double as RefDb writes it there: the shorter of its shortest plain and scientific forms
/// (the plain one on a tie), the exponent with a sign and at least two digits: `0.05`, `2`,
/// `-0`, `1e+300`, `1.5e-07`; `NaN`, `Infinity` and `-Infinity` bare.
pub(crate) fn double(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_owned();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    let plain = format!("{x}");
    let sci = format!("{x:e}");
    let sci = match sci.split_once('e') {
        Some((mantissa, exp)) => {
            let (sign, digits) = exp.strip_prefix('-').map_or(("+", exp), |d| ("-", d));
            format!("{mantissa}e{sign}{digits:0>2}")
        }
        None => sci,
    };
    if sci.len() < plain.len() { sci } else { plain }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
}
