//! Documents built during execution read back as raw BSON (for the matcher, the sort keys and
//! the extensions), through one conversion: a document past BSON's limits (more levels or bytes
//! than a stored document may have) is a `BadValue`.

use teseladb_mql::MqlError;
use teseladb_bson::{BsonError, Document, MAX_DEPTH, MAX_SIZE, RawDoc, RawValue};

use crate::expr::codes::BAD_VALUE;

/// The `BadValue` of a document that cannot be written as BSON.
pub(crate) fn bad(e: BsonError) -> MqlError {
    MqlError::new(BAD_VALUE, e.to_string())
}

/// `doc` in BSON.
pub(crate) fn bytes_of(doc: &Document) -> Result<Vec<u8>, MqlError> {
    doc.to_bytes().map_err(bad)
}

/// Runs `f` over `doc` as raw BSON. A document that keeps the canonical bytes it was read from
/// (a stored one) is read where it is, without a copy or another validation (003 FR-020), once
/// it is known to be within a stored document's limits.
pub(crate) fn with_raw_doc<R>(
    doc: &Document,
    f: impl FnOnce(RawDoc<'_>) -> Result<R, MqlError>,
) -> Result<R, MqlError> {
    if let Some(kept) = doc.canonical_bytes() {
        let raw = RawDoc::from_stored(kept).map_err(bad)?;
        if within_stored_limits(raw) {
            return f(raw);
        }
    }
    let bytes = bytes_of(doc)?;
    f(RawDoc::from_bytes(&bytes).map_err(bad)?)
}

/// The fewest bytes one more level of nesting takes: the element's type, an empty name and an
/// empty document.
const LEVEL_BYTES: usize = 7;

/// Whether a valid document fits a stored document's size and nesting.
fn within_stored_limits(raw: RawDoc<'_>) -> bool {
    let len = raw.as_bytes().len();
    if len > MAX_SIZE {
        return false;
    }
    // A document this short cannot nest past the limit: no need to walk it.
    len / LEVEL_BYTES < MAX_DEPTH || levels(raw) <= MAX_DEPTH
}

/// Levels of `raw`, itself the first: documents, arrays and the scope of JavaScript code.
fn levels(raw: RawDoc<'_>) -> usize {
    let inner = raw.iter().map(|(_, v)| match v {
        RawValue::Document(d) | RawValue::JavaScriptWithScope { scope: d, .. } => levels(d),
        RawValue::Array(a) => levels(a.as_doc()),
        _ => 0,
    });
    1 + inner.max().unwrap_or(0)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use teseladb_bson::MAX_DEPTH;

    use super::*;

    /// A document nested `levels` deep.
    fn nested(levels: usize) -> Document {
        (1..levels).fold(Document::new().with("a", 1), |d, _| Document::new().with("a", d))
    }

    #[test]
    fn a_document_within_the_limits_is_read_back() {
        let doc = nested(3);
        let got = with_raw_doc(&doc, |raw| Ok(Document::from(raw))).expect("readable");
        assert_eq!(got, doc);
        assert_eq!(bytes_of(&doc).expect("small"), doc.to_bytes().expect("small"));
    }

    #[test]
    fn a_kept_document_is_read_from_its_own_bytes() {
        let bytes = nested(4).with("x", "y").to_bytes().expect("small");
        let kept = Document::from_stored(bytes.clone()).expect("framed");
        let at = with_raw_doc(&kept, |raw| Ok(raw.as_bytes().as_ptr())).expect("readable");
        assert_eq!(at, kept.canonical_bytes().expect("kept").as_ptr(), "no copy");
        let got = with_raw_doc(&kept, |raw| Ok(Document::from(raw))).expect("readable");
        assert_eq!(got, Document::from(RawDoc::from_bytes(&bytes).expect("valid")));
    }

    #[test]
    fn a_kept_document_past_the_depth_limit_is_a_bad_value() {
        let deep = nested(MAX_DEPTH + 1);
        let bytes = deep
            .to_bytes_with(teseladb_bson::Limits { max_size: usize::MAX, max_depth: usize::MAX })
            .expect("written");
        let kept = Document::from_canonical(
            RawDoc::from_bytes_with(
                &bytes,
                teseladb_bson::Limits { max_size: usize::MAX, max_depth: usize::MAX },
            )
            .expect("valid"),
        );
        let e = with_raw_doc(&kept, |_| Ok(())).expect_err("too deep");
        assert_eq!(e.code, BAD_VALUE);
        let ok = with_raw_doc(
            &Document::from_canonical(
                RawDoc::from_bytes(&nested(MAX_DEPTH).to_bytes().expect("within")).expect("valid"),
            ),
            |_| Ok(()),
        );
        assert!(ok.is_ok());
    }

    #[test]
    fn a_document_past_the_depth_limit_is_a_bad_value() {
        let doc = nested(MAX_DEPTH + 1);
        let e = with_raw_doc(&doc, |_| Ok(())).expect_err("too deep");
        assert_eq!(e.code, BAD_VALUE);
        let e = bytes_of(&doc).expect_err("too deep");
        assert_eq!(e.code, BAD_VALUE);
    }
}
