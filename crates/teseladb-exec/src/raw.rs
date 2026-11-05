//! Documents built during execution read back as raw BSON (for the matcher, the sort keys and
//! the extensions), through one conversion: a document past BSON's limits (more levels or bytes
//! than a stored document may have) is a `BadValue`.

use teseladb_mql::MqlError;
use teseladb_bson::{BsonError, Document, RawDoc};

use crate::expr::codes::BAD_VALUE;

/// The `BadValue` of a document that cannot be written as BSON.
pub(crate) fn bad(e: BsonError) -> MqlError {
    MqlError::new(BAD_VALUE, e.to_string())
}

/// `doc` in BSON.
pub(crate) fn bytes_of(doc: &Document) -> Result<Vec<u8>, MqlError> {
    doc.to_bytes().map_err(bad)
}

/// Runs `f` over `doc` as raw BSON.
pub(crate) fn with_raw_doc<R>(
    doc: &Document,
    f: impl FnOnce(RawDoc<'_>) -> Result<R, MqlError>,
) -> Result<R, MqlError> {
    let bytes = bytes_of(doc)?;
    f(RawDoc::from_bytes(&bytes).map_err(bad)?)
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
    fn a_document_past_the_depth_limit_is_a_bad_value() {
        let doc = nested(MAX_DEPTH + 1);
        let e = with_raw_doc(&doc, |_| Ok(())).expect_err("too deep");
        assert_eq!(e.code, BAD_VALUE);
        let e = bytes_of(&doc).expect_err("too deep");
        assert_eq!(e.code, BAD_VALUE);
    }
}
