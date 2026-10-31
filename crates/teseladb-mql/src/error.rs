//! Errors of parsing and evaluating MQL, each with RefDb's code and code name (contract
//! `error-codes.md` of 006).

use std::fmt;

/// A RefDb error code with its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ErrorCode {
    /// Number.
    pub code: i32,
    /// `codeName`.
    pub name: &'static str,
}

/// The codes this crate answers with.
pub mod codes {
    use super::ErrorCode;

    /// An unexpected internal state.
    pub const INTERNAL_ERROR: ErrorCode = ErrorCode { code: 1, name: "InternalError" };
    /// An invalid option or value.
    pub const BAD_VALUE: ErrorCode = ErrorCode { code: 2, name: "BadValue" };
    /// A malformed update or filter.
    pub const FAILED_TO_PARSE: ErrorCode = ErrorCode { code: 9, name: "FailedToParse" };
    /// A legacy `$near` whose `$minDistance` is not a number.
    pub const NEAR_MIN_NOT_NUMBER: ErrorCode = ErrorCode { code: 16893, name: "Location16893" };
    /// A legacy `$near` whose `$minDistance` is negative.
    pub const NEAR_MIN_NEGATIVE: ErrorCode = ErrorCode { code: 16894, name: "Location16894" };
    /// A legacy `$near` whose `$maxDistance` is not a number.
    pub const NEAR_MAX_NOT_NUMBER: ErrorCode = ErrorCode { code: 16895, name: "Location16895" };
    /// A legacy `$near` whose `$maxDistance` is negative.
    pub const NEAR_MAX_NEGATIVE: ErrorCode = ErrorCode { code: 16896, name: "Location16896" };
    /// A legacy `$near` with another operator beside it.
    pub const NEAR_INVALID_ARGUMENT: ErrorCode = ErrorCode { code: 34413, name: "Location34413" };
    /// `$options` beside a `$regex` regular expression with flags, before it.
    pub const REGEX_OPTIONS_BEFORE: ErrorCode = ErrorCode { code: 51074, name: "Location51074" };
    /// `$options` beside a `$regex` regular expression with flags, after it.
    pub const REGEX_OPTIONS_AFTER: ErrorCode = ErrorCode { code: 51075, name: "Location51075" };
    /// A query feature the operation does not take (`$expr` in an upsert's filter).
    pub const QUERY_FEATURE_NOT_ALLOWED: ErrorCode =
        ErrorCode { code: 224, name: "QueryFeatureNotAllowed" };
    /// A required field that is missing (`$text` without `$search`).
    pub const NO_SUCH_KEY: ErrorCode = ErrorCode { code: 4, name: "NoSuchKey" };
    /// An operand of the wrong type.
    pub const TYPE_MISMATCH: ErrorCode = ErrorCode { code: 14, name: "TypeMismatch" };
    /// An index required by the query does not exist (`$text` without a text index).
    pub const INDEX_NOT_FOUND: ErrorCode = ErrorCode { code: 27, name: "IndexNotFound" };
    /// A path that cannot be created in the document.
    pub const PATH_NOT_VIABLE: ErrorCode = ErrorCode { code: 28, name: "PathNotViable" };
    /// Two update operators on the same path or on a path and its prefix.
    pub const CONFLICTING_UPDATE_OPERATORS: ErrorCode =
        ErrorCode { code: 40, name: "ConflictingUpdateOperators" };
    /// A field name starting with `$` where it is not allowed.
    pub const DOLLAR_PREFIXED_FIELD_NAME: ErrorCode =
        ErrorCode { code: 52, name: "DollarPrefixedFieldName" };
    /// An upsert whose filter sets a path twice.
    pub const NOT_SINGLE_VALUE_FIELD: ErrorCode =
        ErrorCode { code: 54, name: "NotSingleValueField" };
    /// An empty field name in a path.
    pub const EMPTY_FIELD_NAME: ErrorCode = ErrorCode { code: 56, name: "EmptyFieldName" };
    /// A change of `_id`.
    pub const IMMUTABLE_FIELD: ErrorCode = ErrorCode { code: 66, name: "ImmutableField" };
    /// A document a validator rejects.
    pub const DOCUMENT_VALIDATION_FAILURE: ErrorCode =
        ErrorCode { code: 121, name: "DocumentValidationFailure" };
    /// `$near` or `$nearSphere` without a geospatial index.
    pub const NO_QUERY_EXECUTION_PLANS: ErrorCode =
        ErrorCode { code: 291, name: "NoQueryExecutionPlans" };
    /// A document over 16 MiB.
    pub const BSON_OBJECT_TOO_LARGE: ErrorCode =
        ErrorCode { code: 10334, name: "BSONObjectTooLarge" };
    /// A regular expression that does not compile.
    pub const REGEX_INVALID: ErrorCode = ErrorCode { code: 51091, name: "Location51091" };
    /// An unknown regular expression option.
    pub const REGEX_INVALID_OPTION: ErrorCode = ErrorCode { code: 51108, name: "Location51108" };
    /// A projection path that collides with another one.
    pub const PROJECTION_PATH_COLLISION: ErrorCode =
        ErrorCode { code: 31250, name: "Location31250" };
    /// An inclusion in an exclusion projection.
    pub const PROJECTION_INCLUSION_IN_EXCLUSION: ErrorCode =
        ErrorCode { code: 31253, name: "Location31253" };
    /// An exclusion in an inclusion projection.
    pub const PROJECTION_EXCLUSION_IN_INCLUSION: ErrorCode =
        ErrorCode { code: 31254, name: "Location31254" };
}

/// An MQL error: RefDb's code and its message.
#[derive(Clone, Debug, PartialEq)]
pub struct MqlError {
    /// Code and code name.
    pub code: ErrorCode,
    /// `errmsg`.
    pub message: String,
    /// `errInfo`, for the errors that carry it (`DocumentValidationFailure`).
    pub info: Option<teseladb_bson::Document>,
}

impl MqlError {
    /// An error with `code` and `message`.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> MqlError {
        MqlError { code, message: message.into(), info: None }
    }
}

impl fmt::Display for MqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}): {}", self.code.name, self.code.code, self.message)
    }
}

impl std::error::Error for MqlError {}

/// `BadValue` with `message`.
pub(crate) fn bad_value(message: impl Into<String>) -> MqlError {
    MqlError::new(codes::BAD_VALUE, message)
}

/// `FailedToParse` with `message`.
pub(crate) fn failed_to_parse(message: impl Into<String>) -> MqlError {
    MqlError::new(codes::FAILED_TO_PARSE, message)
}

/// `TypeMismatch` with `message`.
pub(crate) fn type_mismatch(message: impl Into<String>) -> MqlError {
    MqlError::new(codes::TYPE_MISMATCH, message)
}

/// The result of this crate's fallible operations.
pub type Result<T> = std::result::Result<T, MqlError>;
