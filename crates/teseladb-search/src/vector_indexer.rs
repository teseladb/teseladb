//! The segments of a `vectorSearch` index, one per range of its collection (011 FR-005, FR-013,
//! FR-017): each segment keeps its range's documents and, for each vector field of the
//! definition, an HNSW graph of their vectors (T031), built again when the segment changes. A
//! query searches every segment's graph (or scores every vector with `exact`), keeps the
//! documents the prefilter accepts and gives the `limit` best of all segments, with the scores
//! `refsearch` gives (`crate::vector`).
//!
//! A document's vector is an array of numbers or a BSON vector (`BinData` subtype 9) of
//! `float32` or `int8`, with as many components as the field's `numDimensions`; any other value
//! is not indexed, as `refsearch` leaves it out (measured 01-11-2026: a vector of 2 components in a
//! field of 3 and a string are not found).

use std::collections::BTreeMap;

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_mql::{Filter, FilterUse, MatchContext, NoExtensions};

use crate::definition::{VectorDefinition, VectorField};
use crate::indexer::Found;
use crate::query::search::{SearchError, value_at};
use crate::query::vector_search::VectorQuery;
use crate::segment::hnsw::{DEFAULT_M, Hnsw};
use crate::vector::{self, Quantization, Similarity};

/// The BSON binary subtype of a vector.
const VECTOR_SUBTYPE: u8 = 9;
/// The `dtype` byte of a vector of `float32`.
const FLOAT32: u8 = 0x27;
/// The `dtype` byte of a vector of `int8`.
const INT8: u8 = 0x03;

/// A document of a segment: its value and its bytes for the prefilter.
#[derive(Clone, Debug)]
struct Stored {
    doc: Document,
    bytes: Vec<u8>,
}

/// The graph of one vector field over a segment's documents (by document number, the
/// position of the key in key order).
#[derive(Clone, Debug)]
struct Graph {
    path: String,
    hnsw: Hnsw,
}

/// The segment of one range `[start, end)` (an empty `end` has no upper bound).
#[derive(Clone, Debug)]
struct Segment {
    start: Vec<u8>,
    end: Vec<u8>,
    docs: BTreeMap<Vec<u8>, Stored>,
    graphs: Vec<Graph>,
    dirty: bool,
}

impl Segment {
    fn new(start: Vec<u8>, end: Vec<u8>) -> Segment {
        Segment { start, end, docs: BTreeMap::new(), graphs: Vec::new(), dirty: true }
    }
}

/// The segments of one `vectorSearch` index.
#[derive(Clone, Debug)]
pub struct VectorIndexer {
    definition: VectorDefinition,
    segments: Vec<Segment>,
}

/// Whether `key` is in `[start, end)`.
fn in_range(key: &[u8], start: &[u8], end: &[u8]) -> bool {
    start <= key && (end.is_empty() || key < end)
}

/// The vector of a value, if it is one.
fn vector_of(v: &Value) -> Option<Vec<f32>> {
    match v {
        Value::Array(items) => items
            .iter()
            .map(|x| match x {
                Value::Int32(n) => Some(*n as f32),
                Value::Int64(n) => Some(*n as f32),
                Value::Double(d) => Some(*d as f32),
                _ => None,
            })
            .collect(),
        Value::Binary { subtype: VECTOR_SUBTYPE, bytes } => match bytes.as_slice() {
            [FLOAT32, 0, rest @ ..] if rest.len() % 4 == 0 => {
                Some(rest.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect())
            }
            [INT8, 0, rest @ ..] => Some(rest.iter().map(|b| f32::from(*b as i8)).collect()),
            _ => None,
        },
        _ => None,
    }
}

/// The parameters of a vector field of the definition.
fn space(similarity: &str, quantization: Option<&str>) -> (Similarity, Quantization) {
    (
        Similarity::by_name(similarity).unwrap_or(Similarity::Euclidean),
        quantization.and_then(Quantization::by_name).unwrap_or(Quantization::None),
    )
}

impl VectorIndexer {
    /// An index of `definition` with one empty segment over every key.
    pub fn new(definition: VectorDefinition) -> VectorIndexer {
        VectorIndexer { definition, segments: vec![Segment::new(Vec::new(), Vec::new())] }
    }

    /// The definition.
    pub fn definition(&self) -> &VectorDefinition {
        &self.definition
    }

    /// Number of segments.
    pub fn segments(&self) -> usize {
        self.segments.len()
    }

    /// Number of documents kept.
    pub fn len(&self) -> usize {
        self.segments.iter().map(|s| s.docs.len()).sum()
    }

    /// Whether no document is kept.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Makes the segments follow the ranges `bounds` as [`crate::indexer::Indexer::reshape`]
    /// does.
    pub fn reshape(&mut self, bounds: &[(Vec<u8>, Vec<u8>)]) {
        let whole = [(Vec::new(), Vec::new())];
        let bounds = if bounds.is_empty() { &whole[..] } else { bounds };
        let mut old = std::mem::take(&mut self.segments);
        let mut moved = Vec::new();
        old.retain_mut(|s| {
            let stays = bounds.iter().any(|(a, b)| *a == s.start && *b == s.end);
            if !stays {
                moved.extend(std::mem::take(&mut s.docs));
            }
            stays
        });
        for (start, end) in bounds {
            match old.iter().position(|s| s.start == *start && s.end == *end) {
                Some(i) => self.segments.push(old.swap_remove(i)),
                None => self.segments.push(Segment::new(start.clone(), end.clone())),
            }
        }
        for (key, stored) in moved {
            if let Some(s) = self.segment_of(&key) {
                s.docs.insert(key, stored);
                s.dirty = true;
            }
        }
        self.rebuild();
    }

    fn segment_of(&mut self, key: &[u8]) -> Option<&mut Segment> {
        self.segments.iter_mut().find(|s| in_range(key, &s.start, &s.end))
    }

    /// Applies the newest version of some keys: a document replaces the key's, `None` deletes
    /// it.
    pub fn apply(&mut self, changes: impl IntoIterator<Item = (Vec<u8>, Option<Document>)>) {
        for (key, doc) in changes {
            let Some(s) = self.segment_of(&key) else { continue };
            s.dirty = true;
            match doc {
                Some(doc) => {
                    let bytes = doc.to_bytes().unwrap_or_default();
                    s.docs.insert(key, Stored { doc, bytes });
                }
                None => {
                    s.docs.remove(&key);
                }
            }
        }
        self.rebuild();
    }

    /// Builds again the graphs of the segments that changed.
    fn rebuild(&mut self) {
        let fields: Vec<(String, usize, Similarity, Quantization)> = self
            .definition
            .fields
            .iter()
            .filter_map(|f| match f {
                VectorField::Vector { path, dimensions, similarity, quantization } => {
                    let (sim, q) = space(similarity, quantization.as_deref());
                    Some((path.clone(), usize::try_from(*dimensions).unwrap_or(0), sim, q))
                }
                _ => None,
            })
            .collect();
        for s in self.segments.iter_mut().filter(|s| s.dirty) {
            s.graphs = fields
                .iter()
                .map(|(path, dims, sim, q)| {
                    let vectors = s
                        .docs
                        .values()
                        .map(|d| value_at(&d.doc, path).and_then(vector_of))
                        .map(|v| v.filter(|v| v.len() == *dims && v.iter().all(|x| x.is_finite())))
                        .collect();
                    Graph {
                        path: path.clone(),
                        hnsw: Hnsw::build(*dims, *sim, *q, DEFAULT_M, vectors),
                    }
                })
                .collect();
            s.dirty = false;
        }
    }

    /// The documents `query` finds, best first (equal scores in key order), at most its
    /// `limit`.
    pub fn search(&self, query: &VectorQuery) -> Result<Vec<Found>, SearchError> {
        let (_, similarity, quantization) = query.check(&self.definition)?;
        let (similarity, _) = space(&similarity, quantization.as_deref());
        let filter = match &query.filter {
            None => None,
            Some(f) => {
                let bytes = f.to_bytes().map_err(|e| SearchError(format!("{e:?}")))?;
                let raw = RawDoc::from_bytes(&bytes).map_err(|e| SearchError(format!("{e:?}")))?;
                Some(Filter::parse(raw, FilterUse::Query).map_err(|e| SearchError(e.message))?)
            }
        };
        let ctx = MatchContext { collation: None, ext: &NoExtensions, vars: None };
        let mut found = Vec::new();
        for s in &self.segments {
            let Some(graph) = s.graphs.iter().find(|g| g.path == query.path) else { continue };
            let stored: Vec<(&Vec<u8>, &Stored)> = s.docs.iter().collect();
            let keep = |doc: u32| match (&filter, stored.get(doc as usize)) {
                (None, Some(_)) => true,
                (Some(f), Some((_, d))) => RawDoc::from_bytes(&d.bytes)
                    .ok()
                    .is_some_and(|raw| f.matches(raw, &ctx).unwrap_or(false)),
                (_, None) => false,
            };
            let hits = match query.exact {
                true => vector::exact(
                    graph.hnsw.vectors(),
                    &query.vector,
                    similarity,
                    query.limit,
                    keep,
                ),
                false => graph.hnsw.search(&query.vector, query.num_candidates, query.limit, keep),
            };
            for h in hits {
                if let Some((key, d)) = stored.get(h.doc as usize) {
                    found.push(Found {
                        key: (*key).clone(),
                        score: h.score,
                        doc: d.doc.clone(),
                        token: None,
                        details: None,
                        highlights: None,
                    });
                }
            }
        }
        found.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.key.cmp(&b.key)));
        found.truncate(query.limit);
        Ok(found)
    }
}

#[cfg(test)]
mod tests;
