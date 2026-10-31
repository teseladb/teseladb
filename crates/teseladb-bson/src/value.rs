//! Owned BSON values for building documents (FR-004), and their conversion from the view.

use crate::decimal128::Decimal128;
use crate::raw::{RawDoc, RawValue};
use crate::types;

/// An owned BSON value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// Double.
    Double(f64),
    /// String.
    String(String),
    /// Embedded document.
    Document(Document),
    /// Array.
    Array(Vec<Value>),
    /// Binary data (for subtype 2, without its inner length).
    Binary {
        /// Subtype.
        subtype: u8,
        /// Bytes.
        bytes: Vec<u8>,
    },
    /// Undefined.
    Undefined,
    /// ObjectId.
    ObjectId([u8; 12]),
    /// Boolean.
    Boolean(bool),
    /// UTC datetime in milliseconds.
    DateTime(i64),
    /// Null.
    Null,
    /// Regular expression; written with its options in alphabetical order.
    Regex {
        /// Pattern.
        pattern: String,
        /// Options.
        options: String,
    },
    /// DBPointer.
    DbPointer {
        /// Namespace.
        namespace: String,
        /// ObjectId.
        id: [u8; 12],
    },
    /// JavaScript code.
    JavaScript(String),
    /// Symbol.
    Symbol(String),
    /// JavaScript code with scope.
    JavaScriptWithScope {
        /// Code.
        code: String,
        /// Scope.
        scope: Document,
    },
    /// 32-bit integer.
    Int32(i32),
    /// Timestamp.
    Timestamp {
        /// Seconds.
        time: u32,
        /// Ordinal within the second.
        increment: u32,
    },
    /// 64-bit integer.
    Int64(i64),
    /// Decimal128.
    Decimal128(Decimal128),
    /// Min key.
    MinKey,
    /// Max key.
    MaxKey,
}

/// An owned BSON document: fields in order. BSON allows a name twice and so does this type.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Document {
    fields: Vec<(String, Value)>,
}

impl Document {
    /// Empty document.
    pub fn new() -> Document {
        Document::default()
    }

    /// Sets `key` to `value`: in its place if it is there, at the end otherwise. Returns the value
    /// it had.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Value>) -> Option<Value> {
        let (key, value) = (key.into(), value.into());
        match self.fields.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => Some(std::mem::replace(v, value)),
            None => {
                self.fields.push((key, value));
                None
            }
        }
    }

    /// Adds a field at the end, even if the name is already there.
    pub fn push(&mut self, key: impl Into<String>, value: impl Into<Value>) {
        self.fields.push((key.into(), value.into()));
    }

    /// The value of the first field named `key`.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// The value of the first field named `key`, to change in place.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.fields.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Removes the first field named `key`.
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let i = self.fields.iter().position(|(k, _)| k == key)?;
        Some(self.fields.remove(i).1)
    }

    /// Its fields in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> + '_ {
        self.fields.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Number of fields.
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Whether it has no fields.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

impl From<RawDoc<'_>> for Document {
    fn from(raw: RawDoc<'_>) -> Document {
        let mut d = Document::new();
        for (k, v) in raw.iter() {
            d.push(k, Value::from(v));
        }
        d
    }
}

impl From<RawValue<'_>> for Value {
    fn from(v: RawValue<'_>) -> Value {
        match v {
            RawValue::Double(x) => Value::Double(x),
            RawValue::String(s) => Value::String(s.to_owned()),
            RawValue::Document(d) => Value::Document(d.into()),
            RawValue::Array(a) => Value::Array(a.iter().map(Value::from).collect()),
            RawValue::Binary { subtype, bytes } => Value::Binary { subtype, bytes: bytes.to_vec() },
            RawValue::Undefined => Value::Undefined,
            RawValue::ObjectId(id) => Value::ObjectId(id),
            RawValue::Boolean(b) => Value::Boolean(b),
            RawValue::DateTime(ms) => Value::DateTime(ms),
            RawValue::Null => Value::Null,
            RawValue::Regex { pattern, options } => {
                Value::Regex { pattern: pattern.to_owned(), options: options.to_owned() }
            }
            RawValue::DbPointer { namespace, id } => {
                Value::DbPointer { namespace: namespace.to_owned(), id }
            }
            RawValue::JavaScript(c) => Value::JavaScript(c.to_owned()),
            RawValue::Symbol(s) => Value::Symbol(s.to_owned()),
            RawValue::JavaScriptWithScope { code, scope } => {
                Value::JavaScriptWithScope { code: code.to_owned(), scope: scope.into() }
            }
            RawValue::Int32(n) => Value::Int32(n),
            RawValue::Timestamp { time, increment } => Value::Timestamp { time, increment },
            RawValue::Int64(n) => Value::Int64(n),
            RawValue::Decimal128(d) => Value::Decimal128(d),
            RawValue::MinKey => Value::MinKey,
            RawValue::MaxKey => Value::MaxKey,
        }
    }
}

impl Value {
    /// The value's BSON element type.
    pub fn element_type(&self) -> u8 {
        match self {
            Value::Double(_) => types::DOUBLE,
            Value::String(_) => types::STRING,
            Value::Document(_) => types::DOCUMENT,
            Value::Array(_) => types::ARRAY,
            Value::Binary { .. } => types::BINARY,
            Value::Undefined => types::UNDEFINED,
            Value::ObjectId(_) => types::OBJECT_ID,
            Value::Boolean(_) => types::BOOLEAN,
            Value::DateTime(_) => types::DATE_TIME,
            Value::Null => types::NULL,
            Value::Regex { .. } => types::REGEX,
            Value::DbPointer { .. } => types::DB_POINTER,
            Value::JavaScript(_) => types::JAVASCRIPT,
            Value::Symbol(_) => types::SYMBOL,
            Value::JavaScriptWithScope { .. } => types::JAVASCRIPT_WITH_SCOPE,
            Value::Int32(_) => types::INT32,
            Value::Timestamp { .. } => types::TIMESTAMP,
            Value::Int64(_) => types::INT64,
            Value::Decimal128(_) => types::DECIMAL128,
            Value::MinKey => types::MIN_KEY,
            Value::MaxKey => types::MAX_KEY,
        }
    }
}
