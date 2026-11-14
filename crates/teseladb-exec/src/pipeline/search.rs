//! The search stages `$search`, `$searchMeta` and `$vectorSearch` (011 FR-022): first stages
//! whose documents come from the node's search indexes through [`super::Env::search_source`].
//! The pipeline only checks that the argument is an object; the search engine checks the rest.
//! Measured on a `refrouter` 9.0.4 without search and on `refsearch` 1.70.6.

use std::collections::BTreeMap;

use teseladb_bson::{Document, RawValue, Value};
use teseladb_mql::{ErrorCode, Filter, FilterUse, MqlError};

use super::errors::{FAILED_TO_PARSE, error};
use crate::expr::arith::raw_type_name;

/// The search stages.
pub const SEARCH_STAGES: [&str; 3] = ["$search", "$searchMeta", "$vectorSearch"];

/// RefDb's error when the deployment has no search.
const SEARCH_NOT_ENABLED: ErrorCode = ErrorCode { code: 31082, name: "SearchNotEnabled" };

/// A document a search stage gives, with its metadata for `$meta` (`searchScore`,
/// `searchScoreDetails`, `searchHighlights`, `searchSequenceToken`, `vectorSearchScore`).
#[derive(Clone, Debug, PartialEq)]
pub struct Scored {
    /// The document.
    pub doc: Document,
    /// Its metadata by `$meta` name.
    pub meta: BTreeMap<String, Value>,
}

/// The error of a deployment without search, whatever the stage.
pub fn not_enabled() -> MqlError {
    error(SEARCH_NOT_ENABLED, teseladb_names::ERR_SEARCH_NOT_CONFIGURED)
}

/// RefDb's error when a field of a stage has the wrong type.
const WRONG_TYPE: ErrorCode = ErrorCode { code: 13_111, name: "Location13111" };

/// Checks the argument of search stage `name`: an object, whose `scoreDetails` is a boolean in
/// `$search` (measured on 9.0.4: `refnode` checks it before it asks `refsearch`).
pub fn parse(name: &str, arg: RawValue<'_>) -> Result<Document, MqlError> {
    match arg {
        RawValue::Document(d) => {
            if name == "$search"
                && let Some(v) = d.get("scoreDetails")
                && !matches!(v, RawValue::Boolean(_))
            {
                return Err(error(
                    WRONG_TYPE,
                    format!("wrong type for field (scoreDetails) {} != bool", raw_type_name(v)),
                ));
            }
            Ok(Document::from(d))
        }
        other => Err(error(
            FAILED_TO_PARSE,
            format!("{name} value must be an object. Found: {}", raw_type_name(other)),
        )),
    }
}

/// RefDb's error when `$vectorSearch`'s `limit` is not a number.
const LIMIT_NOT_NUMBER: ErrorCode = ErrorCode { code: 8_575_100, name: "Location8575100" };
/// RefDb's error when `$vectorSearch`'s `limit` is not positive.
const LIMIT_NOT_POSITIVE: ErrorCode = ErrorCode { code: 7_912_700, name: "Location7912700" };
/// RefDb's error when `$vectorSearch`'s `filter` is not an object.
const FILTER_NOT_OBJECT: ErrorCode = ErrorCode { code: 10_065, name: "Location10065" };

/// Checks the argument of `$vectorSearch` as `refnode` does before it asks `refsearch`: an object
/// whose `limit`, when given, is a positive number and whose `filter`, when given, is a valid
/// query filter (measured on 9.0.4: `refsearch` checks everything else, and a missing `limit` is
/// its error).
pub fn parse_vector(arg: RawValue<'_>) -> Result<Document, MqlError> {
    let RawValue::Document(raw) = arg else { return parse("$vectorSearch", arg) };
    let spec = parse("$vectorSearch", arg)?;
    match raw.get("limit") {
        None => {}
        Some(v) => {
            let n = match v {
                RawValue::Int32(n) => f64::from(n),
                RawValue::Int64(n) => n as f64,
                RawValue::Double(d) => d,
                RawValue::Decimal128(d) => teseladb_mql::decimal_to_f64(d),
                _ => {
                    return Err(error(
                        LIMIT_NOT_NUMBER,
                        "Expected limit field to be a number in $vectorSearch",
                    ));
                }
            };
            if n.is_nan() || n <= 0.0 {
                return Err(error(LIMIT_NOT_POSITIVE, "Expected limit to be positive"));
            }
        }
    }
    match raw.get("filter") {
        None => {}
        Some(RawValue::Document(f)) => drop(Filter::parse(f, FilterUse::Query)?),
        Some(_) => {
            return Err(error(FILTER_NOT_OBJECT, "invalid parameter: expected an object (filter)"));
        }
    }
    Ok(spec)
}

/// The stage name RefDb gives when a search stage is not first: `$search` runs as
/// `$_internalSearchRefSearchRemote`.
pub fn first_stage_name(name: &str) -> &str {
    if name == "$search" { teseladb_names::STAGE_INTERNAL_SEARCH_REMOTE } else { name }
}
