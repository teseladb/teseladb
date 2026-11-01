//! The error codes of pipelines and their stages, with RefDb's names (measured on a `refrouter`
//! 9.0.4).

use teseladb_bson::Document;
use teseladb_bson::RawValue;
use teseladb_mql::{ErrorCode, MqlError};

use crate::expr::arith::raw_type_name;

/// A location code: its number and `LocationNNNNN`.
macro_rules! location {
    ($($name:ident = $n:literal),* $(,)?) => {
        $(
            #[doc = concat!("RefDb's location code ", stringify!($n), ".")]
            pub const $name: ErrorCode = ErrorCode { code: $n, name: concat!("Location", $n) };
        )*
    };
}

/// `BadValue`.
pub const BAD_VALUE: ErrorCode = ErrorCode { code: 2, name: "BadValue" };
/// `FailedToParse`.
pub const FAILED_TO_PARSE: ErrorCode = ErrorCode { code: 9, name: "FailedToParse" };
/// `TypeMismatch`.
pub const TYPE_MISMATCH: ErrorCode = ErrorCode { code: 14, name: "TypeMismatch" };
/// `IndexNotFound`.
pub const INDEX_NOT_FOUND: ErrorCode = ErrorCode { code: 27, name: "IndexNotFound" };
/// `InvalidNamespace`.
pub const INVALID_NAMESPACE: ErrorCode = ErrorCode { code: 73, name: "InvalidNamespace" };

location!(
    N_NOT_OBJECT = 5_787_801,
    N_UNKNOWN = 5_787_901,
    N_INPUT_MISSING = 5_787_907,
    TOP_NOT_OBJECT = 5_788_001,
    TOP_UNKNOWN = 5_788_002,
    TOP_N_MISSING = 5_788_003,
    TOP_OUTPUT_MISSING = 5_788_004,
    GROUP_SPEC_NOT_OBJECT = 15947,
    BUCKET_NOT_OBJECT = 40201,
    BUCKET_AUTO_NOT_OBJECT = 40240,
    ADD_FIELDS_NOT_OBJECT = 40272,
    REPLACE_ROOT_NOT_OBJECT = 40229,
    SAMPLE_NOT_OBJECT = 28745,
    UNWIND_NOT_OBJECT = 15981,
    SORT_BY_COUNT_TYPE = 40149,
    PROJECT_POSITIONAL = 31324,
    TEXT_NOT_FIRST = 17313,
    MATCH_NOT_OBJECT = 15959,
    PROJECT_NOT_OBJECT = 15969,
    PROJECT_EMPTY = 51272,
    PROJECT_EMPTY_SUB = 51270,
    PROJECT_COLLISION = 31249,
    UNKNOWN_EXPRESSION = 31325,
    PROJECT_EXCLUSION_IN_INCLUSION = 31254,
    PROJECT_INCLUSION_IN_EXCLUSION = 31253,
    FIELD_DOLLAR = 16410,
    PATH_DOT_END = 40353,
    UNSET_EMPTY = 31119,
    UNSET_TYPE = 31002,
    NEW_ROOT = 40228,
    MISSING_FIELD = 40414,
    UNKNOWN_FIELD = 40415,
    SORT_EMPTY = 15976,
    SORT_ORDER = 15975,
    SORT_ILLEGAL_META = 31138,
    SORT_NOT_OBJECT = 15973,
    LIMIT_ZERO = 15958,
    LIMIT_ARGUMENT = 5_107_201,
    SKIP_ARGUMENT = 5_107_200,
    COUNT_TYPE = 40156,
    COUNT_EMPTY = 40157,
    COUNT_DOLLAR = 40158,
    COUNT_DOT = 40160,
    COUNT_ID = 9_039_800,
    UNWIND_PREFIX = 28818,
    UNWIND_INDEX_PREFIX = 28822,
    UNWIND_OPTION = 28811,
    UNWIND_NO_PATH = 28812,
    UNWIND_PRESERVE = 28809,
    SORT_BY_COUNT_OBJECT = 40147,
    SORT_BY_COUNT_FIELD = 40148,
    REDACT_RESULT = 17053,
    SAMPLE_SIZE = 28747,
    SAMPLE_MISSING = 28749,
    FACET_EMPTY = 40169,
    FACET_ARRAY = 40170,
    FACET_STAGE = 40600,
    FIRST_STAGE = 40602,
    INDEX_STATS_EMPTY = 28803,
    COLL_STATS_OBJECT = 5_447_000,
    BUCKET_UNSORTED = 40194,
    BUCKET_FEW = 40192,
    BUCKET_GROUP_BY = 40202,
    BUCKET_TYPES = 40193,
    BUCKET_DEFAULT = 40199,
    BUCKET_NO_MATCH = 7_158_303,
    BUCKET_NO_MATCH_CLASSIC = 40066,
    BUCKET_AUTO_BUCKETS = 40243,
    BUCKET_AUTO_NUMERIC = 40241,
    BUCKET_AUTO_INT = 40242,
    BUCKET_AUTO_REQUIRED = 40246,
    BUCKET_AUTO_OUTPUT = 40244,
    BUCKET_REQUIRED = 40198,
    BUCKET_BOUNDARIES_ARRAY = 40200,
    BUCKET_OUTPUT = 40196,
    GROUP_ID = 15955,
    GROUP_ONE_ACCUMULATOR = 40238,
    GROUP_UNKNOWN = 15952,
    GROUP_NOT_OBJECT = 40234,
    GROUP_DOT = 40235,
    GROUP_DOLLAR = 40236,
    GROUP_UNARY = 40237,
    MERGE_OBJECTS = 5_911_200,
    N_POSITIVE = 7_548_606,
    N_MISSING = 5_787_906,
    SORT_BY_MISSING = 5_788_005,
    NEAR_IN_MATCH = 5_626_500,
    OUT_TYPE = 16990,
    OUT_SPECIAL = 17385,
    MERGE_SPECIAL = 31319,
    MERGE_INTO_TYPE = 51178,
    MERGE_ON_TYPE = 51186,
    MERGE_ON_EMPTY = 51187,
    MERGE_ON_ELEMENT = 51134,
    MERGE_WHEN_MATCHED_TYPE = 51191,
    MERGE_MODES = 51181,
    MERGE_INTO_EMPTY = 5_786_800,
    MERGE_INTO_COLL = 5_786_801,
    WRITE_NOT_LAST = 40601,
    WRITE_IN_LOOKUP = 51047,
    WRITE_IN_UNION = 31441,
);

/// `$merge` with `whenNotMatched: fail` and a document without a target.
pub const MERGE_NO_MATCH: ErrorCode =
    ErrorCode { code: 13113, name: "MergeStageNoMatchingDocument" };
/// `InvalidOptions`: a stage an update's pipeline does not take.
pub const INVALID_OPTIONS: ErrorCode = ErrorCode { code: 72, name: "InvalidOptions" };
/// RefDb's `QueryExceededMemoryLimitNoDiskUseAllowed` (292).
pub const NO_DISK_USE: ErrorCode =
    ErrorCode { code: 292, name: "QueryExceededMemoryLimitNoDiskUseAllowed" };

/// The memory a stage may hold (`internalQueryMaxBlockingSortMemoryUsageBytes` and the limit of
/// `$group`, 100 MiB).
pub const STAGE_MEMORY_LIMIT: usize = 104_857_600;

/// The bytes of `docs` in BSON, whatever their size.
pub(crate) fn bytes_of(docs: &[Document]) -> usize {
    docs.iter().map(document_size).sum()
}

/// The bytes of document `d` in BSON.
pub(crate) fn document_size(d: &Document) -> usize {
    5 + d.iter().map(|(k, v)| 2 + k.len() + value_size(v)).sum::<usize>()
}

/// The bytes of value `v` in BSON (without its type byte and name).
pub(crate) fn value_size(v: &teseladb_bson::Value) -> usize {
    use teseladb_bson::Value as V;
    match v {
        V::Double(_) | V::Int64(_) | V::DateTime(_) | V::Timestamp { .. } => 8,
        V::Int32(_) => 4,
        V::Decimal128(_) => 16,
        V::Boolean(_) => 1,
        V::ObjectId(_) => 12,
        V::String(s) | V::Symbol(s) | V::JavaScript(s) => 5 + s.len(),
        V::Binary { bytes, .. } => 5 + bytes.len(),
        V::Document(d) => document_size(d),
        V::Array(items) => {
            5 + items
                .iter()
                .enumerate()
                .map(|(i, x)| 2 + i.to_string().len() + value_size(x))
                .sum::<usize>()
        }
        _ => 16,
    }
}

/// The 292 of a sort over more than [`STAGE_MEMORY_LIMIT`] bytes of `docs` when the command does
/// not allow writing to disk (measured on a `refrouter` 9.0.4).
pub(crate) fn check_sort_memory(docs: &[Document], allow_disk_use: bool) -> Result<(), MqlError> {
    if allow_disk_use || bytes_of(docs) <= STAGE_MEMORY_LIMIT {
        return Ok(());
    }
    Err(error(
        NO_DISK_USE,
        format!(
            "Sort exceeded memory limit of {STAGE_MEMORY_LIMIT} bytes, but did not opt in to external sorting."
        ),
    ))
}

/// RefDb's `UnsupportedFormat` (12).
pub const UNSUPPORTED_FORMAT: ErrorCode = ErrorCode { code: 12, name: "UnsupportedFormat" };

/// An error with `code` and `msg`.
pub fn error(code: ErrorCode, msg: impl Into<String>) -> MqlError {
    MqlError::new(code, msg)
}

/// The `TypeMismatch` of a stage argument `field` holding `v` where `expected` (`"type
/// 'string'"`, `"types '[object, string]'"`) was due.
pub fn wrong_type(field: &str, v: RawValue<'_>, expected: &str) -> MqlError {
    error(
        TYPE_MISMATCH,
        format!(
            "BSON field '{field}' is the wrong type '{}', expected {expected}",
            raw_type_name(v)
        ),
    )
}

/// The `40414` of a missing required argument `field` of `stage`.
pub fn missing_field(stage: &str, field: &str) -> MqlError {
    error(MISSING_FIELD, format!("BSON field '{stage}.{field}' is missing but a required field"))
}

/// The `40415` of an argument `field` of `stage` it does not know.
pub fn unknown_field(stage: &str, field: &str) -> MqlError {
    error(UNKNOWN_FIELD, format!("BSON field '{stage}.{field}' is an unknown field."))
}
