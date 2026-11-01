//! The search stages `$search`, `$searchMeta` and `$vectorSearch` (011 FR-022): first stages
//! whose documents come from the node's search indexes through [`super::Env::search_source`].
//! The pipeline only checks that the argument is an object; the search engine checks the rest.
//! Measured on a `refrouter` 9.0.4 without search and on `refsearch` 1.70.6.

use std::collections::BTreeMap;

use teseladb_bson::{Document, RawValue, Value};
use teseladb_mql::{ErrorCode, MqlError};

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
    error(
        SEARCH_NOT_ENABLED,
        "Using $search and $vectorSearch aggregation stages requires additional configuration. Please connect to Atlas or an RefCloud local deployment to enable.For more information on how to connect, see https://dochub.refdb.org/core/atlas-cli-deploy-local-reqs.",
    )
}

/// Checks the argument of search stage `name`: an object.
pub fn parse(name: &str, arg: RawValue<'_>) -> Result<Document, MqlError> {
    match arg {
        RawValue::Document(d) => Ok(Document::from(d)),
        other => Err(error(
            FAILED_TO_PARSE,
            format!("{name} value must be an object. Found: {}", raw_type_name(other)),
        )),
    }
}

/// The stage name RefDb gives when a search stage is not first: `$search` runs as
/// `$_internalSearchRefSearchRemote`.
pub fn first_stage_name(name: &str) -> &str {
    if name == "$search" { "$_internalSearchRefSearchRemote" } else { name }
}
