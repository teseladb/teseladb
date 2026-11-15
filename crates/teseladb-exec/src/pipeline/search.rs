//! The search stages `$search`, `$searchMeta` and `$vectorSearch` (011 FR-022): first stages
//! whose documents come from the node's search indexes through [`super::Env::search_source`].
//! The pipeline only checks that the argument is an object; the search engine checks the rest.
//! Measured on a reference router 9.0.4 without search and on the reference search server 1.70.6.

use std::collections::BTreeMap;

use teseladb_bson::{Document, RawValue, Value};
use teseladb_mql::{ErrorCode, Filter, FilterUse, MqlError};

use super::errors::{FAILED_TO_PARSE, error};
use crate::expr::arith::raw_type_name;

/// The search stages.
pub const SEARCH_STAGES: [&str; 3] = ["$search", "$searchMeta", "$vectorSearch"];

/// The reference server's error when the deployment has no search.
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

/// The results a stage after a search reads at a time (011 FR-031): their documents are read
/// together, when the first of those stages asks for them.
pub const SEARCH_BATCH: usize = 64;

/// What reads the documents of results `from..to` of a [`Lazy`], in order.
pub type Loader<'a> = Box<dyn FnMut(usize, usize) -> Result<Vec<Document>, MqlError> + 'a>;

/// The results of a search stage whose documents are read only when a stage asks for them
/// (011 FR-031): each result's metadata, in order, and the reader of their documents.
pub struct Lazy<'a> {
    /// Each result's metadata for `$meta`.
    pub metas: Vec<BTreeMap<String, Value>>,
    load: Loader<'a>,
    /// Documents read so far, for tests and metrics.
    loaded: usize,
}

impl<'a> Lazy<'a> {
    /// Results with `metas` whose documents `load` reads.
    pub fn new(metas: Vec<BTreeMap<String, Value>>, load: Loader<'a>) -> Lazy<'a> {
        Lazy { metas, load, loaded: 0 }
    }

    /// Results whose documents are already read.
    pub fn eager(found: Vec<Scored>) -> Lazy<'a> {
        let (docs, metas): (Vec<Document>, Vec<_>) =
            found.into_iter().map(|s| (s.doc, s.meta)).unzip();
        let mut docs: Vec<Option<Document>> = docs.into_iter().map(Some).collect();
        let load = move |from: usize, to: usize| {
            Ok(docs[from..to].iter_mut().map(|d| d.take().unwrap_or_default()).collect())
        };
        Lazy::new(metas, Box::new(load))
    }

    /// How many results there are.
    pub fn len(&self) -> usize {
        self.metas.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.metas.is_empty()
    }

    /// The documents of results `from..to`, each read once.
    pub fn load(&mut self, from: usize, to: usize) -> Result<Vec<Document>, MqlError> {
        let docs = (self.load)(from, to)?;
        self.loaded += docs.len();
        Ok(docs)
    }

    /// How many documents have been read.
    pub fn loaded(&self) -> usize {
        self.loaded
    }
}

/// The error of a deployment without search, whatever the stage.
pub fn not_enabled() -> MqlError {
    error(SEARCH_NOT_ENABLED, teseladb_names::ERR_SEARCH_NOT_CONFIGURED)
}

/// The reference server's error when a field of a stage has the wrong type.
const WRONG_TYPE: ErrorCode = ErrorCode { code: 13_111, name: "Location13111" };

/// Checks the argument of search stage `name`: an object, whose `scoreDetails` is a boolean in
/// `$search` (measured on 9.0.4: the reference server checks it before it asks the reference search server).
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

/// The reference server's error when `$vectorSearch`'s `limit` is not a number.
const LIMIT_NOT_NUMBER: ErrorCode = ErrorCode { code: 8_575_100, name: "Location8575100" };
/// The reference server's error when `$vectorSearch`'s `limit` is not positive.
const LIMIT_NOT_POSITIVE: ErrorCode = ErrorCode { code: 7_912_700, name: "Location7912700" };
/// The reference server's error when `$vectorSearch`'s `filter` is not an object.
const FILTER_NOT_OBJECT: ErrorCode = ErrorCode { code: 10_065, name: "Location10065" };

/// Checks the argument of `$vectorSearch` as the reference server does before it asks the reference search server: an object
/// whose `limit`, when given, is a positive number and whose `filter`, when given, is a valid
/// query filter (measured on 9.0.4: the reference search server checks everything else, and a missing `limit` is
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

/// The stage name the reference server gives when a search stage is not first: `$search` runs as
/// [`teseladb_names::STAGE_INTERNAL_SEARCH_REMOTE`].
pub fn first_stage_name(name: &str) -> &str {
    if name == "$search" { teseladb_names::STAGE_INTERNAL_SEARCH_REMOTE } else { name }
}
