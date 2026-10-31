//! BSON type aliases of `$type` and `bsonType`, each declared once, with RefDb's numbers.

use teseladb_bson::types as t;

/// `double`.
pub const DOUBLE: &str = "double";
/// `string`.
pub const STRING: &str = "string";
/// `object`.
pub const OBJECT: &str = "object";
/// `array`.
pub const ARRAY: &str = "array";
/// `binData`.
pub const BIN_DATA: &str = "binData";
/// `undefined`.
pub const UNDEFINED: &str = "undefined";
/// `objectId`.
pub const OBJECT_ID: &str = "objectId";
/// `bool`.
pub const BOOL: &str = "bool";
/// `date`.
pub const DATE: &str = "date";
/// `null`.
pub const NULL: &str = "null";
/// `regex`.
pub const REGEX: &str = "regex";
/// `dbPointer`.
pub const DB_POINTER: &str = "dbPointer";
/// `javascript`.
pub const JAVASCRIPT: &str = "javascript";
/// `symbol`.
pub const SYMBOL: &str = "symbol";
/// `javascriptWithScope`.
pub const JAVASCRIPT_WITH_SCOPE: &str = "javascriptWithScope";
/// `int`.
pub const INT: &str = "int";
/// `timestamp`.
pub const TIMESTAMP: &str = "timestamp";
/// `long`.
pub const LONG: &str = "long";
/// `decimal`.
pub const DECIMAL: &str = "decimal";
/// `minKey`.
pub const MIN_KEY: &str = "minKey";
/// `maxKey`.
pub const MAX_KEY: &str = "maxKey";
/// `number`: the four numeric types.
pub const NUMBER: &str = "number";

/// The `$type` code of `minKey`.
pub const MIN_KEY_CODE: i32 = -1;
/// The `$type` code of `maxKey`.
pub const MAX_KEY_CODE: i32 = 127;

/// Aliases with their BSON element type.
const ALIASES: [(&str, u8); 21] = [
    (DOUBLE, t::DOUBLE),
    (STRING, t::STRING),
    (OBJECT, t::DOCUMENT),
    (ARRAY, t::ARRAY),
    (BIN_DATA, t::BINARY),
    (UNDEFINED, t::UNDEFINED),
    (OBJECT_ID, t::OBJECT_ID),
    (BOOL, t::BOOLEAN),
    (DATE, t::DATE_TIME),
    (NULL, t::NULL),
    (REGEX, t::REGEX),
    (DB_POINTER, t::DB_POINTER),
    (JAVASCRIPT, t::JAVASCRIPT),
    (SYMBOL, t::SYMBOL),
    (JAVASCRIPT_WITH_SCOPE, t::JAVASCRIPT_WITH_SCOPE),
    (INT, t::INT32),
    (TIMESTAMP, t::TIMESTAMP),
    (LONG, t::INT64),
    (DECIMAL, t::DECIMAL128),
    (MIN_KEY, t::MIN_KEY),
    (MAX_KEY, t::MAX_KEY),
];

/// A type a `$type` or `bsonType` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeSpec {
    /// One BSON element type.
    Element(u8),
    /// Any of the four numeric types.
    Number,
}

impl TypeSpec {
    /// Whether a value of element type `ty` is of this type.
    pub fn matches(self, ty: u8) -> bool {
        match self {
            TypeSpec::Element(e) => e == ty,
            TypeSpec::Number => matches!(ty, t::INT32 | t::INT64 | t::DOUBLE | t::DECIMAL128),
        }
    }
}

/// The type of an alias.
pub fn by_alias(alias: &str) -> Option<TypeSpec> {
    if alias == NUMBER {
        return Some(TypeSpec::Number);
    }
    ALIASES.iter().find(|(a, _)| *a == alias).map(|&(_, e)| TypeSpec::Element(e))
}

/// The type of a `$type` code (`-1` and `127` for the keys).
pub fn by_code(code: i32) -> Option<TypeSpec> {
    let element = match code {
        MIN_KEY_CODE => t::MIN_KEY,
        MAX_KEY_CODE => t::MAX_KEY,
        1..=19 => u8::try_from(code).ok()?,
        _ => return None,
    };
    Some(TypeSpec::Element(element))
}

/// The alias of an element type, as RefDb names types in messages.
pub fn name_of(ty: u8) -> &'static str {
    ALIASES.iter().find(|&&(_, e)| e == ty).map_or("unknown", |&(a, _)| a)
}
