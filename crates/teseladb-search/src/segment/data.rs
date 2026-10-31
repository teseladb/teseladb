//! The content of a segment in memory: what the indexer builds, the builder writes, the reader
//! gives back whole and a merge combines (data-model §2).

use std::collections::BTreeMap;

use teseladb_keys::Hlc;
use teseladb_types::GroupId;

/// One occurrence of a term in a field: token position and byte offsets in the field's text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Occurrence {
    /// Token position.
    pub position: u32,
    /// Byte offset of the token's start.
    pub start: u32,
    /// Byte offset past the token's end.
    pub end: u32,
}

/// The occurrences of a term in one document, by position; the frequency is their number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostingEntry {
    /// Document number within the segment.
    pub doc: u32,
    /// Occurrences, at least one, by increasing position.
    pub occurrences: Vec<Occurrence>,
}

/// A text field: the length of the field in each document (0 without it) and its terms.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FieldData {
    /// Length in tokens per document number.
    pub lengths: Vec<u32>,
    /// Postings of each term, by document.
    pub terms: BTreeMap<Vec<u8>, Vec<PostingEntry>>,
}

/// A document of the segment: its key and the timestamp of the version indexed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocEntry {
    /// Key of the document.
    pub key: Vec<u8>,
    /// Timestamp of the indexed version.
    pub ts: Hlc,
}

/// Everything a segment holds; per-document vectors are indexed by document number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentData {
    /// Range the segment belongs to.
    pub range: GroupId,
    /// Name of the search index.
    pub index: String,
    /// Version of the analyzers that produced the terms (FR-011).
    pub analyzer_version: u32,
    /// Highest log index of the range the segment covers (FR-012).
    pub applied_index: u64,
    /// Documents by number.
    pub docs: Vec<DocEntry>,
    /// Text fields by path.
    pub fields: BTreeMap<String, FieldData>,
    /// Per-document values for `sort`, `range`, `equals`, `in` and facets, by path (BSON bytes).
    pub columns: BTreeMap<String, Vec<Option<Vec<u8>>>>,
    /// Geographic cells of each document, by path, in the cell encoding of 009.
    pub geo: BTreeMap<String, Vec<Vec<u64>>>,
    /// The `storedSource` fields of each document (a BSON document), when the index keeps them.
    pub stored: Vec<Option<Vec<u8>>>,
}

impl SegmentData {
    /// An empty segment of `index` in `range`.
    pub fn new(range: GroupId, index: &str, analyzer_version: u32) -> SegmentData {
        SegmentData {
            range,
            index: index.to_owned(),
            analyzer_version,
            applied_index: 0,
            docs: Vec::new(),
            fields: BTreeMap::new(),
            columns: BTreeMap::new(),
            geo: BTreeMap::new(),
            stored: Vec::new(),
        }
    }

    /// Lowest and highest timestamp of the documents, or `None` without documents.
    pub fn ts_bounds(&self) -> Option<(Hlc, Hlc)> {
        let min = self.docs.iter().map(|d| d.ts).min()?;
        let max = self.docs.iter().map(|d| d.ts).max()?;
        Some((min, max))
    }
}
