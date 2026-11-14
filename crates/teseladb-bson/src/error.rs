//! Errors of reading and writing BSON, with the code the reference server answers for each one
//! (contract `bson-api.md`).

use std::fmt;

/// A document that cannot be read or written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BsonError {
    /// Incoherent structure at `offset`: a length, a terminator, an unknown type, bytes left over.
    InvalidBson {
        /// Offset of the first byte that does not fit.
        offset: usize,
        /// What does not fit.
        reason: &'static str,
    },
    /// A string or a field name without valid UTF-8 at `offset`.
    InvalidUtf8 {
        /// Offset of the string's first byte.
        offset: usize,
    },
    /// More bytes than the limit allows ([`crate::MAX_SIZE`] for a stored document).
    TooLarge {
        /// Size of the document, or of the string or binary value that is already past `max`.
        size: usize,
        /// The limit.
        max: usize,
    },
    /// More nested levels than the limit allows, the deepest one starting at `offset`.
    TooDeep {
        /// Offset of the document past the limit (in the bytes written so far, when writing).
        offset: usize,
        /// Dotted names from the top document to the element whose value is past the limit,
        /// array positions included (`documents.0.a`), as the reference server names it.
        path: String,
    },
    /// A BSON column (binary subtype 7) whose bytes do not follow the column layout, at `offset`.
    InvalidColumn {
        /// Offset of the binary value.
        offset: usize,
        /// Dotted names from the top document to the element, as for [`BsonError::TooDeep`].
        path: String,
    },
}

impl BsonError {
    /// The reference server's error code and name for this error.
    #[must_use]
    pub fn code(&self) -> (i32, &'static str) {
        match self {
            BsonError::InvalidBson { .. } | BsonError::InvalidUtf8 { .. } => (22, "InvalidBSON"),
            BsonError::TooLarge { .. } => (10334, "BSONObjectTooLarge"),
            BsonError::TooDeep { .. } => (15, "Overflow"),
            BsonError::InvalidColumn { .. } => (378, "NonConformantBSON"),
        }
    }
}

impl fmt::Display for BsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BsonError::InvalidBson { offset, reason } => {
                write!(f, "invalid BSON at {offset}: {reason}")
            }
            BsonError::InvalidUtf8 { offset } => write!(f, "invalid UTF-8 at {offset}"),
            BsonError::TooLarge { size, max } => {
                write!(f, "object of {size} bytes, over the maximum of {max}")
            }
            // The reference server's words for a document past its nesting limit (a reference
            // router 9.0.4).
            BsonError::TooDeep { path, .. } => write!(
                f,
                "BSONObj exceeds maximum nested object depth in element with field name '{path}'"
            ),
            // A reference router 9.0.4's words (measured, 003 T029).
            BsonError::InvalidColumn { path, .. } => {
                write!(f, "Invalid BSON column in element with field name '{path}'")
            }
        }
    }
}

impl std::error::Error for BsonError {}

impl BsonError {
    /// The same error seen from the document that holds the element `key`: a depth or column
    /// error gets `key` in front of its path.
    pub(crate) fn under(self, key: &str) -> BsonError {
        let prefixed = |path: String| {
            if path.is_empty() { key.to_owned() } else { format!("{key}.{path}") }
        };
        match self {
            BsonError::TooDeep { offset, path } => {
                BsonError::TooDeep { offset, path: prefixed(path) }
            }
            BsonError::InvalidColumn { offset, path } => {
                BsonError::InvalidColumn { offset, path: prefixed(path) }
            }
            other => other,
        }
    }
}

/// [`BsonError::InvalidBson`] at `offset`.
pub(crate) fn invalid(offset: usize, reason: &'static str) -> BsonError {
    BsonError::InvalidBson { offset, reason }
}
