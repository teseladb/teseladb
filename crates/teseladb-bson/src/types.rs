//! Element types and binary subtypes of BSON 1.1 (bsonspec.org), each declared once.

/// 64-bit binary floating point.
pub const DOUBLE: u8 = 0x01;
/// UTF-8 string.
pub const STRING: u8 = 0x02;
/// Embedded document.
pub const DOCUMENT: u8 = 0x03;
/// Array.
pub const ARRAY: u8 = 0x04;
/// Binary data.
pub const BINARY: u8 = 0x05;
/// Undefined (deprecated, still stored by RefDb).
pub const UNDEFINED: u8 = 0x06;
/// `ObjectId`.
pub const OBJECT_ID: u8 = 0x07;
/// Boolean.
pub const BOOLEAN: u8 = 0x08;
/// UTC datetime in milliseconds.
pub const DATE_TIME: u8 = 0x09;
/// Null.
pub const NULL: u8 = 0x0A;
/// Regular expression.
pub const REGEX: u8 = 0x0B;
/// `DBPointer` (deprecated).
pub const DB_POINTER: u8 = 0x0C;
/// JavaScript code.
pub const JAVASCRIPT: u8 = 0x0D;
/// Symbol (deprecated).
pub const SYMBOL: u8 = 0x0E;
/// JavaScript code with scope (deprecated).
pub const JAVASCRIPT_WITH_SCOPE: u8 = 0x0F;
/// 32-bit integer.
pub const INT32: u8 = 0x10;
/// Timestamp.
pub const TIMESTAMP: u8 = 0x11;
/// 64-bit integer.
pub const INT64: u8 = 0x12;
/// 128-bit decimal floating point.
pub const DECIMAL128: u8 = 0x13;
/// Min key.
pub const MIN_KEY: u8 = 0xFF;
/// Max key.
pub const MAX_KEY: u8 = 0x7F;

/// Generic binary subtype.
pub const BINARY_GENERIC: u8 = 0x00;
/// Function.
pub const BINARY_FUNCTION: u8 = 0x01;
/// Old binary, which repeats its length inside.
pub const BINARY_OLD: u8 = 0x02;
/// Old UUID (16 bytes).
pub const BINARY_UUID_OLD: u8 = 0x03;
/// UUID (16 bytes).
pub const BINARY_UUID: u8 = 0x04;
/// MD5 digest (16 bytes).
pub const BINARY_MD5: u8 = 0x05;
/// Encrypted value.
pub const BINARY_ENCRYPTED: u8 = 0x06;
/// Compressed time series column.
pub const BINARY_COLUMN: u8 = 0x07;
/// Sensitive data.
pub const BINARY_SENSITIVE: u8 = 0x08;
/// Vector.
pub const BINARY_VECTOR: u8 = 0x09;
/// First user-defined subtype (`0x80` to `0xFF`).
pub const BINARY_USER: u8 = 0x80;
