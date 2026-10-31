//! The JSON value.

/// Lossless JSON number.
#[derive(Clone, Debug, PartialEq)]
pub enum Number {
    /// Integer that fits in `i64`.
    Int(i64),
    /// Positive integer that only fits in `u64`.
    UInt(u64),
    /// Number with a fraction or exponent (always finite).
    Float(f64),
    /// Integer wider than 64 bits, with its exact text.
    Big(String),
}

impl Number {
    /// As `i64`, if it is an integer that fits.
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Number::Int(v) => Some(v),
            Number::UInt(v) => i64::try_from(v).ok(),
            _ => None,
        }
    }

    /// As `u64`, if it is a non-negative integer that fits.
    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            Number::Int(v) => u64::try_from(v).ok(),
            Number::UInt(v) => Some(v),
            _ => None,
        }
    }

    /// As `f64` (possibly losing precision for large integers).
    pub fn as_f64(&self) -> f64 {
        match self {
            Number::Int(v) => *v as f64,
            Number::UInt(v) => *v as f64,
            Number::Float(v) => *v,
            Number::Big(s) => s.parse().unwrap_or(f64::NAN),
        }
    }
}

/// JSON object: pairs in insertion order, without repeated keys.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Object {
    entries: Vec<(String, Value)>,
}

impl Object {
    /// Empty object.
    pub fn new() -> Object {
        Object::default()
    }

    /// Number of pairs.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Tells whether it has no pairs.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Value of a key.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Sets a key: replaces its value if present (keeping its position) or appends it at the
    /// end. Returns the previous value.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Value>) -> Option<Value> {
        let key = key.into();
        let value = value.into();
        match self.entries.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => Some(core::mem::replace(v, value)),
            None => {
                self.entries.push((key, value));
                None
            }
        }
    }

    /// Removes a key.
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let i = self.entries.iter().position(|(k, _)| k == key)?;
        Some(self.entries.remove(i).1)
    }

    /// Pairs in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> + '_ {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Appends without checking for duplicates (the parser already did).
    pub(crate) fn push_unchecked(&mut self, key: String, value: Value) {
        self.entries.push((key, value));
    }
}

impl<K: Into<String>, V: Into<Value>> FromIterator<(K, V)> for Object {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Object {
        let mut o = Object::new();
        for (k, v) in iter {
            o.insert(k, v);
        }
        o
    }
}

/// JSON value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// Number.
    Number(Number),
    /// String.
    String(String),
    /// Array.
    Array(Vec<Value>),
    /// Object.
    Object(Object),
}

impl Value {
    /// Tells whether it is `null`.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// As a boolean.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// As `i64`.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Number(n) => n.as_i64(),
            _ => None,
        }
    }

    /// As `u64`.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Number(n) => n.as_u64(),
            _ => None,
        }
    }

    /// As `f64`.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(n.as_f64()),
            _ => None,
        }
    }

    /// As a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// As an array.
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    /// As an object.
    pub fn as_object(&self) -> Option<&Object> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }

    /// Field of an object (`None` if it is not an object or lacks the key).
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_object()?.get(key)
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Value {
        Value::Bool(v)
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Value {
        Value::Number(Number::Int(v))
    }
}

impl From<u64> for Value {
    fn from(v: u64) -> Value {
        Value::Number(i64::try_from(v).map_or(Number::UInt(v), Number::Int))
    }
}

impl From<u32> for Value {
    fn from(v: u32) -> Value {
        Value::Number(Number::Int(i64::from(v)))
    }
}

impl From<f64> for Value {
    /// Non-finite values (NaN, ±∞) do not exist in JSON: they become `null`.
    fn from(v: f64) -> Value {
        if v.is_finite() { Value::Number(Number::Float(v)) } else { Value::Null }
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Value {
        Value::String(v.to_owned())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Value {
        Value::String(v)
    }
}

impl From<Vec<Value>> for Value {
    fn from(v: Vec<Value>) -> Value {
        Value::Array(v)
    }
}

impl From<Object> for Value {
    fn from(v: Object) -> Value {
        Value::Object(v)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Value {
        v.map_or(Value::Null, Into::into)
    }
}
