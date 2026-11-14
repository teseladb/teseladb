//! Media types (FR-006, contract `rest-api.md`): Extended JSON v2 relaxed (the default),
//! canonical, or BSON, chosen by `Content-Type` for the body and by `Accept` for the reply
//! (RFC 9110, sections 8.3 and 12.5.1). A body and a reply may hold up to `maxMessageSizeBytes`,
//! like a message of the wire protocol, although a stored document stops at 16 MiB.

use teseladb_bson::{Document, Limits, MAX_DEPTH, RawDoc, extjson};
use teseladb_wire::MAX_MESSAGE_SIZE;

/// Relaxed Extended JSON.
pub const JSON: &str = "application/json";
/// Canonical Extended JSON.
pub const CANONICAL_JSON: &str = "application/vnd.teseladb.ejson-canonical+json";
/// BSON.
pub const BSON: &str = "application/bson";
/// Any type.
const ANY: &str = "*/*";
/// Any application type.
const ANY_APPLICATION: &str = "application/*";

/// The format of a body or a reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// `application/json`.
    Relaxed,
    /// `application/vnd.teseladb.ejson-canonical+json`.
    Canonical,
    /// `application/bson`.
    Bson,
}

impl Format {
    const ALL: [Format; 3] = [Format::Relaxed, Format::Canonical, Format::Bson];

    /// Its media type.
    pub fn media_type(self) -> &'static str {
        match self {
            Format::Relaxed => JSON,
            Format::Canonical => CANONICAL_JSON,
            Format::Bson => BSON,
        }
    }

    fn from_media_type(t: &str) -> Option<Format> {
        Format::ALL.into_iter().find(|f| f.media_type().eq_ignore_ascii_case(t))
    }
}

/// Why a body or a reply cannot be handled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaError {
    /// `Content-Type` is none of the formats: `415`.
    UnsupportedMediaType,
    /// `Accept` admits none of the formats: `406`.
    NotAcceptable,
    /// The body does not read in its format: `400` with `BadValue`.
    BadBody(String),
    /// The reply does not fit in `maxMessageSizeBytes`: `500`.
    ReplyTooLarge,
}

impl MediaError {
    /// The HTTP status.
    pub fn status(&self) -> u16 {
        match self {
            MediaError::UnsupportedMediaType => 415,
            MediaError::NotAcceptable => 406,
            MediaError::BadBody(_) => 400,
            MediaError::ReplyTooLarge => 500,
        }
    }
}

/// The media type of a header element, without parameters.
fn essence(t: &str) -> &str {
    t.split(';').next().unwrap_or_default().trim()
}

/// The format of a request body; without `Content-Type`, relaxed Extended JSON.
pub fn request_format(content_type: Option<&str>) -> Result<Format, MediaError> {
    match content_type {
        None => Ok(Format::Relaxed),
        Some(t) => Format::from_media_type(essence(t)).ok_or(MediaError::UnsupportedMediaType),
    }
}

/// `q` of an `Accept` element: 1 when absent or unreadable, clamped to 0..=1.
fn quality(element: &str) -> f64 {
    element
        .split(';')
        .skip(1)
        .filter_map(|p| p.trim().strip_prefix("q="))
        .find_map(|q| q.trim().parse::<f64>().ok())
        .map_or(1.0, |q| q.clamp(0.0, 1.0))
}

/// The format of the reply: the admitted format with the highest `q`, the first listed on a tie;
/// wildcards choose relaxed Extended JSON. Without `Accept`, relaxed Extended JSON.
pub fn response_format(accept: Option<&str>) -> Result<Format, MediaError> {
    let Some(accept) = accept else { return Ok(Format::Relaxed) };
    let mut best: Option<(f64, Format)> = None;
    for element in accept.split(',') {
        let t = essence(element);
        let format = if t == ANY || t.eq_ignore_ascii_case(ANY_APPLICATION) {
            Some(Format::Relaxed)
        } else {
            Format::from_media_type(t)
        };
        let q = quality(element);
        if let Some(f) = format.filter(|_| q > 0.0)
            && best.is_none_or(|(bq, _)| q > bq)
        {
            best = Some((q, f));
        }
    }
    best.map(|(_, f)| f).ok_or(MediaError::NotAcceptable)
}

fn message_limits() -> Limits {
    Limits { max_size: MAX_MESSAGE_SIZE, max_depth: MAX_DEPTH }
}

/// Reads a request body in `format`.
pub fn decode(body: &[u8], format: Format) -> Result<Document, MediaError> {
    match format {
        Format::Bson => RawDoc::from_bytes_with(body, message_limits())
            .map(Document::from)
            .map_err(|e| MediaError::BadBody(format!("invalid BSON: {e}"))),
        Format::Relaxed | Format::Canonical => {
            let text = std::str::from_utf8(body)
                .map_err(|_| MediaError::BadBody("the body is not UTF-8".into()))?;
            extjson::parse_with(text, message_limits())
                .map_err(|e| MediaError::BadBody(e.to_string()))
        }
    }
}

/// Writes a reply in `format`.
pub fn encode(doc: &Document, format: Format) -> Result<Vec<u8>, MediaError> {
    let limits = message_limits();
    let bytes = doc.to_bytes_with(limits).map_err(|_| MediaError::ReplyTooLarge)?;
    let raw = RawDoc::from_bytes_with(&bytes, limits).map_err(|_| MediaError::ReplyTooLarge)?;
    Ok(match format {
        Format::Bson => bytes,
        Format::Relaxed => extjson::to_relaxed(raw).into_bytes(),
        Format::Canonical => extjson::to_canonical(raw).into_bytes(),
    })
}
