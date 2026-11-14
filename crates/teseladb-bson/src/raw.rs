//! Zero-copy view over the bytes of a validated document (FR-004): iteration in field order and
//! access by dotted path, with array elements by their position.

use crate::decimal128::Decimal128;
use crate::error::BsonError;
use crate::read::{self, Limits, Mode};
use crate::types;

/// A validated BSON document, borrowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawDoc<'a> {
    bytes: &'a [u8],
}

/// A validated BSON array, borrowed: a document whose names are its positions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawArray<'a>(pub(crate) RawDoc<'a>);

/// A value inside a [`RawDoc`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RawValue<'a> {
    /// Double.
    Double(f64),
    /// String.
    String(&'a str),
    /// Embedded document.
    Document(RawDoc<'a>),
    /// Array.
    Array(RawArray<'a>),
    /// Binary data (for subtype 2, without its inner length).
    Binary {
        /// Subtype.
        subtype: u8,
        /// Bytes.
        bytes: &'a [u8],
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
    /// Regular expression.
    Regex {
        /// Pattern.
        pattern: &'a str,
        /// Options.
        options: &'a str,
    },
    /// `DBPointer`.
    DbPointer {
        /// Namespace.
        namespace: &'a str,
        /// `ObjectId`.
        id: [u8; 12],
    },
    /// JavaScript code.
    JavaScript(&'a str),
    /// Symbol.
    Symbol(&'a str),
    /// JavaScript code with scope.
    JavaScriptWithScope {
        /// Code.
        code: &'a str,
        /// Scope.
        scope: RawDoc<'a>,
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

impl RawDoc<'static> {
    /// The document without fields.
    pub const EMPTY: RawDoc<'static> = RawDoc { bytes: &[5, 0, 0, 0, 0] };
}

impl<'a> RawDoc<'a> {
    /// Validates `bytes` as one stored document (FR-002, FR-003).
    ///
    /// # Errors
    ///
    /// A [`BsonError`]: [`BsonError::InvalidBson`] or [`BsonError::InvalidUtf8`] for bytes that do not follow BSON,
    /// [`BsonError::TooLarge`] or [`BsonError::TooDeep`] past the limits, and [`BsonError::InvalidColumn`] for a
    /// bad BSON column.
    pub fn from_bytes(bytes: &'a [u8]) -> Result<RawDoc<'a>, BsonError> {
        RawDoc::from_bytes_with(bytes, Limits::default())
    }

    /// Validates `bytes` as one document within `limits`.
    ///
    /// # Errors
    ///
    /// A [`BsonError`]: [`BsonError::InvalidBson`] or [`BsonError::InvalidUtf8`] for bytes that do not follow BSON,
    /// [`BsonError::TooLarge`] or [`BsonError::TooDeep`] past the limits, and [`BsonError::InvalidColumn`] for a
    /// bad BSON column.
    pub fn from_bytes_with(bytes: &'a [u8], limits: Limits) -> Result<RawDoc<'a>, BsonError> {
        read::validate(bytes, limits)?;
        Ok(RawDoc { bytes })
    }

    /// A view over a document this program stored itself, checking only its frame: the length
    /// it declares is that of `bytes` and it ends in `00`. Its elements are not walked again: a
    /// document validated when it came in, kept since under checksums (a storage engine's
    /// blocks), is read as it was written. Iterating bytes that are not valid BSON panics, so
    /// anything else goes through [`RawDoc::from_bytes`].
    ///
    /// # Errors
    ///
    /// [`BsonError::InvalidBson`] when the frame does not hold.
    pub fn from_stored(bytes: &'a [u8]) -> Result<RawDoc<'a>, BsonError> {
        let declared = bytes
            .first_chunk::<4>()
            .map(|b| i32::from_le_bytes(*b))
            .and_then(|n| usize::try_from(n).ok());
        match declared {
            Some(n) if n >= 5 && n == bytes.len() && bytes[n - 1] == 0 => Ok(RawDoc { bytes }),
            _ => Err(crate::error::invalid(0, "stored document with a broken frame")),
        }
    }

    /// A view over bytes the reader has already validated.
    pub(crate) fn trusted(bytes: &'a [u8]) -> RawDoc<'a> {
        RawDoc { bytes }
    }

    /// The document's bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Its fields in order.
    #[must_use]
    pub fn iter(&self) -> RawIter<'a> {
        RawIter { bytes: self.bytes, pos: 4 }
    }

    /// Whether it has no fields.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.len() == 5
    }

    /// The value at a dotted path (`a.b.0.c`); a segment of an array is the element's position.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<RawValue<'a>> {
        let mut segments = path.split('.');
        let mut value = self.field(segments.next()?)?;
        for segment in segments {
            value = match value {
                RawValue::Document(d) | RawValue::Array(RawArray(d)) => d.field(segment)?,
                _ => return None,
            };
        }
        Some(value)
    }

    /// The first field named `name`.
    fn field(&self, name: &str) -> Option<RawValue<'a>> {
        self.iter().find(|(k, _)| *k == name).map(|(_, v)| v)
    }
}

impl<'a> RawArray<'a> {
    /// Its elements in order.
    pub fn iter(&self) -> impl Iterator<Item = RawValue<'a>> + 'a {
        self.0.iter().map(|(_, v)| v)
    }

    /// The array as the document BSON stores it.
    #[must_use]
    pub fn as_doc(&self) -> RawDoc<'a> {
        self.0
    }
}

impl<'a> IntoIterator for &RawDoc<'a> {
    type Item = <RawIter<'a> as Iterator>::Item;
    type IntoIter = RawIter<'a>;

    fn into_iter(self) -> RawIter<'a> {
        self.iter()
    }
}

/// Iterator over a [`RawDoc`]'s fields.
#[derive(Clone, Debug)]
pub struct RawIter<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for RawIter<'a> {
    type Item = (&'a str, RawValue<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.bytes[self.pos] == 0 {
            return None;
        }
        let (key, value, next) =
            read::element(self.bytes, self.pos, Mode::Trusted).expect("a validated document");
        self.pos = next;
        Some((key, value))
    }
}

impl RawValue<'_> {
    /// The value's BSON element type.
    #[must_use]
    pub fn element_type(&self) -> u8 {
        match self {
            RawValue::Double(_) => types::DOUBLE,
            RawValue::String(_) => types::STRING,
            RawValue::Document(_) => types::DOCUMENT,
            RawValue::Array(_) => types::ARRAY,
            RawValue::Binary { .. } => types::BINARY,
            RawValue::Undefined => types::UNDEFINED,
            RawValue::ObjectId(_) => types::OBJECT_ID,
            RawValue::Boolean(_) => types::BOOLEAN,
            RawValue::DateTime(_) => types::DATE_TIME,
            RawValue::Null => types::NULL,
            RawValue::Regex { .. } => types::REGEX,
            RawValue::DbPointer { .. } => types::DB_POINTER,
            RawValue::JavaScript(_) => types::JAVASCRIPT,
            RawValue::Symbol(_) => types::SYMBOL,
            RawValue::JavaScriptWithScope { .. } => types::JAVASCRIPT_WITH_SCOPE,
            RawValue::Int32(_) => types::INT32,
            RawValue::Timestamp { .. } => types::TIMESTAMP,
            RawValue::Int64(_) => types::INT64,
            RawValue::Decimal128(_) => types::DECIMAL128,
            RawValue::MinKey => types::MIN_KEY,
            RawValue::MaxKey => types::MAX_KEY,
        }
    }
}
