//! Search results (011 FR-009, FR-010, FR-013, SC-002, SC-006): a search answers at or above the
//! mark it asked for, with the documents its query selects in the collection as it was at the mark
//! its segments reached, each with the score one segment over every document gives it. The oracle
//! builds that segment from the committed writes. A search is a `$search` over a dynamic index or
//! an exact `$vectorSearch` over a vector index (`vector` workload).

use std::collections::BTreeMap;

use teseladb_hlc::Hlc;
use teseladb_search::definition::{Definition, VectorDefinition};
use teseladb_search::indexer::{Found, Indexer};
use teseladb_search::query::vector_search::VectorQuery;
use teseladb_search::vector_indexer::VectorIndexer;
use teseladb_bson::Document;
use teseladb_bson::Value;

use crate::checkers::Violation;

/// Name of the checker.
pub const CHECKER: &str = "search_results";

/// A committed write of a document: its key, its mark and the document (`None`: deleted).
#[derive(Clone, Debug, PartialEq)]
pub struct Write {
    /// Key of the document.
    pub key: Vec<u8>,
    /// Commit mark.
    pub ts: Hlc,
    /// The document written.
    pub doc: Option<Document>,
}

/// A search a client ran.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchLog {
    /// The mark the search asked for (the cluster's when it began).
    pub target: Hlc,
    /// The mark its segments had reached when it answered.
    pub applied: Hlc,
    /// The query: `$search`'s document, or `$vectorSearch`'s.
    pub query: Document,
    /// Whether it is a `$vectorSearch`.
    pub vector: bool,
    /// What it answered: each document's key and the bits of its score, best first.
    pub found: Vec<(Vec<u8>, u32)>,
}

/// The index definition every `$search` of the workload uses.
pub fn definition() -> Definition {
    let mappings = Document::new().with("dynamic", true);
    Definition::parse(&Document::new().with("mappings", mappings)).expect("a dynamic index")
}

/// The index definition every `$vectorSearch` of the workload uses: `v`, two dimensions.
pub fn vector_definition() -> VectorDefinition {
    let field = Document::new()
        .with("type", "vector")
        .with("path", "v")
        .with("numDimensions", 2)
        .with("similarity", "euclidean");
    let def = Document::new().with("fields", Value::Array(vec![Value::Document(field)]));
    VectorDefinition::parse(&def).expect("a vector index")
}

/// Segments of either kind of index, as a searcher and the oracle keep them.
#[derive(Clone, Debug)]
pub enum Segments {
    /// A search index.
    Text(Box<Indexer>),
    /// A vector search index.
    Vector(VectorIndexer),
}

impl Segments {
    /// Empty segments of the workload's index, a vector one or not.
    pub fn new(vector: bool) -> Segments {
        match vector {
            true => Segments::Vector(VectorIndexer::new(vector_definition())),
            false => Segments::Text(Box::new(Indexer::new(definition()))),
        }
    }

    /// Makes the segments follow the ranges `bounds`.
    pub fn reshape(&mut self, bounds: &[(Vec<u8>, Vec<u8>)]) -> bool {
        match self {
            Segments::Text(i) => i.reshape(bounds).is_ok(),
            Segments::Vector(i) => {
                i.reshape(bounds);
                true
            }
        }
    }

    /// Applies the newest version of some keys.
    pub fn apply(&mut self, docs: impl IntoIterator<Item = (Vec<u8>, Option<Document>)>) -> bool {
        match self {
            Segments::Text(i) => i.apply(docs).is_ok(),
            Segments::Vector(i) => {
                i.apply(docs);
                true
            }
        }
    }

    /// What `query` answers: each document's key and the bits of its score, best first.
    pub fn search(&self, query: &Document) -> Option<Vec<(Vec<u8>, u32)>> {
        let found: Vec<Found> = match self {
            Segments::Text(i) => i.search(query).ok()?,
            Segments::Vector(i) => i.search(&VectorQuery::parse(query).ok()?).ok()?,
        };
        Some(found.into_iter().map(|f| (f.key, f.score.to_bits())).collect())
    }
}

/// What `query` answers over the documents `writes` leave at mark `at`, in one segment.
pub fn expected(writes: &[Write], at: Hlc, query: &Document, vector: bool) -> Vec<(Vec<u8>, u32)> {
    let mut newest: BTreeMap<&[u8], (Hlc, Option<&Document>)> = BTreeMap::new();
    for w in writes.iter().filter(|w| w.ts <= at) {
        let e = newest.entry(&w.key).or_insert((w.ts, w.doc.as_ref()));
        if w.ts >= e.0 {
            *e = (w.ts, w.doc.as_ref());
        }
    }
    let mut one = Segments::new(vector);
    let docs = newest.into_iter().map(|(k, (_, d))| (k.to_vec(), d.cloned()));
    if !one.apply(docs) {
        return Vec::new();
    }
    one.search(query).unwrap_or_default()
}

/// Checks every search against the committed writes `writes` (with those of transactions whose
/// end is unknown that some search applied, at the mark it read them at).
pub fn check(writes: &[Write], logs: &[SearchLog]) -> Result<(), Violation> {
    for log in logs {
        if log.applied < log.target {
            return Err(Violation::safety(
                CHECKER,
                format!(
                    "a search for {:?} answered at {:?}, below its mark {:?}",
                    log.query, log.applied, log.target
                ),
            ));
        }
        let want = expected(writes, log.applied, &log.query, log.vector);
        if want != log.found {
            return Err(Violation::safety(
                CHECKER,
                format!(
                    "a search for {:?} at {:?} answered {:?} where one segment over the \
                     committed documents answers {:?}",
                    log.query, log.applied, log.found, want
                ),
            ));
        }
    }
    if logs.is_empty() {
        return Err(Violation::liveness(CHECKER, "no search answered"));
    }
    Ok(())
}
