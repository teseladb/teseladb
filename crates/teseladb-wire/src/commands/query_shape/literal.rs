//! The literals of a query shape (010 FR-023, measured on a `refrouter` 9.0.4, 01-11-2026): the
//! debug shape that `$queryStats` and `$querySettings` show names a literal's type
//! (`"?number"`, `"?array<?string>"`, `"[]"` for an empty array), and the representative shape
//! whose SHA-256 is the `queryShapeHash` puts one fixed value of that type in its place
//! (`1` for any number, `"?"` for a string, `{"?": "?"}` for a document, `[]` or a one-element
//! array, `Date(0)`, an `ObjectId` of `0xff` bytes, `Timestamp(0, 0)`, empty binary data of
//! subtype 0, a regular expression `/?/` without options, `Code("return ?;")`).

use teseladb_bson::{Document, Value};

/// Which of the two serialisations of a shape is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// The shape shown by `$queryStats` and `$querySettings`: literals as their type names.
    Debug,
    /// The shape hashed into `queryShapeHash`: literals as a fixed value of their type.
    Representative,
}

/// The pattern of the representative regular expression.
const REGEX_PATTERN: &str = "/?/";
/// The options of the representative regular expression.
const REGEX_OPTIONS: &str = "";
/// The code of the representative JavaScript.
const CODE: &str = "return ?;";
/// The representative string.
pub const STRING: &str = "?";

/// The name of the canonical type of `v` as the debug shape writes it.
fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Double(_) | Value::Int32(_) | Value::Int64(_) | Value::Decimal128(_) => "number",
        Value::String(_) | Value::Symbol(_) => "string",
        Value::Document(_) => "object",
        Value::Array(_) => "array",
        Value::Binary { .. } => "binData",
        Value::Undefined | Value::Null => "null",
        Value::ObjectId(_) => "objectId",
        Value::Boolean(_) => "bool",
        Value::DateTime(_) => "date",
        Value::Regex { .. } => "regex",
        Value::DbPointer { .. } => "dbPointer",
        Value::JavaScript(_) => "javascript",
        Value::JavaScriptWithScope { .. } => "javascriptWithScope",
        Value::Timestamp { .. } => "timestamp",
        Value::MinKey => "minKey",
        Value::MaxKey => "maxKey",
    }
}

/// Whether the elements of an array all have the same BSON type (numbers of different types do
/// not, measured: `[1, 2.5]` is `"?array<>"`).
fn same_types(items: &[Value]) -> bool {
    items.iter().all(|i| std::mem::discriminant(i) == std::mem::discriminant(&items[0]))
}

/// Literal `v` under policy `p`.
pub fn literal(v: &Value, p: Policy) -> Value {
    match p {
        Policy::Debug => Value::String(debug(v)),
        Policy::Representative => representative(v),
    }
}

/// The debug name of literal `v`: `"?<type>"`, `"?array<?<type>>"` for an array whose elements
/// share a type, `"?array<>"` for one whose elements do not, `"[]"` for an empty one.
fn debug(v: &Value) -> String {
    match v {
        Value::Array(items) if items.is_empty() => "[]".to_owned(),
        Value::Array(items) => {
            let first = type_name(&items[0]);
            if same_types(items) { format!("?array<?{first}>") } else { "?array<>".to_owned() }
        }
        other => format!("?{}", type_name(other)),
    }
}

/// The representative value of literal `v`.
fn representative(v: &Value) -> Value {
    match v {
        Value::Array(items) => match items.first() {
            Some(first) if same_types(items) => Value::Array(vec![element(first)]),
            _ => Value::Array(Vec::new()),
        },
        other => element(other),
    }
}

/// The representative value of `v` as an element of an array: an array is empty there.
fn element(v: &Value) -> Value {
    match v {
        Value::Double(_) | Value::Int32(_) | Value::Int64(_) | Value::Decimal128(_) => {
            Value::Int32(1)
        }
        Value::String(_) | Value::Symbol(_) => Value::String(STRING.to_owned()),
        Value::Document(_) => Value::Document(Document::new().with(STRING, STRING)),
        Value::Array(_) => Value::Array(Vec::new()),
        Value::Binary { .. } => Value::Binary { subtype: 0, bytes: Vec::new() },
        Value::Undefined | Value::Null => Value::Null,
        Value::ObjectId(_) => Value::ObjectId([0xff; 12]),
        Value::Boolean(_) => Value::Boolean(true),
        Value::DateTime(_) => Value::DateTime(0),
        Value::Regex { .. } => {
            Value::Regex { pattern: REGEX_PATTERN.to_owned(), options: REGEX_OPTIONS.to_owned() }
        }
        Value::DbPointer { .. } => Value::DbPointer { namespace: STRING.to_owned(), id: [0; 12] },
        Value::JavaScript(_) => Value::JavaScript(CODE.to_owned()),
        Value::JavaScriptWithScope { .. } => {
            Value::JavaScriptWithScope { code: CODE.to_owned(), scope: Document::new() }
        }
        Value::Timestamp { .. } => Value::Timestamp { time: 0, increment: 0 },
        Value::MinKey => Value::MinKey,
        Value::MaxKey => Value::MaxKey,
    }
}

/// `{$const: v}`: a constant of an expression in the representative shape, where it must parse
/// back as the same constant.
pub fn constant(v: Value) -> Value {
    Value::Document(Document::new().with("$const", v))
}

/// A value the debug shape writes as its literal and the representative shape keeps as it is.
pub fn kept_in_representative(v: &Value, p: Policy) -> Value {
    match p {
        Policy::Debug => literal(v, p),
        Policy::Representative => v.clone(),
    }
}
