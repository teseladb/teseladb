//! Owned BSON values for building documents (FR-004), and their conversion from the view.

use crate::decimal128::Decimal128;
use std::sync::OnceLock;

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
    /// `ObjectId`.
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
    /// `DBPointer`.
    DbPointer {
        /// Namespace.
        namespace: String,
        /// `ObjectId`.
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
///
/// A document made by [`Document::from_canonical`] keeps the bytes it was read from: its fields
/// are read from them the first time they are asked for, and it is written by copying them, until
/// it is changed. It takes the room of its fields' vector either way.
pub struct Document {
    repr: Repr,
}

/// What a [`Document`] holds.
enum Repr {
    /// Its fields.
    Fields(Vec<(String, Value)>),
    /// The canonical BSON it was read from, and its fields once they are asked for.
    Kept(Box<Kept>),
}

/// A document kept as the canonical bytes it was read from.
struct Kept {
    bytes: KeptBytes,
    fields: OnceLock<Vec<(String, Value)>>,
}

/// The bytes of a kept document: its own, or a part of a buffer it shares with other documents
/// read from the same message ([`crate::read_message_in`]).
#[derive(Clone)]
enum KeptBytes {
    Own(Box<[u8]>),
    Shared(teseladb_types::Bytes),
}

impl std::ops::Deref for KeptBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            KeptBytes::Own(b) => b,
            KeptBytes::Shared(b) => b,
        }
    }
}

impl Default for Document {
    fn default() -> Document {
        Document { repr: Repr::Fields(Vec::new()) }
    }
}

impl Clone for Document {
    fn clone(&self) -> Document {
        let repr = match &self.repr {
            Repr::Fields(f) => Repr::Fields(f.clone()),
            Repr::Kept(k) => Repr::Kept(Box::new(Kept {
                bytes: k.bytes.clone(),
                fields: k.fields.get().map_or_else(OnceLock::new, |f| OnceLock::from(f.clone())),
            })),
        };
        Document { repr }
    }
}

impl PartialEq for Document {
    fn eq(&self, other: &Document) -> bool {
        self.fields() == other.fields()
    }
}

impl std::fmt::Debug for Document {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Document").field("fields", self.fields()).finish()
    }
}

/// The fields of a validated document, as owned values.
fn read_fields(raw: RawDoc<'_>) -> Vec<(String, Value)> {
    raw.iter().map(|(k, v)| (k.to_owned(), Value::from(v))).collect()
}

impl Document {
    /// Empty document.
    #[must_use]
    pub fn new() -> Document {
        Document::default()
    }

    /// The document `raw`, keeping its bytes: its fields are read only if they are asked for, and
    /// [`Document::to_bytes`] copies the bytes while it is not changed. `raw` has to be canonical
    /// BSON, as [`Document::to_bytes`] writes it (arrays with their positions as names, regular
    /// expression options in alphabetical order), within [`crate::MAX_COMMAND_DEPTH`] levels:
    /// [`Document::from`] gives the same document from any `raw`, and writes it canonical.
    #[must_use]
    pub fn from_canonical(raw: RawDoc<'_>) -> Document {
        debug_assert!(
            crate::write::levels(raw) <= crate::MAX_COMMAND_DEPTH
                && Document::from(raw)
                    .to_bytes_with(crate::Limits { max_size: usize::MAX, max_depth: usize::MAX })
                    .ok()
                    .as_deref()
                    == Some(raw.as_bytes()),
            "a document that is not canonical BSON"
        );
        Document::kept(raw)
    }

    /// [`Document::from_canonical`] for bytes a pass of the reader found canonical already
    /// ([`crate::read_message`]): they are not decoded and written again to check it.
    pub(crate) fn kept(raw: RawDoc<'_>) -> Document {
        Document::keep(KeptBytes::Own(raw.as_bytes().into()))
    }

    /// [`Document::kept`] over a part of a shared buffer, without a copy.
    pub(crate) fn kept_shared(bytes: teseladb_types::Bytes) -> Document {
        Document::keep(KeptBytes::Shared(bytes))
    }

    fn keep(bytes: KeptBytes) -> Document {
        let kept = Kept { bytes, fields: OnceLock::new() };
        Document { repr: Repr::Kept(Box::new(kept)) }
    }

    /// The document of `bytes`, which this program stored itself (see [`RawDoc::from_stored`]),
    /// taking them without a copy: as [`Document::from_canonical`] it keeps them and writes them
    /// as they are while it is not changed. Only the frame is checked.
    ///
    /// # Errors
    ///
    /// [`BsonError::InvalidBson`] when the frame does not hold.
    pub fn from_stored(bytes: Vec<u8>) -> Result<Document, crate::BsonError> {
        RawDoc::from_stored(&bytes)?;
        debug_assert!(
            RawDoc::from_bytes(&bytes).is_ok_and(|r| {
                Document::from(r)
                    .to_bytes_with(crate::Limits { max_size: usize::MAX, max_depth: usize::MAX })
                    .ok()
                    .as_deref()
                    == Some(bytes.as_slice())
            }),
            "a stored document that is not canonical BSON"
        );
        let kept =
            Kept { bytes: KeptBytes::Own(bytes.into_boxed_slice()), fields: OnceLock::new() };
        Ok(Document { repr: Repr::Kept(Box::new(kept)) })
    }

    /// The canonical BSON a document of [`Document::from_canonical`] was read from, while it is
    /// not changed: what [`Document::to_bytes`] writes, so its length is the document's.
    #[must_use]
    pub fn canonical_bytes(&self) -> Option<&[u8]> {
        match &self.repr {
            Repr::Fields(_) => None,
            Repr::Kept(k) => Some(&k.bytes),
        }
    }

    /// Its fields, read from its bytes the first time.
    fn fields(&self) -> &Vec<(String, Value)> {
        match &self.repr {
            Repr::Fields(f) => f,
            Repr::Kept(k) => k.fields.get_or_init(|| read_fields(RawDoc::trusted(&k.bytes))),
        }
    }

    /// Its fields to change: from then on it is written from them.
    fn fields_mut(&mut self) -> &mut Vec<(String, Value)> {
        if let Repr::Kept(k) = &mut self.repr {
            let fields = k.fields.take().unwrap_or_else(|| read_fields(RawDoc::trusted(&k.bytes)));
            self.repr = Repr::Fields(fields);
        }
        match &mut self.repr {
            Repr::Fields(f) => f,
            Repr::Kept(_) => unreachable!("a kept document was just read into its fields"),
        }
    }

    /// Sets `key` to `value`: in its place if it is there, at the end otherwise. Returns the value
    /// it had.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Value>) -> Option<Value> {
        let (key, value) = (key.into(), value.into());
        let fields = self.fields_mut();
        if let Some((_, v)) = fields.iter_mut().find(|(k, _)| *k == key) {
            Some(std::mem::replace(v, value))
        } else {
            fields.push((key, value));
            None
        }
    }

    /// Adds a field at the end, even if the name is already there.
    pub fn push(&mut self, key: impl Into<String>, value: impl Into<Value>) {
        self.fields_mut().push((key.into(), value.into()));
    }

    /// The value of the first field named `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields().iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// The value of the first field named `key`, to change in place.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.fields_mut().iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Removes the first field named `key`.
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let i = self.fields().iter().position(|(k, _)| k == key)?;
        Some(self.fields_mut().remove(i).1)
    }

    /// Its fields in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> + '_ {
        self.fields().iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Number of fields.
    #[must_use]
    pub fn len(&self) -> usize {
        self.fields().len()
    }

    /// Whether it has no fields.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields().is_empty()
    }
}

impl From<RawDoc<'_>> for Document {
    fn from(raw: RawDoc<'_>) -> Document {
        Document { repr: Repr::Fields(read_fields(raw)) }
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
    /// The document this value is, if it is one.
    #[must_use]
    pub fn as_document(&self) -> Option<&Document> {
        match self {
            Value::Document(d) => Some(d),
            _ => None,
        }
    }

    /// The value's BSON element type.
    #[must_use]
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
