//! Writing canonical BSON: arrays with their positions as names, regular expression options in
//! alphabetical order and the inner length of subtype 2. What would make invalid bytes is refused:
//! a name or a regular expression with a `00` byte, more bytes or more levels than the limits
//! ([`crate::MAX_SIZE`] and [`crate::MAX_DEPTH`] for a stored document).

use crate::Limits;
use crate::error::{BsonError, invalid};
use crate::types;
use crate::value::{Document, Value};

impl Document {
    /// The document in canonical BSON, within the limits of a stored document.
    pub fn to_bytes(&self) -> Result<Vec<u8>, BsonError> {
        self.to_bytes_with(Limits::default())
    }

    /// The document in canonical BSON within `limits` (a command reply may exceed a stored
    /// document's).
    pub fn to_bytes_with(&self, limits: Limits) -> Result<Vec<u8>, BsonError> {
        let mut out = Vec::new();
        write_document(&mut out, self.iter(), 1, limits)?;
        if out.len() > limits.max_size {
            return Err(BsonError::TooLarge { size: out.len(), max: limits.max_size });
        }
        Ok(out)
    }
}

/// Refuses a string or binary value of `len` bytes past the limit before it is written, so that
/// every length written fits in BSON's `i32`.
fn within(len: usize, limits: Limits) -> Result<i32, BsonError> {
    match i32::try_from(len) {
        Ok(n) if len <= limits.max_size => Ok(n),
        _ => Err(BsonError::TooLarge { size: len, max: limits.max_size }),
    }
}

/// A document of `fields` at nesting level `depth`.
fn write_document<'a>(
    out: &mut Vec<u8>,
    fields: impl Iterator<Item = (&'a str, &'a Value)>,
    depth: usize,
    limits: Limits,
) -> Result<(), BsonError> {
    if depth > limits.max_depth {
        return Err(BsonError::TooDeep { offset: out.len(), path: String::new() });
    }
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    for (key, value) in fields {
        out.push(value.element_type());
        cstring(out, key)?;
        write_value(out, value, depth, limits).map_err(|e| e.under(key))?;
    }
    out.push(0);
    patch_length(out, start);
    Ok(())
}

fn write_value(
    out: &mut Vec<u8>,
    value: &Value,
    depth: usize,
    limits: Limits,
) -> Result<(), BsonError> {
    match value {
        Value::Double(x) => out.extend_from_slice(&x.to_le_bytes()),
        Value::String(s) | Value::JavaScript(s) | Value::Symbol(s) => string(out, s, limits)?,
        Value::Document(d) => write_document(out, d.iter(), depth + 1, limits)?,
        Value::Array(items) => {
            let names: Vec<String> = (0..items.len()).map(|i| i.to_string()).collect();
            let fields = names.iter().map(String::as_str).zip(items);
            write_document(out, fields, depth + 1, limits)?;
        }
        Value::Binary { subtype, bytes } => {
            let old = *subtype == types::BINARY_OLD;
            let len = within(bytes.len() + if old { 4 } else { 0 }, limits)?;
            out.extend_from_slice(&len.to_le_bytes());
            out.push(*subtype);
            if old {
                out.extend_from_slice(&(len - 4).to_le_bytes());
            }
            out.extend_from_slice(bytes);
        }
        Value::Undefined | Value::Null | Value::MinKey | Value::MaxKey => {}
        Value::ObjectId(id) => out.extend_from_slice(id),
        Value::Boolean(b) => out.push(u8::from(*b)),
        Value::DateTime(ms) => out.extend_from_slice(&ms.to_le_bytes()),
        Value::Regex { pattern, options } => {
            cstring(out, pattern)?;
            let mut sorted: Vec<char> = options.chars().collect();
            sorted.sort_unstable();
            cstring(out, &sorted.into_iter().collect::<String>())?;
        }
        Value::DbPointer { namespace, id } => {
            string(out, namespace, limits)?;
            out.extend_from_slice(id);
        }
        Value::JavaScriptWithScope { code, scope } => {
            let start = out.len();
            out.extend_from_slice(&[0; 4]);
            string(out, code, limits)?;
            write_document(out, scope.iter(), depth + 1, limits)?;
            patch_length(out, start);
        }
        Value::Int32(n) => out.extend_from_slice(&n.to_le_bytes()),
        Value::Timestamp { time, increment } => {
            out.extend_from_slice(&increment.to_le_bytes());
            out.extend_from_slice(&time.to_le_bytes());
        }
        Value::Int64(n) => out.extend_from_slice(&n.to_le_bytes()),
        Value::Decimal128(d) => out.extend_from_slice(&d.to_le_bytes()),
    }
    Ok(())
}

/// A name or a regular expression part: UTF-8 without `00`, then `00`.
fn cstring(out: &mut Vec<u8>, s: &str) -> Result<(), BsonError> {
    if s.as_bytes().contains(&0) {
        return Err(invalid(out.len(), "name or regular expression with a 00 byte"));
    }
    out.extend_from_slice(s.as_bytes());
    out.push(0);
    Ok(())
}

/// A string: length counting its terminator, UTF-8 bytes (`00` allowed inside) and `00`.
fn string(out: &mut Vec<u8>, s: &str, limits: Limits) -> Result<(), BsonError> {
    out.extend_from_slice(&within(s.len() + 1, limits)?.to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    out.push(0);
    Ok(())
}

/// Writes the length of what starts at `start` (an `i32` placeholder) up to the end.
fn patch_length(out: &mut [u8], start: usize) {
    let len = (out.len() - start) as i32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
}
