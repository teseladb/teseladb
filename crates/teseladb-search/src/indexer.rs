//! The segments of a search index, one per range of its collection (011 FR-009, FR-013, FR-015,
//! FR-016): each range's documents are analyzed into their own segment, a split hands them to
//! the segments of the new ranges and a merge joins them. With more than one segment a query runs
//! in two steps: the statistics of every segment are added first and each segment scores with
//! them, so the scores are those of one index with all the documents, as `refsearch` gives them on a
//! replica set; with one segment the first step is left out.

use teseladb_bson::Document;

use crate::analysis::ANALYZERS_VERSION;
use crate::definition::Definition;
use crate::index::{Contents, IndexError, MemIndex, Stats};
use crate::query::search::{self, SearchError};

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
}

/// The segments of one index.
#[derive(Clone, Debug)]
pub struct Indexer {
    definition: Definition,
    segments: Vec<Segment>,
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

    /// Makes the segments follow the ranges `bounds` (each `[start, end)`, in key order; none
    /// means one range over every key): a segment whose range stays is kept, and the documents
    /// of the others go to the segments of the new ranges (FR-013).
    pub fn reshape(&mut self, bounds: &[(Vec<u8>, Vec<u8>)]) -> Result<(), IndexError> {
        let whole = [(Vec::new(), Vec::new())];
        let bounds = if bounds.is_empty() { &whole[..] } else { bounds };
        let mut old: Vec<Segment> = std::mem::take(&mut self.segments);
        let mut moved: Vec<(Vec<u8>, Document)> = Vec::new();
        old.retain(|s| {
            let stays = bounds.iter().any(|(a, b)| *a == s.start && *b == s.end);
            if !stays {
                moved.extend(s.index.entries().map(|(k, d)| (k.to_vec(), d.clone())));
            }
            stays
        });
        for (start, end) in bounds {
            match old.iter().position(|s| s.start == *start && s.end == *end) {
                Some(i) => self.segments.push(old.swap_remove(i)),
                None => self.segments.push(Segment {
                    start: start.clone(),
                    end: end.clone(),
                    index: MemIndex::new(self.definition.clone()),
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

    /// The documents `spec` selects, best first (equal scores in key order).
    pub fn search(&self, spec: &Document) -> Result<Vec<Found>, SearchError> {
        let mut found = Vec::new();
        for s in &self.segments {
            for hit in search::run(&s.index, spec)? {
                if let Some(doc) = s.index.document_of(&hit.key) {
                    found.push(Found { doc: doc.clone(), key: hit.key, score: hit.score });
                }
            }
        }
        found.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.key.cmp(&b.key)));
        Ok(found)
    }

    /// The document of `$searchMeta` for `spec`: the count over every segment.
    pub fn meta(&self, spec: &Document) -> Result<Document, SearchError> {
        let total = search::count_total(spec)?;
        let mut n = 0;
        for s in &self.segments {
            n += search::run(&s.index, spec)?.len();
        }
        Ok(search::count_document(total, n))
    }
}

#[cfg(test)]
mod tests;
