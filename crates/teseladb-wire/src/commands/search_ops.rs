//! The search indexes this node serves (011 FR-009, FR-010, FR-013, FR-014, FR-020, FR-022):
//! each index of the catalog has its segments, one per range of its collection, filled from a
//! time scan of the range's versions (`scan_time`, through [`Backend::range_changes`]) and kept up
//! to date with the same flow of changes the change streams read (014, D-114). A query first
//! brings the segments up to the ranges' resolved marks, so it sees every write acknowledged
//! before it, and then runs over them with the statistics of the whole index
//! ([`teseladb_search::indexer`]).
//!
//! Measured on a 9.0.4 replica set with `refsearch` 1.70.6 (01-11-2026): an index not yet built is
//! `PENDING` with no `statusDetail`; a built one is `READY`, with one `statusDetail` per host
//! whose `mainIndex` carries the definition version it serves and that definition completed with
//! its defaults; a query on an index or a collection that does not exist finds nothing, and
//! `$searchMeta` then counts `{total: 0}`; a query error is `refsearch`'s message with code 8.

use std::collections::BTreeMap;

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_changestream::changes::{Versioned, changes};
use teseladb_exec::pipeline::search::Scored;
use teseladb_hlc::Hlc;
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_meta::indexes::{SearchIndexEntry, SearchIndexKind};
use teseladb_search::definition::Definition;
use teseladb_search::indexer::Indexer;

use super::query::documents_of;
use super::{Backend, Shared, lock};
use crate::error::{CommandError, INTERNAL_ERROR, UNKNOWN_ERROR};

/// The index a search stage uses when it names none.
pub const DEFAULT_INDEX: &str = "default";

/// The `$search` metadata of a document's score.
const SEARCH_SCORE: &str = "searchScore";

/// The segments of one index as this node built them.
#[derive(Clone, Debug)]
pub struct Built {
    /// The collection.
    collection: u32,
    /// The definition version they serve.
    version: u32,
    /// When that definition was given, in milliseconds since the epoch.
    defined_ms: i64,
    /// That definition completed with its defaults.
    definition: Document,
    /// The segments.
    indexer: Indexer,
    /// The mark every segment has applied the versions up to.
    applied: Hlc,
}

/// The indexes this node built, by identifier.
#[derive(Clone, Debug, Default)]
pub struct SearchIndexes {
    built: BTreeMap<[u8; 12], Built>,
}

/// What `statusDetail` says of an index this node built: the version it serves, when it was
/// given and the definition completed.
#[derive(Clone, Debug, PartialEq)]
pub struct Served {
    /// Definition version.
    pub version: u32,
    /// When it was given, in milliseconds since the epoch.
    pub defined_ms: i64,
    /// The definition completed with its defaults.
    pub definition: Document,
}

impl SearchIndexes {
    /// What this node serves of index `id`.
    pub fn served(&self, id: &[u8; 12]) -> Option<Served> {
        self.built.get(id).map(|b| Served {
            version: b.version,
            defined_ms: b.defined_ms,
            definition: b.definition.clone(),
        })
    }

    /// Forgets index `id` (dropped).
    pub fn forget(&mut self, id: &[u8; 12]) {
        self.built.remove(id);
    }
}

/// A query error of `refsearch`: its message with code 8.
fn query_error(msg: String) -> CommandError {
    CommandError::new(UNKNOWN_ERROR, msg)
}

/// The definition of a `search` index of the catalog.
fn definition_of(entry: &SearchIndexEntry) -> Result<(Definition, Document), CommandError> {
    let raw = Document::from(RawDoc::from_bytes(&entry.definition)?);
    let def = Definition::parse(&raw).map_err(|e| query_error(e.message("")))?;
    let completed = def.normalized();
    Ok((def, completed))
}

/// Brings the segments of `entry` on `coll` up to the resolved marks of its ranges, building
/// them first when this node has none for its definition version, and runs `f` over them.
pub fn with_index<B: Backend + ?Sized, R>(
    backend: &mut B,
    shared: &Shared,
    coll: &CollectionEntry,
    entry: &SearchIndexEntry,
    f: impl FnOnce(&Indexer) -> R,
) -> Result<R, CommandError> {
    let taken = lock(shared).search.built.remove(&entry.id);
    let mut built = match taken {
        Some(b) if b.version == entry.version && b.collection == coll.id => b,
        _ => {
            let (def, definition) = definition_of(entry)?;
            Built {
                collection: coll.id,
                version: entry.version,
                defined_ms: entry.defined_ms,
                definition,
                indexer: Indexer::new(def),
                applied: Hlc { physical: 0, logical: 0 },
            }
        }
    };
    catch_up(backend, coll, &mut built)?;
    let out = f(&built.indexer);
    lock(shared).search.built.insert(entry.id, built);
    Ok(out)
}

/// Applies to `built` what the ranges of its collection wrote after its mark.
fn catch_up<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    built: &mut Built,
) -> Result<(), CommandError> {
    let (start, end) = documents_of(coll);
    let ranges = backend.range_changes(&start, &end, built.applied)?;
    let bounds: Vec<(Vec<u8>, Vec<u8>)> =
        ranges.iter().map(|r| (r.start.clone(), r.end.clone())).collect();
    let resolved = ranges.iter().map(|r| r.resolved).min().unwrap_or(built.applied);
    let resolved = resolved.max(built.applied);
    let versions = ranges
        .into_iter()
        .flat_map(|r| r.rows)
        .filter(|r| r.ts <= resolved)
        .map(|r| Versioned { key: r.key, ts: r.ts, value: r.value });
    let found = changes(versions).map_err(|e| {
        CommandError::new(INTERNAL_ERROR, format!("a search index read a bad meta: {e:?}"))
    })?;
    // In the merge order, so the newest version of each key stays.
    let mut newest: BTreeMap<Vec<u8>, Option<Document>> = BTreeMap::new();
    for c in found {
        let doc = match c.doc {
            Some(bytes) => Some(Document::from(RawDoc::from_bytes(&bytes)?)),
            None => None,
        };
        newest.insert(c.event.key, doc);
    }
    built.indexer.reshape(&bounds).map_err(index_error)?;
    built.indexer.apply(newest).map_err(index_error)?;
    built.applied = resolved;
    Ok(())
}

/// A document the index cannot analyze.
fn index_error(e: teseladb_search::index::IndexError) -> CommandError {
    query_error(teseladb_search::query::search::SearchError::from(e).0)
}

/// The name of the index a search stage names: `index` or [`DEFAULT_INDEX`].
fn index_name(spec: &Document) -> Result<String, CommandError> {
    match spec.get("index") {
        None => Ok(DEFAULT_INDEX.to_owned()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(query_error("\"index\" must be a string".to_owned())),
    }
}

/// The documents of `$search` or `$searchMeta` on `ns` with `spec`.
pub fn source<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &Shared,
    ns: &Namespace,
    stage: &str,
    spec: &Document,
) -> Result<Vec<Scored>, CommandError> {
    if stage == "$vectorSearch" {
        // The vector indexes are not served by this node's indexer: the stage answers as a
        // deployment without search, as before the indexer.
        return Err(super::mutation::from_mql(teseladb_exec::pipeline::search::not_enabled()));
    }
    let name = index_name(spec)?;
    let Some(coll) = backend.collection(ns)? else { return Ok(Vec::new()) };
    let entry = backend
        .search_indexes(coll.id)?
        .into_iter()
        .find(|e| e.name == name && e.kind == SearchIndexKind::Search);
    let meta_only = stage == "$searchMeta";
    let Some(entry) = entry else {
        return Ok(match meta_only {
            true => vec![plain(teseladb_search::query::search::count_document(true, 0))],
            false => Vec::new(),
        });
    };
    let found = with_index(backend, shared, &coll, &entry, |ix| match meta_only {
        true => ix.meta(spec).map(|d| vec![plain(d)]),
        false => ix.search(spec).map(|found| {
            found
                .into_iter()
                .map(|f| {
                    let score = Value::Double(f64::from(f.score));
                    Scored { doc: f.doc, meta: BTreeMap::from([(SEARCH_SCORE.to_owned(), score)]) }
                })
                .collect()
        }),
    })?;
    found.map_err(|e| query_error(e.0))
}

/// A document without metadata.
fn plain(doc: Document) -> Scored {
    Scored { doc, meta: BTreeMap::new() }
}

/// The `statusDetail` of an index that this node serves: its host and the version it serves.
pub fn status_detail(host: u64, served: &Served) -> Document {
    // `createdAt` keeps whole seconds (measured).
    let created = served.defined_ms - served.defined_ms.rem_euclid(1000);
    let version = Document::new()
        .with("version", i32::try_from(served.version).unwrap_or(i32::MAX))
        .with("createdAt", Value::DateTime(created));
    let main = Document::new()
        .with("status", "READY")
        .with("queryable", true)
        .with("definitionVersion", version)
        .with("definition", served.definition.clone());
    Document::new()
        .with("hostname", format!("{host:024x}"))
        .with("status", "READY")
        .with("queryable", true)
        .with("mainIndex", main)
}
