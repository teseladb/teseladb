//! Building documents in code: conversions from Rust's values and chained fields.
//!
//! ```
//! use teseladb_bson::{Document, Value};
//!
//! let d = Document::new().with("a", 1).with("b", "x").with("c", vec![Value::from(true)]);
//! assert_eq!(d.get("b"), Some(&Value::String("x".into())));
//! ```

use crate::decimal128::Decimal128;
use crate::value::{Document, Value};

impl Document {
    /// The document with `key` set to `value` ([`Document::insert`]).
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Value>) -> Document {
        self.insert(key, value);
        self
    }
}

impl From<f64> for Value {
    fn from(x: f64) -> Value {
        Value::Double(x)
    }
}

impl From<i32> for Value {
    fn from(n: i32) -> Value {
        Value::Int32(n)
    }
}

impl From<i64> for Value {
    fn from(n: i64) -> Value {
        Value::Int64(n)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Boolean(b)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::String(s.to_owned())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::String(s)
    }
}

impl From<Document> for Value {
    fn from(d: Document) -> Value {
        Value::Document(d)
    }
}

impl From<Vec<Value>> for Value {
    fn from(items: Vec<Value>) -> Value {
        Value::Array(items)
    }
}

impl From<Decimal128> for Value {
    fn from(d: Decimal128) -> Value {
        Value::Decimal128(d)
    }
}
