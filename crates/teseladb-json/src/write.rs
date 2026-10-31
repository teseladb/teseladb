//! Compact serializer.

use std::fmt;
use std::fmt::Write as _;

use crate::value::{Number, Value};

/// A value that cannot be written as JSON.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteError {
    /// Non-finite `Number::Float` (NaN or infinity).
    NonFinite,
    /// `Number::Big` whose text is not a JSON integer.
    InvalidNumber,
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            WriteError::NonFinite => "non-finite number",
            WriteError::InvalidNumber => "invalid integer number",
        })
    }
}

impl std::error::Error for WriteError {}

/// Compact JSON of a value.
pub fn to_string(v: &Value) -> Result<String, WriteError> {
    let mut out = String::new();
    write_into(v, &mut out)?;
    Ok(out)
}

fn valid_integer(s: &str) -> bool {
    let digits = s.strip_prefix('-').unwrap_or(s);
    let b = digits.as_bytes();
    !b.is_empty() && b.iter().all(u8::is_ascii_digit) && (b[0] != b'0' || b.len() == 1)
}

/// Appends the compact JSON of a value to the end of `out`.
pub fn write_into(v: &Value, out: &mut String) -> Result<(), WriteError> {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(Number::Int(n)) => write!(out, "{n}").expect("String"),
        Value::Number(Number::UInt(n)) => write!(out, "{n}").expect("String"),
        Value::Number(Number::Float(f)) => {
            if !f.is_finite() {
                return Err(WriteError::NonFinite);
            }
            // `Debug` gives the shortest representation that reads back identically and always
            // has a point or an exponent (so it reads back as `Float`).
            write!(out, "{f:?}").expect("String");
        }
        Value::Number(Number::Big(s)) => {
            if !valid_integer(s) {
                return Err(WriteError::InvalidNumber);
            }
            out.push_str(s);
        }
        Value::String(s) => write_str(s, out),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_into(x, out)?;
            }
            out.push(']');
        }
        Value::Object(o) => {
            out.push('{');
            for (i, (k, x)) in o.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_str(k, out);
                out.push(':');
                write_into(x, out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

fn write_str(s: &str, out: &mut String) {
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
            c if (c as u32) < 0x20 => write!(out, "\\u{:04x}", c as u32).expect("String"),
            c => out.push(c),
        }
    }
    out.push('"');
}
