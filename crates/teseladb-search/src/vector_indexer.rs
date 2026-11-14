//! The segments of a `vectorSearch` index, one per range of its collection (011 FR-005, FR-013,
//! FR-017): each segment keeps its range's documents and, for each vector field of the
//! definition, an HNSW graph of their vectors (T031). A change links the new versions into the
//! graphs and leaves the old ones as tombstones, which a query walks but never returns; a segment
//! whose tombstones pass a share of its nodes, or that takes documents from another range when
//! ranges split or merge, is built again whole in key order. A query searches every segment's graph (or scores every vector with `exact`), keeps the
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

/// A segment is built again whole once more than one node in this many is a tombstone: each
/// rebuild then follows at least a third as many changes as the documents it links, so building
/// costs a constant number of links per change.
const TOMBSTONE_SHARE: usize = 4;

/// A document of a segment: its value, its bytes for the prefilter and its node in the graphs.
#[derive(Clone, Debug)]
struct Stored {
    doc: Document,
    bytes: Vec<u8>,
    node: u32,
}

/// The graph of one vector field over a segment's nodes.
#[derive(Clone, Debug)]
struct Graph {
    path: String,
    hnsw: Hnsw,
}

/// A vector field of the definition.
#[derive(Clone, Debug)]
struct Field {
    path: String,
    dims: usize,
    similarity: Similarity,
    quantization: Quantization,
}

impl Field {
    /// The vector of `doc` this field indexes, if it has a usable one.
    fn vector(&self, doc: &Document) -> Option<Vec<f32>> {
        let v = value_at(doc, &self.path).and_then(vector_of)?;
        Some(v).filter(|v| v.len() == self.dims && v.iter().all(|x| x.is_finite()))
    }

    fn graph(&self, vectors: Vec<Option<Vec<f32>>>) -> Graph {
        let (dims, sim, q) = (self.dims, self.similarity, self.quantization);
        Graph { path: self.path.clone(), hnsw: Hnsw::build(dims, sim, q, DEFAULT_M, vectors) }
    }
}

/// The segment of one range `[start, end)` (an empty `end` has no upper bound).
#[derive(Clone, Debug)]
struct Segment {
    start: Vec<u8>,
    end: Vec<u8>,
    docs: BTreeMap<Vec<u8>, Stored>,
    /// The key of each node of the graphs, in the order they were linked; `None` for a
    /// tombstone, a version deleted or replaced since.
    nodes: Vec<Option<Vec<u8>>>,
    /// Tombstones among the nodes.
    dead: usize,
    graphs: Vec<Graph>,
    /// Whether the segment took documents it has not linked, and has to be built again.
    stale: bool,
}

impl Segment {
    fn new(start: Vec<u8>, end: Vec<u8>, fields: &[Field]) -> Segment {
        let graphs = fields.iter().map(|f| f.graph(Vec::new())).collect();
        Segment {
            start,
            end,
            docs: BTreeMap::new(),
            nodes: Vec::new(),
            dead: 0,
            graphs,
            stale: false,
        }
    }

    /// Builds the graphs again with one node per document, in key order.
    fn rebuild(&mut self, fields: &[Field]) {
        self.nodes = self.docs.keys().cloned().map(Some).collect();
        self.dead = 0;
        for (node, stored) in self.docs.values_mut().enumerate() {
            stored.node = node as u32;
        }
        self.graphs = fields
            .iter()
            .map(|f| f.graph(self.docs.values().map(|d| f.vector(&d.doc)).collect()))
            .collect();
        self.stale = false;
    }

    /// Makes the node of `key`, if any, a tombstone.
    fn bury(&mut self, key: &[u8]) -> Option<Stored> {
        let old = self.docs.remove(key)?;
        self.nodes[old.node as usize] = None;
        self.dead += 1;
        Some(old)
    }

    /// Links `doc` as the newest version of `key`.
    fn link(&mut self, fields: &[Field], key: Vec<u8>, doc: Document) {
        let node = self.nodes.len() as u32;
        for (f, g) in fields.iter().zip(&mut self.graphs) {
            g.hnsw.push(f.vector(&doc));
        }
        self.nodes.push(Some(key.clone()));
        let bytes = doc.to_bytes().unwrap_or_default();
        self.docs.insert(key, Stored { doc, bytes, node });
    }

    /// Builds the segment again when it took documents or has too many tombstones.
    fn settle(&mut self, fields: &[Field]) {
        if self.stale || self.dead * TOMBSTONE_SHARE > self.nodes.len() {
            self.rebuild(fields);
        }
    }
}

/// The segments of one `vectorSearch` index.
#[derive(Clone, Debug)]
pub struct VectorIndexer {
    definition: VectorDefinition,
    fields: Vec<Field>,
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
        let fields: Vec<Field> = definition
            .fields
            .iter()
            .filter_map(|f| match f {
                VectorField::Vector { path, dimensions, similarity, quantization } => {
                    let (similarity, quantization) = space(similarity, quantization.as_deref());
                    let dims = usize::try_from(*dimensions).unwrap_or(0);
                    Some(Field { path: path.clone(), dims, similarity, quantization })
                }
                _ => None,
            })
            .collect();
        let segments = vec![Segment::new(Vec::new(), Vec::new(), &fields)];
        VectorIndexer { definition, fields, segments }
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
    /// does: a segment whose range stays keeps its graphs, and one that takes documents of
    /// another range is built again.
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
            match old.iter().position(|s| (&s.start, &s.end) == (start, end)) {
                Some(i) => self.segments.push(old.swap_remove(i)),
                None => self.segments.push(Segment::new(start.clone(), end.clone(), &self.fields)),
            }
        }
        for (key, stored) in moved {
            if let Some(s) = self.segment_of(&key) {
                s.docs.insert(key, stored);
                s.stale = true;
            }
        }
        self.settle();
    }

    fn segment_of(&mut self, key: &[u8]) -> Option<&mut Segment> {
        self.segments.iter_mut().find(|s| in_range(key, &s.start, &s.end))
    }

    /// Applies the newest version of some keys: a document replaces the key's, `None` deletes
    /// it. The new versions are linked into the graphs in the order given.
    pub fn apply(&mut self, changes: impl IntoIterator<Item = (Vec<u8>, Option<Document>)>) {
        let fields = std::mem::take(&mut self.fields);
        for (key, doc) in changes {
            let Some(s) = self.segment_of(&key) else { continue };
            s.bury(&key);
            if let Some(doc) = doc {
                s.link(&fields, key, doc);
            }
        }
        self.fields = fields;
        self.settle();
    }

    /// Builds again the segments that took documents or have too many tombstones.
    fn settle(&mut self) {
        for s in &mut self.segments {
            s.settle(&self.fields);
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
            let stored = |node: u32| {
                let key = s.nodes.get(node as usize)?.as_ref()?;
                s.docs.get(key).map(|d| (key, d))
            };
            let keep = |node: u32| match (&filter, stored(node)) {
                (None, Some(_)) => true,
                (Some(f), Some((_, d))) => RawDoc::from_bytes(&d.bytes)
                    .ok()
                    .is_some_and(|raw| f.matches(raw, &ctx).unwrap_or(false)),
                (_, None) => false,
            };
            // Every candidate comes back, so that equal scores go in key order below whatever
            // order the nodes were linked in.
            let hits = match query.exact {
                true => {
                    vector::exact(graph.hnsw.vectors(), &query.vector, similarity, usize::MAX, keep)
                }
                false => {
                    let candidates = query.num_candidates.max(query.limit);
                    graph.hnsw.candidates(&query.vector, candidates, keep)
                }
            };
            let mut hits: Vec<(f32, &Vec<u8>, &Stored)> =
                hits.iter().filter_map(|h| stored(h.doc).map(|(k, d)| (h.score, k, d))).collect();
            hits.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
            hits.truncate(query.limit);
            for (score, key, d) in hits {
                found.push(Found {
                    key: key.clone(),
                    score,
                    doc: d.doc.clone(),
                    token: None,
                    details: None,
                    highlights: None,
                });
            }
        }
        found.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| id_order(&a.doc, &b.doc)));
        found.truncate(query.limit);
        Ok(found)
    }
}

/// The order of two documents of equal score: by `_id`, the order `refsearch` reads a collection
/// in when it builds the index (measured with `tests/compat/scenarios/vector-quantization.json`:
/// `_id` 1 before 8).
fn raw_id(bytes: &[u8]) -> Option<teseladb_bson::RawValue<'_>> {
    RawDoc::from_bytes(bytes).ok().and_then(|d| d.get("_id"))
}

fn id_order(a: &Document, b: &Document) -> std::cmp::Ordering {
    let bytes = |d: &Document| {
        Document::new().with("_id", d.get("_id").cloned().unwrap_or(Value::Null)).to_bytes().ok()
    };
    let (Some(x), Some(y)) = (bytes(a), bytes(b)) else { return std::cmp::Ordering::Equal };
    match (raw_id(&x), raw_id(&y)) {
        (Some(x), Some(y)) => teseladb_bson::compare(x, y, None),
        _ => std::cmp::Ordering::Equal,
    }
}

#[cfg(test)]
mod tests;
