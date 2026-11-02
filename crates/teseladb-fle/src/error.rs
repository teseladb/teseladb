//! The errors of this crate: a RefDb code, its name and the message a `refnode` 9.0.4 gives.

use std::fmt;

/// An error with RefDb's code and name (`Location<code>` for the codes without a name).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FleError {
    /// Number.
    pub code: i32,
    /// `codeName`.
    pub name: &'static str,
    /// `errmsg`.
    pub message: String,
}

impl FleError {
    /// An error with a code and its name.
    pub fn new(code: i32, name: &'static str, message: impl Into<String>) -> FleError {
        FleError { code, name, message: message.into() }
    }
}

impl fmt::Display for FleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}): {}", self.name, self.code, self.message)
    }
}

impl std::error::Error for FleError {}

/// Declares the codes this crate answers with, as `(number, name)` constants.
macro_rules! codes {
    ($($(#[$doc:meta])* $id:ident = $n:literal, $name:literal;)*) => {
        $( $(#[$doc])* pub const $id: (i32, &str) = ($n, $name); )*
    };
}

codes! {
    /// A value outside what the field allows.
    BAD_VALUE = 2, "BadValue";
    /// A value of the wrong BSON type.
    TYPE_MISMATCH = 14, "TypeMismatch";
    /// A required field is missing.
    IDL_FAILED_TO_PARSE = 40_414, "IDLFailedToParse";
    /// A field the command does not know.
    IDL_UNKNOWN_FIELD = 40_415, "IDLUnknownField";
    /// An encrypted field on `_id` or below it.
    ENCRYPTED_ID = 6_316_403, "Location6316403";
    /// Two encrypted fields with the same key.
    DUPLICATE_KEY_IDS = 6_338_401, "Location6338401";
    /// Two encrypted fields with the same path.
    DUPLICATE_PATHS = 6_338_402, "Location6338402";
    /// One encrypted path is a prefix of another.
    CONFLICTING_PATHS = 6_338_403, "Location6338403";
    /// A type equality queries cannot index.
    EQUALITY_TYPE = 6_338_405, "Location6338405";
    /// A type range queries cannot index.
    RANGE_TYPE = 6_775_201, "Location6775201";
    /// A floating point range without its precision, minimum and maximum together.
    RANGE_PRECISION_TRIO = 6_967_100, "Location6967100";
    /// A range bound of another type than the field.
    RANGE_BOUND_TYPE = 7_018_200, "Location7018200";
    /// A metadata collection whose name is not the one of its collection.
    ESC_NAME = 7_406_900, "Location7406900";
    /// `rangePreview`, deprecated (DV-014).
    RANGE_PREVIEW = 8_575_605, "Location8575605";
    /// A payload too short to hold its BSON.
    OVERFLOW = 15, "Overflow";
    /// A payload whose BSON is not valid.
    INVALID_BSON = 22, "InvalidBSON";
    /// A document the implicit schema of its encrypted fields refuses.
    DOCUMENT_VALIDATION_FAILURE = 121, "DocumentValidationFailure";
    /// An encrypted value of a kind the operation does not take.
    UNEXPECTED_PAYLOAD = 6_373_503, "Location6373503";
    /// A payload encrypted with another key than its field's.
    MISMATCHED_KEY_ID = 6_726_301, "Location6726301";
    /// A payload's contention factor above its field's.
    CONTENTION_EXCEEDED = 9_188_700, "Location9188700";
    /// A token of the wrong length.
    TOKEN_LENGTH = 9_616_300, "Location9616300";
    /// A type the text queries cannot index.
    TEXT_TYPE = 9_783_400, "Location9783400";
    /// A text query without one of its required parameters.
    TEXT_PARAMETER_MISSING = 9_783_402, "Location9783402";
    /// `queries` as an empty array.
    NO_QUERY_TYPE = 9_783_412, "Location9783412";
    /// Several query types other than `suffix` and `prefix`.
    MULTIPLE_QUERY_TYPES = 9_783_414, "Location9783414";
    /// `prefixPreview`, `suffixPreview` or `substringPreview`, deprecated (DV-014).
    TEXT_PREVIEW = 12_341_600, "Location12341600";
}

/// An error from a `(code, name)` pair.
pub fn err(code: (i32, &'static str), message: impl Into<String>) -> FleError {
    FleError::new(code.0, code.1, message)
}
