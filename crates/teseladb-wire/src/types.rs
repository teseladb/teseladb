//! Names RefDb gives BSON types in its messages (`$type` aliases), and the typed reading of a
//! command's fields with RefDb's errors.

use teseladb_bson::{Value, types};

use crate::error::{CommandError, TYPE_MISMATCH};

/// RefDb's alias of a value's type (`int`, `long`, `objectId`, …).
pub fn type_name(v: &Value) -> &'static str {
    match v.element_type() {
        types::DOUBLE => "double",
        types::STRING => "string",
        types::DOCUMENT => "object",
        types::ARRAY => "array",
        types::BINARY => "binData",
        types::UNDEFINED => "undefined",
        types::OBJECT_ID => "objectId",
        types::BOOLEAN => "bool",
        types::DATE_TIME => "date",
        types::NULL => "null",
        types::REGEX => "regex",
        types::DB_POINTER => "dbPointer",
        types::JAVASCRIPT => "javascript",
        types::SYMBOL => "symbol",
        types::JAVASCRIPT_WITH_SCOPE => "javascriptWithScope",
        types::INT32 => "int",
        types::TIMESTAMP => "timestamp",
        types::INT64 => "long",
        types::DECIMAL128 => "decimal",
        types::MIN_KEY => "minKey",
        _ => "maxKey",
    }
}

/// `TypeMismatch` for field `path` holding `v` where `expected` was wanted (one type alias).
pub fn wrong_type(path: &str, v: &Value, expected: &str) -> CommandError {
    let msg = format!(
        "BSON field '{path}' is the wrong type '{}', expected type '{expected}'",
        type_name(v)
    );
    CommandError::new(TYPE_MISMATCH, msg)
}

/// `TypeMismatch` for field `path` holding `v` where one of `expected` was wanted, as a
/// `refrouter` 9.0 writes it (measured).
pub fn wrong_types(path: &str, v: &Value, expected: &[&str]) -> CommandError {
    let msg = format!(
        "BSON field '{path}' is the wrong type '{}', expected types '[{}]'",
        type_name(v),
        expected.join(", ")
    );
    CommandError::new(TYPE_MISMATCH, msg)
}

/// The numeric types a "safe" number field takes, in RefDb's order.
pub const NUMBER_TYPES: [&str; 4] = ["long", "decimal", "double", "int"];
/// The types of the `safeBool` fields of the write commands (`bypassDocumentValidation`, `findAndModify`'s flags).
pub const WRITE_SAFE_BOOL_TYPES: [&str; 5] = ["long", "decimal", "bool", "int", "double"];
/// The types of the `safeBool` fields of `bulkWrite`.
pub const BULK_SAFE_BOOL_TYPES: [&str; 5] = ["long", "decimal", "int", "bool", "double"];
/// The types of the `safeBool` fields of `listDatabases`.
pub const LIST_DATABASES_SAFE_BOOL_TYPES: [&str; 5] = ["decimal", "long", "int", "bool", "double"];
/// The numeric types of `count`'s fields, in the order a `refrouter` 9.0 lists them (measured).
pub const COUNT_NUMBER_TYPES: [&str; 4] = ["decimal", "long", "double", "int"];
/// The numeric types of `hello.maxAwaitTimeMS`, in a `refrouter` 9.0's order (measured).
pub const AWAIT_TYPES: [&str; 4] = ["long", "int", "double", "decimal"];
/// The types of `hello.helloOk`, in a `refrouter` 9.0's order (measured).
pub const HELLO_OK_TYPES: [&str; 5] = ["bool", "long", "double", "decimal", "int"];

/// A number as `f64`, for the fields RefDb reads as any numeric type.
pub fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int32(n) => Some(f64::from(*n)),
        Value::Int64(n) => Some(*n as f64),
        Value::Double(x) => Some(*x),
        Value::Decimal128(d) => d.to_string().parse().ok(),
        _ => None,
    }
}

/// A value's truth as BSON reads it (RefDb's `trueValue`): `false`, `null`, `undefined` and
/// numeric zero are false, anything else true.
pub fn bson_true(v: &Value) -> bool {
    match v {
        Value::Boolean(b) => *b,
        Value::Null | Value::Undefined => false,
        other => number(other).is_none_or(|x| x != 0.0),
    }
}

/// Truthiness of a value as a `bool`-or-number field (`helloOk: 1`).
pub fn truthy(v: &Value) -> Option<bool> {
    match v {
        Value::Boolean(b) => Some(*b),
        other => number(other).map(|x| x != 0.0),
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_bson::{Decimal128, Document};

    #[test]
    fn every_type_has_its_alias() {
        let values = [
            (Value::Double(1.0), "double"),
            (Value::from("s"), "string"),
            (Value::Document(Document::new()), "object"),
            (Value::Array(Vec::new()), "array"),
            (Value::Binary { subtype: 0, bytes: Vec::new() }, "binData"),
            (Value::Undefined, "undefined"),
            (Value::ObjectId([0; 12]), "objectId"),
            (Value::Boolean(true), "bool"),
            (Value::DateTime(0), "date"),
            (Value::Null, "null"),
            (Value::Regex { pattern: "a".into(), options: String::new() }, "regex"),
            (Value::DbPointer { namespace: "a".into(), id: [0; 12] }, "dbPointer"),
            (Value::JavaScript("x".into()), "javascript"),
            (Value::Symbol("x".into()), "symbol"),
            (
                Value::JavaScriptWithScope { code: "x".into(), scope: Document::new() },
                "javascriptWithScope",
            ),
            (Value::Int32(1), "int"),
            (Value::Timestamp { time: 0, increment: 0 }, "timestamp"),
            (Value::Int64(1), "long"),
            (Value::Decimal128("1.5".parse::<Decimal128>().unwrap()), "decimal"),
            (Value::MinKey, "minKey"),
            (Value::MaxKey, "maxKey"),
        ];
        for (v, name) in values {
            assert_eq!(type_name(&v), name);
        }
        assert_eq!(number(&Value::Decimal128("2.5".parse::<Decimal128>().unwrap())), Some(2.5));
        assert_eq!(number(&Value::Null), None);
        assert_eq!(truthy(&Value::Int32(0)), Some(false));
        assert_eq!(truthy(&Value::Null), None);
    }
}
