//! Extended JSON v2 of RefDb's specification (FR-015): writing a document in its canonical form
//! (every BSON type kept) or its relaxed one (plain JSON numbers and ISO dates where nothing is
//! lost for a reader), and reading either back. Doubles print in their shortest form that reads
//! back the same, with `.0` or an `E±` exponent.
//!
//! ```
//! use teseladb_bson::{Document, RawDoc, extjson};
//!
//! let bytes = Document::new().with("n", 1).with("x", 1.5).to_bytes().unwrap();
//! let raw = RawDoc::from_bytes(&bytes).unwrap();
//! assert_eq!(extjson::to_canonical(raw), r#"{"n":{"$numberInt":"1"},"x":{"$numberDouble":"1.5"}}"#);
//! assert_eq!(extjson::to_relaxed(raw), r#"{"n":1,"x":1.5}"#);
//! assert_eq!(extjson::parse(&extjson::to_canonical(raw)).unwrap().to_bytes().unwrap(), bytes);
//! ```

use std::fmt;

use crate::raw::RawDoc;
use crate::read::Limits;
use crate::value::Document;

pub mod base64;
mod date;
mod legacy;
mod parse;
mod write;

pub use date::to_log_time;

/// Text that is not Extended JSON of a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtJsonError(String);

impl ExtJsonError {
    /// RefDb's error code and name for it.
    pub fn code(&self) -> (i32, &'static str) {
        (2, "BadValue")
    }
}

impl fmt::Display for ExtJsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not Extended JSON: {}", self.0)
    }
}

impl std::error::Error for ExtJsonError {}

/// The document in canonical Extended JSON.
pub fn to_canonical(doc: RawDoc<'_>) -> String {
    let mut out = String::new();
    write::document(&mut out, doc, write::Mode::Canonical);
    out
}

/// The document in relaxed Extended JSON.
pub fn to_relaxed(doc: RawDoc<'_>) -> String {
    let mut out = String::new();
    write::document(&mut out, doc, write::Mode::Relaxed);
    out
}

/// The document in relaxed Extended JSON with UUIDs as `$uuid`, the form of RefDb's OCSF
/// audit log (013 FR-031).
pub fn to_relaxed_uuid(doc: RawDoc<'_>) -> String {
    let mut out = String::new();
    write::document(&mut out, doc, write::Mode::RelaxedUuid);
    out
}

/// A UUID's 16 bytes as `8-4-4-4-12` hexadecimal digits.
pub fn uuid_text(bytes: &[u8; 16]) -> String {
    write::uuid_text(bytes)
}

/// The document in the legacy strict JSON of RefDb's audit log (`{ "k" : 1 }`).
pub fn to_legacy(doc: RawDoc<'_>) -> String {
    let mut out = String::new();
    legacy::document(&mut out, doc);
    out
}

/// Reads a document from canonical or relaxed Extended JSON.
pub fn parse(text: &str) -> Result<Document, ExtJsonError> {
    parse_with(text, Limits::default())
}

/// Reads a document within `limits` instead of a stored document's (a request body of the REST
/// API, 019, may hold up to `maxMessageSizeBytes`).
pub fn parse_with(text: &str, limits: Limits) -> Result<Document, ExtJsonError> {
    // The JSON may nest one level more than the BSON: a type wrapper at the deepest level.
    let json_limits = teseladb_json::Limits {
        max_depth: limits.max_depth + 1,
        ..teseladb_json::Limits::default()
    };
    let json = teseladb_json::parse_with(text.as_bytes(), json_limits)
        .map_err(|e| ExtJsonError(e.to_string()))?;
    let object =
        json.as_object().ok_or_else(|| ExtJsonError("the top level is not a document".into()))?;
    let doc = parse::document(object)?;
    // What BSON cannot hold (too large, too deep) is refused here and not when it is written.
    doc.to_bytes_with(limits).map_err(|e| ExtJsonError(e.to_string()))?;
    Ok(doc)
}
