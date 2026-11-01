//! `$changeStreamSplitLargeEvent` (014 FR-011): an event larger than a document may be is
//! returned in fragments. As a `refrouter` 9.0.4 does (measured on 01-11), the fields other than
//! `_id` go by the size of their element, smallest first, into fragments of at most
//! [`MAX_EVENT_BYTES`]; each fragment starts with its own `_id` (the event's token with the
//! fragment's number) and `splitEvent: {fragment, of}`. Without the stage, an event larger than
//! [`MAX_SERIALIZED_BYTES`] fails.

use teseladb_bson::{BsonError, Document, Limits, Value};

use crate::token::ResumeToken;

/// The largest event returned whole, and the largest fragment: a document's limit.
pub const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;
/// The largest document a reply serializes (16 MiB and 16 KiB, measured: `Size … exceeds
/// maximum 16793600`).
pub const MAX_SERIALIZED_BYTES: usize = MAX_EVENT_BYTES + 16 * 1024;

/// Why an event cannot be split.
#[derive(Clone, Debug, PartialEq)]
pub enum SplitError {
    /// It does not serialize.
    Bson(BsonError),
    /// One of its fields alone does not fit in a fragment: its size.
    FieldTooLarge(usize),
}

/// Nesting an event may add to a stored document's: `fullDocument` and its namespace.
const EVENT_EXTRA_DEPTH: usize = 2;

/// The bytes of `doc` serialized, however large.
pub fn size(doc: &Document) -> Result<usize, BsonError> {
    let limits =
        Limits { max_size: usize::MAX, max_depth: teseladb_bson::MAX_DEPTH + EVENT_EXTRA_DEPTH };
    Ok(doc.to_bytes_with(limits)?.len())
}

/// The bytes of field `k` with value `v` as an element of a document.
fn element_size(k: &str, v: &Value) -> Result<usize, BsonError> {
    let one = size(&Document::new().with(k, v.clone()))?;
    // The document's length (4) and terminator (1) are not the element's.
    Ok(one - 5)
}

/// The fragments of event `doc` with token `token`, or the event alone when it fits.
pub fn split(
    doc: &Document,
    token: &ResumeToken,
) -> Result<Vec<(ResumeToken, Document)>, SplitError> {
    if size(doc).map_err(SplitError::Bson)? <= MAX_EVENT_BYTES {
        return Ok(vec![(token.clone(), doc.clone())]);
    }
    let mut fields: Vec<(usize, &str, &Value)> = Vec::new();
    for (k, v) in doc.iter().filter(|(k, _)| *k != "_id") {
        fields.push((element_size(k, v).map_err(SplitError::Bson)?, k, v));
    }
    fields.sort_by_key(|(n, ..)| *n);
    // The head of a fragment: `_id` with a token of a large fragment number, and `splitEvent`.
    let head = |n: u32, of: u32| -> Result<Document, BsonError> {
        let t = ResumeToken { fragment: Some(n), ..token.clone() };
        let int = |x: u32| i32::try_from(x).unwrap_or(i32::MAX);
        let split = Document::new().with("fragment", int(n)).with("of", int(of));
        Ok(Document::new()
            .with("_id", Document::new().with("_data", t.encode()))
            .with("splitEvent", split))
    };
    let head_size =
        head(u32::MAX, u32::MAX).and_then(|h| h.to_bytes()).map_err(SplitError::Bson)?.len();
    let mut groups: Vec<Vec<(&str, &Value)>> = vec![Vec::new()];
    let mut used = head_size;
    for (n, k, v) in fields {
        if head_size + n > MAX_EVENT_BYTES {
            return Err(SplitError::FieldTooLarge(n));
        }
        if used + n > MAX_EVENT_BYTES {
            groups.push(Vec::new());
            used = head_size;
        }
        used += n;
        if let Some(g) = groups.last_mut() {
            g.push((k, v));
        }
    }
    let of = u32::try_from(groups.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(groups.len());
    for (i, group) in groups.into_iter().enumerate() {
        let n = u32::try_from(i + 1).unwrap_or(u32::MAX);
        let mut d = head(n, of).map_err(SplitError::Bson)?;
        for (k, v) in group {
            d.push(k, v.clone());
        }
        out.push((ResumeToken { fragment: Some(n), ..token.clone() }, d));
    }
    Ok(out)
}
