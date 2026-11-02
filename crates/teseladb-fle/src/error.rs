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
    /// An integer option given as a fractional double.
    FAILED_TO_PARSE = 9, "FailedToParse";
    /// A validator with `encrypt` on a collection with encrypted fields.
    QUERY_FEATURE_NOT_ALLOWED = 224, "QueryFeatureNotAllowed";
    /// An empty encrypted path.
    EMPTY_PATH = 6_316_402, "Location6316402";
    /// A type no encrypted field takes.
    UNINDEXED_TYPE = 6_338_406, "Location6338406";
    /// Encrypted fields on a view or a time series.
    VIEW_OR_TIMESERIES = 6_346_401, "Location6346401";
    /// Encrypted fields on a capped collection.
    CAPPED = 6_367_301, "Location6367301";
    /// An indexed field without `bsonType`.
    INDEXED_WITHOUT_TYPE = 6_412_601, "Location6412601";
    /// A range minimum not below its maximum.
    MIN_NOT_BELOW_MAX = 6_720_005, "Location6720005";
    /// A range option on an equality field.
    EQUALITY_OPTION = 6_775_205, "Location6775205";
    /// `min` or `max` on an equality field.
    EQUALITY_BOUND = 6_775_206, "Location6775206";
    /// An ECOC whose name is not the one of its collection.
    ECOC_NAME = 7_406_902, "Location7406902";
    /// A double minimum with more decimals than the precision.
    DOUBLE_MIN_DIGITS = 6_966_805, "Location6966805";
    /// A double maximum with more decimals than the precision.
    DOUBLE_MAX_DIGITS = 6_966_806, "Location6966806";
    /// A decimal minimum with more decimals than the precision.
    DECIMAL_MIN_DIGITS = 6_966_807, "Location6966807";
    /// A decimal maximum with more decimals than the precision.
    DECIMAL_MAX_DIGITS = 6_966_808, "Location6966808";
    /// A scaled double maximum past 2^63.
    DOUBLE_UPPER_BOUND = 9_178_803, "Location9178803";
    /// A scaled double minimum past 2^63.
    DOUBLE_LOWER_BOUND = 9_178_804, "Location9178804";
    /// A scaled decimal maximum past 2^127.
    DECIMAL_UPPER_BOUND = 9_178_810, "Location9178810";
    /// A scaled decimal minimum past 2^127.
    DECIMAL_LOWER_BOUND = 9_178_811, "Location9178811";
    /// Sparsity and trim factor that make queries too large.
    QUERY_TOO_LARGE = 9_203_504, "Location9203504";
    /// Sparsity, trim factor and bounds that make queries too large.
    QUERY_TOO_LARGE_BOUNDS = 9_203_508, "Location9203508";
    /// A trim factor outside the domain.
    TRIM_FACTOR = 8_574_000, "Location8574000";
    /// A text option on an equality field.
    EQUALITY_TEXT_OPTION = 10_774_903, "Location10774903";
    /// A text option on a range field.
    RANGE_TEXT_OPTION = 10_774_908, "Location10774908";
    /// A range option on a text field.
    TEXT_RANGE_OPTION = 10_774_911, "Location10774911";
    /// More tags than a value may have.
    TAG_LIMIT = 10_384_602, "Location10384602";
    /// A substring query shorter than 2.
    SUBSTRING_MIN = 12_860_001, "Location12860001";
    /// A substring query longer than 6.
    SUBSTRING_MAX = 12_860_002, "Location12860002";
    /// A substring field longer than 50.
    SUBSTRING_LENGTH = 12_860_003, "Location12860003";
    /// `strMaxQueryLength` missing on a text field.
    TEXT_MAX_MISSING = 9_783_403, "Location9783403";
    /// `caseSensitive` missing on a text field.
    TEXT_CASE_MISSING = 9_783_404, "Location9783404";
    /// `diacriticSensitive` missing on a text field.
    TEXT_DIACRITIC_MISSING = 9_783_405, "Location9783405";
    /// `strMaxLength` missing on a substring field.
    SUBSTRING_LENGTH_MISSING = 9_783_407, "Location9783407";
    /// `strMaxQueryLength` above `strMaxLength`.
    QUERY_ABOVE_LENGTH = 9_783_408, "Location9783408";
    /// `strMinQueryLength` above `strMaxQueryLength`.
    MIN_ABOVE_MAX_QUERY = 9_783_406, "Location9783406";
    /// Different case sensitivity across the text queries of a field.
    CASE_DIFFERS = 9_783_409, "Location9783409";
    /// Different diacritic sensitivity across the text queries of a field.
    DIACRITIC_DIFFERS = 9_783_410, "Location9783410";
    /// Different contention across the text queries of a field.
    CONTENTION_DIFFERS = 9_783_411, "Location9783411";
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
