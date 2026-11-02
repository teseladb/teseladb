//! The segments of a search index, one per range of its collection (011 FR-009, FR-013, FR-015,
//! FR-016): each range's documents are analyzed into their own segment, a split hands them to
//! the segments of the new ranges and a merge joins them. With more than one segment a query runs
//! in two steps: the statistics of every segment are added first and each segment scores with
//! them, so the scores are those of one index with all the documents, as `refsearch` gives them on a
//! replica set; with one segment the first step is left out.

use teseladb_bson::Document;

use crate::analysis::ANALYZERS_VERSION;
use crate::definition::Definition;
pub use crate::index::DumpError;
use crate::index::{Contents, IndexError, MemIndex, Stats};
use crate::query::search::{self, SearchError};
use teseladb_keys::Hlc;

/// The segment of one range `[start, end)` (an empty `end` has no upper bound).
#[derive(Clone, Debug)]
struct Segment {
    start: Vec<u8>,
    end: Vec<u8>,
    index: MemIndex,
}

/// A document a query found, with its score.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    /// Key of the document.
    pub key: Vec<u8>,
    /// Score, a 32-bit float as `refsearch` gives it.
    pub score: f32,
    /// The document as the index has it.
    pub doc: Document,
    /// Its `searchSequenceToken`, for `searchAfter` and `searchBefore` (none from
    /// `$vectorSearch`).
    pub token: Option<String>,
    /// The explanation of its score, when the query asks for `scoreDetails`.
    pub details: Option<crate::index::Explanation>,
}

/// A segment written as an immutable file (FR-012): its range `[start, end)` and the bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentFile {
    /// First key of the range.
    pub start: Vec<u8>,
    /// Key after the range (empty: no upper bound).
    pub end: Vec<u8>,
    /// The file.
    pub bytes: Vec<u8>,
}

/// The segments of one index.
#[derive(Clone, Debug)]
pub struct Indexer {
    definition: Definition,
    segments: Vec<Segment>,
    /// Segments rebuilt by [`Indexer::reshape`].
    merges: u64,
    /// The documents of each synonym mapping's collection, as last read (T036).
    synonyms: std::collections::BTreeMap<String, Vec<Document>>,
}

/// Whether `key` is in `[start, end)`.
fn in_range(key: &[u8], start: &[u8], end: &[u8]) -> bool {
    start <= key && (end.is_empty() || key < end)
}

impl Indexer {
    /// An index of `definition` with one empty segment over every key.
    pub fn new(definition: Definition) -> Indexer {
        let index = MemIndex::new(definition.clone());
        Indexer {
            definition,
            segments: vec![Segment { start: Vec::new(), end: Vec::new(), index }],
            merges: 0,
            synonyms: std::collections::BTreeMap::new(),
        }
    }

    /// The definition of the index.
    pub fn definition(&self) -> &Definition {
        &self.definition
    }

    /// The version of the analyzers that make the terms of every segment: each replica analyzes
    /// the documents itself, and replicas of one version make the same segments (FR-011).
    pub fn analyzers_version(&self) -> u32 {
        ANALYZERS_VERSION
    }

    /// Each segment's range and contents, in key order.
    pub fn contents(&self) -> Vec<(Vec<u8>, Vec<u8>, Contents)> {
        self.segments.iter().map(|s| (s.start.clone(), s.end.clone(), s.index.contents())).collect()
    }

    /// Number of segments.
    pub fn segments(&self) -> usize {
        self.segments.len()
    }

    /// Number of documents indexed.
    pub fn len(&self) -> usize {
        self.segments.iter().map(|s| s.index.len()).sum()
    }

    /// Whether no document is indexed.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The segments rebuilt since the index was made: those whose range split or merged with
    /// another, whose documents went to the segments of the new ranges (011 FR-026).
    pub fn merges(&self) -> u64 {
        self.merges
    }

    /// An estimate of the memory the segments take: their documents, terms and positions.
    pub fn memory_bytes(&self) -> u64 {
        self.segments.iter().map(|s| s.index.memory_bytes()).sum()
    }

    /// Makes the segments follow the ranges `bounds` (each `[start, end)`, in key order; none
    /// means one range over every key): a segment whose range stays is kept, and the documents
    /// of the others go to the segments of the new ranges (FR-013).
    pub fn reshape(&mut self, bounds: &[(Vec<u8>, Vec<u8>)]) -> Result<(), IndexError> {
        let whole = [(Vec::new(), Vec::new())];
        let bounds = if bounds.is_empty() { &whole[..] } else { bounds };
        let mut old: Vec<Segment> = std::mem::take(&mut self.segments);
        let mut moved: Vec<(Vec<u8>, Document)> = Vec::new();
        let before = old.len();
        old.retain(|s| {
            let stays = bounds.iter().any(|(a, b)| *a == s.start && *b == s.end);
            if !stays {
                moved.extend(s.index.entries().map(|(k, d)| (k.to_vec(), d.clone())));
            }
            stays
        });
        self.merges += (before - old.len()) as u64;
        for (start, end) in bounds {
            match old.iter().position(|s| s.start == *start && s.end == *end) {
                Some(i) => self.segments.push(old.swap_remove(i)),
                None => self.segments.push(Segment {
                    start: start.clone(),
                    end: end.clone(),
                    index: self.empty_segment()?,
                }),
            }
        }
        for (key, doc) in moved {
            if let Some(s) = self.segment_of(&key) {
                s.index.put(&key, &doc)?;
            }
        }
        self.refresh_stats();
        Ok(())
    }

    /// An empty segment with the synonyms read so far.
    fn empty_segment(&self) -> Result<MemIndex, IndexError> {
        let mut index = MemIndex::new(self.definition.clone());
        for (name, docs) in &self.synonyms {
            index.set_synonyms(name, docs)?;
        }
        Ok(index)
    }

    /// Gives every segment the documents `docs` of the collection of synonym mapping `name`,
    /// when they changed since the last read (the reload of 011 FR-006).
    pub fn set_synonyms(&mut self, name: &str, docs: &[Document]) -> Result<(), IndexError> {
        if self.synonyms.get(name).is_some_and(|d| d.as_slice() == docs) {
            return Ok(());
        }
        for s in &mut self.segments {
            s.index.set_synonyms(name, docs)?;
        }
        self.synonyms.insert(name.to_owned(), docs.to_vec());
        Ok(())
    }

    /// Every segment as an immutable file covering the versions up to `mark`, in key order: what
    /// a node keeps and what a new replica receives with the SST files of the ranges (FR-012,
    /// FR-013).
    pub fn dump(&self, mark: Hlc) -> Vec<SegmentFile> {
        let file = |s: &Segment| SegmentFile {
            start: s.start.clone(),
            end: s.end.clone(),
            bytes: s.index.dump(mark),
        };
        self.segments.iter().map(file).collect()
    }

    /// The index of `definition` that the files of [`Indexer::dump`] hold, without analyzing the
    /// documents again; no file gives one empty segment over every key.
    pub fn load(definition: Definition, files: &[SegmentFile]) -> Result<Indexer, DumpError> {
        if files.is_empty() {
            return Ok(Indexer::new(definition));
        }
        let chained = files.windows(2).all(|w| w[0].end == w[1].start && !w[0].end.is_empty());
        if !chained {
            return Err(DumpError::Ranges);
        }
        let mut segments = Vec::with_capacity(files.len());
        for f in files {
            let index = MemIndex::load(definition.clone(), &f.bytes)?;
            segments.push(Segment { start: f.start.clone(), end: f.end.clone(), index });
        }
        let synonyms = std::collections::BTreeMap::new();
        let mut ix = Indexer { definition, segments, merges: 0, synonyms };
        ix.refresh_stats();
        Ok(ix)
    }

    /// The mark the segment file `bytes` covers.
    pub fn applied_of(bytes: &[u8]) -> Result<Hlc, DumpError> {
        crate::index::applied_of(bytes)
    }

    /// The segment whose range holds `key`.
    fn segment_of(&mut self, key: &[u8]) -> Option<&mut Segment> {
        self.segments.iter_mut().find(|s| in_range(key, &s.start, &s.end))
    }

    /// Applies the newest version of some keys: a document replaces the key's, `None` deletes
    /// it.
    pub fn apply(
        &mut self,
        changes: impl IntoIterator<Item = (Vec<u8>, Option<Document>)>,
    ) -> Result<(), IndexError> {
        let mut outcome = Ok(());
        for (key, doc) in changes {
            let Some(s) = self.segment_of(&key) else { continue };
            match doc {
                Some(d) => {
                    if let Err(e) = s.index.put(&key, &d) {
                        s.index.delete(&key);
                        outcome = Err(e);
                    }
                }
                None => s.index.delete(&key),
            }
        }
        self.refresh_stats();
        outcome
    }

    /// Gives every segment the statistics of all of them when there are several.
    fn refresh_stats(&mut self) {
        let global = (self.segments.len() > 1).then(|| {
            self.segments.iter().fold(Stats::default(), |acc, s| acc.plus(&s.index.stats()))
        });
        for s in &mut self.segments {
            s.index.set_global(global.clone());
        }
    }

    /// The documents `spec` selects in the order of its `sort` (best first without one; equal
    /// documents in key order), after or before its `searchAfter` or `searchBefore` token.
    pub fn search(&self, spec: &Document) -> Result<Vec<Found>, SearchError> {
        let mut found = Vec::new();
        let details = search::wants_details(spec);
        for s in &self.segments {
            let hits = search::run(&s.index, spec)?;
            let mut explained = if details {
                let keys: Vec<Vec<u8>> = hits.iter().map(|h| h.key.clone()).collect();
                search::explain(&s.index, spec, &keys)?.into_iter().collect()
            } else {
                std::collections::BTreeMap::new()
            };
            for hit in hits {
                if let Some(doc) = s.index.document_of(&hit.key) {
                    let doc = doc.clone();
                    let details = explained.remove(&hit.key);
                    found.push(Found { doc, key: hit.key, score: hit.score, token: None, details });
                }
            }
        }
        crate::query::paginate::arrange(found, spec, &self.definition)
    }

    /// The document of `$searchMeta` for `spec`: the count and the facets over every segment,
    /// each segment counting its documents and the counts added up (FR-015).
    pub fn meta(&self, spec: &Document) -> Result<Document, SearchError> {
        let mut partial = search::facet::Partial::default();
        for s in &self.segments {
            partial = partial.plus(&search::meta_partial(&s.index, spec)?);
        }
        let first = &self.segments[0].index;
        search::meta_finish(first, spec, &partial)
    }
}

#[cfg(test)]
mod tests;
