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
use teseladb_search::definition::{Definition, VectorDefinition};
use teseladb_search::index::MemIndex;
use teseladb_search::indexer::{Found, Indexer};
use teseladb_search::query::search::SearchError;
use teseladb_search::query::vector_search::{self, VectorQuery};
use teseladb_search::vector_indexer::VectorIndexer;

use super::query::documents_of;
use super::search_indexes::{IndexView, check_view_stages, split_definition};
use super::{Backend, Shared, lock, views};
use crate::error::{CommandError, INTERNAL_ERROR, MAX_TIME_MS_EXPIRED, UNKNOWN_ERROR};

/// The index a search stage uses when it names none.
pub const DEFAULT_INDEX: &str = "default";

/// The `$search` metadata of a document's score.
const SEARCH_SCORE: &str = "searchScore";
/// The `$vectorSearch` metadata of a document's score.
const VECTOR_SEARCH_SCORE: &str = "vectorSearchScore";

/// The segments of an index, by its type.
#[derive(Clone, Debug)]
pub enum Segments {
    /// A `search` index.
    Search(Box<Indexer>),
    /// A `vectorSearch` index.
    Vector(VectorIndexer),
}

impl Segments {
    fn reshape(&mut self, bounds: &[(Vec<u8>, Vec<u8>)]) -> Result<(), CommandError> {
        match self {
            Segments::Search(ix) => ix.reshape(bounds).map_err(index_error),
            Segments::Vector(ix) => {
                ix.reshape(bounds);
                Ok(())
            }
        }
    }

    fn apply(&mut self, newest: BTreeMap<Vec<u8>, Option<Document>>) -> Result<(), CommandError> {
        match self {
            Segments::Search(ix) => ix.apply(newest).map_err(index_error),
            Segments::Vector(ix) => {
                ix.apply(newest);
                Ok(())
            }
        }
    }
}

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
    indexer: Segments,
    /// The mark every segment has applied the versions up to.
    applied: Hlc,
    /// The view the index was created on: its documents are the collection's through the
    /// view's stages (FR-021).
    view: Option<IndexView>,
    /// With a view, the collection's documents as stored, by key.
    stored: BTreeMap<Vec<u8>, Document>,
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

/// The empty segments of an index of the catalog and its definition completed with its
/// defaults.
fn definition_of(
    entry: &SearchIndexEntry,
) -> Result<(Segments, Document, Option<IndexView>), CommandError> {
    let (raw, view) = split_definition(&entry.definition)?;
    let (segments, completed) = match entry.kind {
        SearchIndexKind::Search => {
            let def = Definition::parse(&raw).map_err(|e| query_error(e.message("")))?;
            let completed = def.normalized();
            (Segments::Search(Box::new(Indexer::new(def))), completed)
        }
        SearchIndexKind::VectorSearch => {
            let def = VectorDefinition::parse(&raw).map_err(|e| query_error(e.message("")))?;
            let completed = def.normalized();
            (Segments::Vector(VectorIndexer::new(def)), completed)
        }
    };
    Ok((segments, completed, view))
}

/// Brings the segments of `entry` on `coll` up to the resolved marks of its ranges, building
/// them first when this node has none for its definition version, and runs `f` over them.
pub fn with_index<B: Backend + ?Sized, R>(
    backend: &mut B,
    shared: &Shared,
    coll: &CollectionEntry,
    entry: &SearchIndexEntry,
    f: impl FnOnce(&Segments, &BTreeMap<Vec<u8>, Document>) -> R,
) -> Result<R, CommandError> {
    let taken = lock(shared).search.built.remove(&entry.id);
    let mut built = match taken {
        Some(b) if b.version == entry.version && b.collection == coll.id => b,
        _ => {
            let (indexer, definition, view) = definition_of(entry)?;
            Built {
                collection: coll.id,
                version: entry.version,
                defined_ms: entry.defined_ms,
                definition,
                indexer,
                applied: Hlc { physical: 0, logical: 0 },
                view,
                stored: BTreeMap::new(),
            }
        }
    };
    catch_up(backend, coll, &mut built)?;
    let out = f(&built.indexer, &built.stored);
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
    // The search waits until every range's segments reach the read's mark, its
    // `search_applied_ts` (FR-010), no longer than the command's `maxTimeMS`.
    let target = backend.read_mark().map_or_else(|| backend.cluster_mark(), |m| m.at);
    let ranges = loop {
        let ranges = backend.range_changes(&start, &end, built.applied)?;
        if ranges.iter().all(|r| r.resolved >= target) {
            break ranges;
        }
        if !backend.pause() {
            return Err(CommandError::new(MAX_TIME_MS_EXPIRED, "operation exceeded time limit"));
        }
    };
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
    if let Some(view) = &built.view {
        // The stages a search view may have read no other collection: no database is needed.
        newest = through_view(backend, "", view, newest, &mut built.stored)?;
    }
    built.indexer.reshape(&bounds)?;
    built.indexer.apply(newest)?;
    built.applied = resolved;
    Ok(())
}

/// The changed documents of an index on a view: each one through the view's stages (`None`
/// when its `$match` leaves it out), the stored ones kept in `stored`.
fn through_view<B: Backend + ?Sized>(
    backend: &mut B,
    db: &str,
    view: &IndexView,
    newest: BTreeMap<Vec<u8>, Option<Document>>,
    stored: &mut BTreeMap<Vec<u8>, Document>,
) -> Result<BTreeMap<Vec<u8>, Option<Document>>, CommandError> {
    let mut out = BTreeMap::new();
    for (key, doc) in newest {
        let seen = match doc {
            Some(d) => {
                stored.insert(key.clone(), d.clone());
                super::aggregate::run_stages(backend, db, &view.stages, vec![d])?.into_iter().next()
            }
            None => {
                stored.remove(&key);
                None
            }
        };
        out.insert(key, seen);
    }
    Ok(out)
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

/// The documents of `$search`, `$searchMeta` or `$vectorSearch` on `ns` with `spec`.
pub fn source<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &Shared,
    ns: &Namespace,
    stage: &str,
    spec: &Document,
) -> Result<Vec<Scored>, CommandError> {
    if stage == "$vectorSearch" {
        return vector_source(backend, shared, ns, spec);
    }
    let name = index_name(spec)?;
    let meta_only = stage == "$searchMeta";
    // Without the collection even `$searchMeta` gives nothing (measured).
    let Some((coll, view)) = target(backend, ns, !meta_only)? else { return Ok(Vec::new()) };
    let entry = backend.search_indexes(coll.id)?.into_iter().find(|e| e.name == name);
    let entry = match entry {
        None => return Ok(missing(meta_only)),
        // `refsearch` reads the query before it finds the index is of the other type.
        Some(e) if e.kind == SearchIndexKind::VectorSearch => {
            let empty = MemIndex::new(dynamic_definition()?);
            teseladb_search::query::search::run(&empty, spec).map_err(|e| query_error(e.0))?;
            return Err(query_error(vector_search::not_search(&name).0));
        }
        Some(e) if !serves(&e, view.as_deref())? => return Ok(missing(meta_only)),
        Some(e) => e,
    };
    let found = with_index(backend, shared, &coll, &entry, |ix, stored| match (ix, meta_only) {
        (Segments::Search(ix), true) => ix.meta(spec).map(|d| vec![plain(d)]),
        (Segments::Search(ix), false) => {
            ix.search(spec).map(|f| scored(as_read(f, stored, view.is_some()), SEARCH_SCORE))
        }
        (Segments::Vector(_), _) => Err(vector_search::not_search(&name)),
    })?;
    found.map_err(|e| query_error(e.0))
}

/// What a search stage finds without an index: nothing, and `$searchMeta` counts no document.
fn missing(meta_only: bool) -> Vec<Scored> {
    match meta_only {
        true => vec![plain(teseladb_search::query::search::count_document(true, 0))],
        false => Vec::new(),
    }
}

/// The collection of `ns` (through its views) and, when `ns` is a view with stages, its name;
/// `check` refuses the stages a search index cannot follow (FR-021). `None` when there is no
/// collection.
fn target<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    check: bool,
) -> Result<Option<(CollectionEntry, Option<String>)>, CommandError> {
    let (coll_ns, view) = match views::resolve(backend, ns)? {
        Some(v) if v.pipeline.is_empty() => {
            (Namespace { db: ns.db.clone(), coll: v.collection }, None)
        }
        Some(v) => {
            if check {
                check_view_stages(&v.pipeline)?;
            }
            (Namespace { db: ns.db.clone(), coll: v.collection }, Some(ns.coll.clone()))
        }
        None => (ns.clone(), None),
    };
    Ok(backend.collection(&coll_ns)?.map(|c| (c, view)))
}

/// Whether index `entry` serves a query on view `view` (`None`: on the collection, which any
/// index serves): measured on 9.0.4, an index created on another view finds nothing.
fn serves(entry: &SearchIndexEntry, view: Option<&str>) -> Result<bool, CommandError> {
    let Some(view) = view else { return Ok(true) };
    let (_, made_on) = split_definition(&entry.definition)?;
    Ok(made_on.is_some_and(|v| v.name == view))
}

/// The documents found as the query reads them: through the index's view when the query is on
/// that view, else as the collection stores them.
fn as_read(found: Vec<Found>, stored: &BTreeMap<Vec<u8>, Document>, on_view: bool) -> Vec<Found> {
    if on_view || stored.is_empty() {
        return found;
    }
    found
        .into_iter()
        .map(|mut f| {
            if let Some(d) = stored.get(&f.key) {
                f.doc = d.clone();
            }
            f
        })
        .collect()
}

/// The definition `{mappings: {dynamic: true}}`, to read a query with no index of its type.
fn dynamic_definition() -> Result<Definition, CommandError> {
    let def = Document::new().with("mappings", Document::new().with("dynamic", true));
    Definition::parse(&def).map_err(|e| query_error(e.message("")))
}

/// The documents of `$vectorSearch` on `ns` with `spec`: `refsearch` reads the query first, then
/// a missing index or collection finds nothing and an index of type `search` refuses it.
fn vector_source<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &Shared,
    ns: &Namespace,
    spec: &Document,
) -> Result<Vec<Scored>, CommandError> {
    let query = VectorQuery::parse(spec).map_err(|e| query_error(e.0))?;
    let Some((coll, view)) = target(backend, ns, true)? else { return Ok(Vec::new()) };
    let entry = backend.search_indexes(coll.id)?.into_iter().find(|e| e.name == query.index);
    let Some(entry) = entry else { return Ok(Vec::new()) };
    if entry.kind == SearchIndexKind::Search {
        return Err(query_error(vector_search::not_vector(&query.path).0));
    }
    if !serves(&entry, view.as_deref())? {
        return Ok(Vec::new());
    }
    let on_view = view.is_some();
    let found = with_index(backend, shared, &coll, &entry, |ix, stored| match ix {
        Segments::Vector(ix) => {
            ix.search(&query).map(|f| scored(as_read(f, stored, on_view), VECTOR_SEARCH_SCORE))
        }
        Segments::Search(_) => Err(vector_search::not_vector(&query.path)),
    })?;
    found.map_err(|e: SearchError| query_error(e.0))
}

/// The documents found, with their score as metadata `name`.
fn scored(found: Vec<Found>, name: &str) -> Vec<Scored> {
    found
        .into_iter()
        .map(|f| {
            let score = Value::Double(f64::from(f.score));
            Scored { doc: f.doc, meta: BTreeMap::from([(name.to_owned(), score)]) }
        })
        .collect()
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
