//! Reading BSON 1.1 (bsonspec.org): one element reader that either validates as it goes (lengths,
//! terminators, types, booleans, UTF-8, the old binary's inner length, nesting, bytes left over;
//! FR-002, FR-003) or, over a document already validated, only follows the lengths. Offsets in
//! errors are from the start of the bytes given to [`crate::RawDoc::from_bytes`].
//!
//! A `refrouter` 9.0.4 checks less (measured, 003 T009): it takes strings and names that are not
//! UTF-8 and an old binary whose two lengths differ. It also checks something the specification
//! leaves open: the layout of each BSON column (binary subtype 7, [`crate::column`], 003 T029).
//! [`validate_message`] checks what RefDb checks, in the same order, so that a server answers a
//! message as RefDb does before it applies the rules of the specification.

use crate::decimal128::Decimal128;
use crate::error::{BsonError, invalid};
use crate::raw::{RawArray, RawDoc, RawValue};
use crate::types;

/// Limits a document is read with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Maximum size in bytes.
    pub max_size: usize,
    /// Maximum nested levels, the top document being the first.
    pub max_depth: usize,
}

impl Default for Limits {
    /// The limits of a stored document: [`crate::MAX_SIZE`] and [`crate::MAX_DEPTH`].
    fn default() -> Limits {
        Limits { max_size: crate::MAX_SIZE, max_depth: crate::MAX_DEPTH }
    }
}

/// Whether the reader validates, and at what depth it is.
#[derive(Clone, Copy)]
pub(crate) enum Mode {
    /// Validating, at `depth` of at most `max`; `spec` adds the checks of the BSON specification
    /// RefDb does not make (UTF-8, the old binary's inner length); `column` is set inside the
    /// values of a BSON column, where another column is refused.
    Check { depth: usize, max: usize, spec: bool, column: bool },
    /// Over bytes already validated.
    Trusted,
}

impl Mode {
    /// Whether the checks of the specification apply (over validated bytes they hold already).
    fn spec(self) -> bool {
        match self {
            Mode::Check { spec, .. } => spec,
            Mode::Trusted => true,
        }
    }
}

/// Validates `bytes` as one whole document within `limits`.
pub(crate) fn validate(bytes: &[u8], limits: Limits) -> Result<(), BsonError> {
    whole(bytes, limits, true)
}

/// Validates `bytes` as RefDb validates each document of a message: lengths, terminators,
/// types, booleans, `limits` and bytes left over, stopping at the first fault in document order;
/// strings and names that are not UTF-8 and an old binary whose two lengths differ pass, as a
/// `refrouter` 9.0.4 lets them pass (003 T009), and a BSON column has to follow the column layout
/// ([`crate::column::validate`], [`BsonError::InvalidColumn`], 003 T029).
pub fn validate_message(bytes: &[u8], limits: Limits) -> Result<(), BsonError> {
    whole(bytes, limits, false)
}

/// The first element of the document `bytes`, if it reads within `limits` (with every check);
/// what follows it is not looked at. A server names a message by its `_id` this way.
pub fn first_element(bytes: &[u8], limits: Limits) -> Option<(&str, RawValue<'_>)> {
    let end = length(bytes, 0, 5).ok()?;
    let b = &bytes[..end];
    if b[4] == 0 {
        return None;
    }
    let mode = Mode::Check { depth: 2, max: limits.max_depth, spec: true, column: false };
    element(b, 4, mode).ok().map(|(key, value, _)| (key, value))
}

fn whole(bytes: &[u8], limits: Limits, spec: bool) -> Result<(), BsonError> {
    if bytes.len() > limits.max_size {
        return Err(BsonError::TooLarge { size: bytes.len(), max: limits.max_size });
    }
    let top = Mode::Check { depth: 1, max: limits.max_depth, spec, column: false };
    let end = document(bytes, 0, top)?;
    if end != bytes.len() {
        return Err(invalid(end, "bytes after the end of the document"));
    }
    Ok(())
}

/// The document that starts at `at`: validates it in `Check` mode, and returns where it ends.
pub(crate) fn document(b: &[u8], at: usize, mode: Mode) -> Result<usize, BsonError> {
    let end = at + length(b, at, 5)?;
    let Mode::Check { depth, max, spec, column } = mode else {
        return Ok(end);
    };
    if depth > max {
        return Err(BsonError::TooDeep { offset: at, path: String::new() });
    }
    let inner = Mode::Check { depth: depth + 1, max, spec, column };
    let mut pos = at + 4;
    loop {
        if pos >= end {
            return Err(invalid(pos, "document without its terminator"));
        }
        if b[pos] == 0 {
            if pos + 1 != end {
                return Err(invalid(pos, "terminator before the end of the document"));
            }
            return Ok(end);
        }
        pos = element(&b[..end], pos, inner)?.2;
    }
}

/// The element at `at`: its name, its value and where the next one starts.
pub(crate) fn element(
    b: &[u8],
    at: usize,
    mode: Mode,
) -> Result<(&str, RawValue<'_>, usize), BsonError> {
    let t = b[at];
    let (key, mut p) = cstring(b, at + 1, mode)?;
    let value = match t {
        types::DOUBLE => RawValue::Double(f64::from_le_bytes(fixed(b, &mut p)?)),
        types::STRING => RawValue::String(string(b, &mut p, mode)?),
        types::DOCUMENT | types::ARRAY => {
            let end = document(b, p, mode).map_err(|e| e.under(key))?;
            let doc = RawDoc::trusted(&b[p..end]);
            p = end;
            if t == types::DOCUMENT {
                RawValue::Document(doc)
            } else {
                RawValue::Array(RawArray(doc))
            }
        }
        types::BINARY => binary(b, &mut p, mode).map_err(|e| e.under(key))?,
        types::UNDEFINED => RawValue::Undefined,
        types::OBJECT_ID => RawValue::ObjectId(fixed(b, &mut p)?),
        types::BOOLEAN => match fixed::<1>(b, &mut p)? {
            [0] => RawValue::Boolean(false),
            [1] => RawValue::Boolean(true),
            _ => return Err(invalid(p - 1, "boolean other than 0 or 1")),
        },
        types::DATE_TIME => RawValue::DateTime(i64::from_le_bytes(fixed(b, &mut p)?)),
        types::NULL => RawValue::Null,
        types::REGEX => {
            let (pattern, q) = cstring(b, p, mode)?;
            let (options, q) = cstring(b, q, mode)?;
            p = q;
            RawValue::Regex { pattern, options }
        }
        types::DB_POINTER => {
            let namespace = string(b, &mut p, mode)?;
            RawValue::DbPointer { namespace, id: fixed(b, &mut p)? }
        }
        types::JAVASCRIPT => RawValue::JavaScript(string(b, &mut p, mode)?),
        types::SYMBOL => RawValue::Symbol(string(b, &mut p, mode)?),
        types::JAVASCRIPT_WITH_SCOPE => {
            code_with_scope(b, &mut p, mode).map_err(|e| e.under(key))?
        }
        types::INT32 => RawValue::Int32(i32::from_le_bytes(fixed(b, &mut p)?)),
        types::TIMESTAMP => {
            let increment = u32::from_le_bytes(fixed(b, &mut p)?);
            RawValue::Timestamp { time: u32::from_le_bytes(fixed(b, &mut p)?), increment }
        }
        types::INT64 => RawValue::Int64(i64::from_le_bytes(fixed(b, &mut p)?)),
        types::DECIMAL128 => RawValue::Decimal128(Decimal128::from_le_bytes(fixed(b, &mut p)?)),
        types::MIN_KEY => RawValue::MinKey,
        types::MAX_KEY => RawValue::MaxKey,
        _ => return Err(invalid(at, "unknown element type")),
    };
    Ok((key, value, p))
}

/// A little-endian `i32` length at `at`, at least `min` and within `b`.
fn length(b: &[u8], at: usize, min: usize) -> Result<usize, BsonError> {
    let mut p = at;
    let n = i32::from_le_bytes(fixed(b, &mut p)?);
    match usize::try_from(n) {
        Ok(n) if n >= min && n <= b.len() - at => Ok(n),
        _ => Err(invalid(at, "length out of range")),
    }
}

/// `N` bytes at `*p`, advancing it.
fn fixed<const N: usize>(b: &[u8], p: &mut usize) -> Result<[u8; N], BsonError> {
    let bytes = b.get(*p..*p + N).ok_or_else(|| invalid(*p, "value cut short"))?;
    *p += N;
    Ok(bytes.try_into().expect("N bytes"))
}

/// The bytes at `at` as UTF-8. Without the specification's checks, bytes that are not UTF-8 pass
/// as an empty text: only [`validate_message`] reads that way, and it keeps no value.
fn text(bytes: &[u8], at: usize, mode: Mode) -> Result<&str, BsonError> {
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok(s),
        Err(_) if mode.spec() => Err(BsonError::InvalidUtf8 { offset: at }),
        Err(_) => Ok(""),
    }
}

/// A `00`-terminated UTF-8 name at `at`, and where it ends.
fn cstring(b: &[u8], at: usize, mode: Mode) -> Result<(&str, usize), BsonError> {
    let len = b
        .get(at..)
        .and_then(|rest| rest.iter().position(|&c| c == 0))
        .ok_or_else(|| invalid(at, "name without its terminator"))?;
    Ok((text(&b[at..at + len], at, mode)?, at + len + 1))
}

/// A string: `i32` length (counting its terminator), UTF-8 bytes and `00`.
fn string<'a>(b: &'a [u8], p: &mut usize, mode: Mode) -> Result<&'a str, BsonError> {
    let at = *p;
    let len = length(b, at, 1)?;
    let end = at + 4 + len;
    if end > b.len() {
        return Err(invalid(at, "string longer than its document"));
    }
    if b[end - 1] != 0 {
        return Err(invalid(end - 1, "string without its terminator"));
    }
    let s = text(&b[at + 4..end - 1], at + 4, mode)?;
    *p = end;
    Ok(s)
}

/// Binary data: `i32` length, subtype and bytes; subtype 2 repeats the length inside. UUIDs and
/// MD5 digests take any length, as a `refrouter` 9.0.4 stores them (measured, 003 T009).
fn binary<'a>(b: &'a [u8], p: &mut usize, mode: Mode) -> Result<RawValue<'a>, BsonError> {
    let at = *p;
    let len = length(b, at, 0)?;
    let start = at + 5;
    if start + len > b.len() {
        return Err(invalid(at, "binary data longer than its document"));
    }
    let subtype = b[at + 4];
    let mut bytes = &b[start..start + len];
    if let (types::BINARY_COLUMN, Mode::Check { spec: false, column, .. }) = (subtype, mode) {
        // A `refrouter` 9.0.4 checks the layout of every column of a message and refuses a column
        // inside another one (measured, 003 T029); the BSON specification takes any bytes.
        if column || crate::column::validate(bytes).is_err() {
            return Err(BsonError::InvalidColumn { offset: at, path: String::new() });
        }
    }
    if subtype == types::BINARY_OLD && mode.spec() {
        let mut q = 0;
        let inner = fixed::<4>(bytes, &mut q)
            .map_err(|_| invalid(start, "old binary without its length"))?;
        if i32::from_le_bytes(inner) != len as i32 - 4 {
            return Err(invalid(start, "old binary whose two lengths differ"));
        }
        bytes = &bytes[4..];
    }
    *p = start + len;
    Ok(RawValue::Binary { subtype, bytes })
}

/// Code with scope: `i32` total length, the code as a string and the scope document, which have to
/// fill that length exactly.
fn code_with_scope<'a>(b: &'a [u8], p: &mut usize, mode: Mode) -> Result<RawValue<'a>, BsonError> {
    let at = *p;
    let end = at + length(b, at, 14)?;
    let b = &b[..end];
    *p = at + 4;
    let code = string(b, p, mode)?;
    let scope_at = *p;
    if scope_at >= end {
        return Err(invalid(at, "code with scope without its scope"));
    }
    let scope_end = document(b, scope_at, mode)?;
    if scope_end != end {
        return Err(invalid(scope_end, "code with scope whose length does not match its parts"));
    }
    *p = end;
    Ok(RawValue::JavaScriptWithScope { code, scope: RawDoc::trusted(&b[scope_at..end]) })
}
