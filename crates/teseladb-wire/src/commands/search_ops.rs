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
use std::sync::atomic::{AtomicU64, Ordering};

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_changestream::changes::{Versioned, changes};
use teseladb_exec::pipeline::search::Scored;
use teseladb_hlc::Hlc;
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_meta::indexes::{SearchIndexEntry, SearchIndexKind};
use teseladb_range::read::ReadConcern;
use teseladb_search::definition::{Definition, VectorDefinition};
use teseladb_search::index::MemIndex;
use teseladb_search::indexer::{Found, Indexer, SegmentFile};
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
/// The metadata of a `$search` result that `searchAfter` and `searchBefore` take.
const SEQUENCE_TOKEN: &str = "searchSequenceToken";
/// The metadata of a `$search` result with the explanation of its score.
const SCORE_DETAILS: &str = "searchScoreDetails";
/// The `$meta` of a document's highlights.
const HIGHLIGHTS: &str = "searchHighlights";

/// Search queries this node answered (011 FR-026).
static QUERIES: AtomicU64 = AtomicU64::new(0);
/// Microseconds those queries took, from asking the index to its answer.
static QUERY_MICROS: AtomicU64 = AtomicU64::new(0);
/// How far behind the mark of the read the last index brought up to date was, in ms.
static INDEXER_LAG_MS: AtomicU64 = AtomicU64::new(0);

/// The search metrics of the node (011 FR-026).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchMetrics {
    /// Queries answered.
    pub queries: u64,
    /// Microseconds they took.
    pub query_micros: u64,
    /// How far behind the read's mark the last index brought up to date was, in ms.
    pub indexer_lag_ms: u64,
    /// Segments of the `search` indexes this node serves.
    pub segments: u64,
    /// Segments those indexes rebuilt when their ranges split or merged.
    pub merges: u64,
    /// An estimate of the bytes their segments take.
    pub memory_bytes: u64,
}

/// The search metrics of the node whose indexes `shared` holds.
pub fn search_metrics(shared: &Shared) -> SearchMetrics {
    let indexes = lock(shared);
    let mut m = SearchMetrics {
        queries: QUERIES.load(Ordering::Relaxed),
        query_micros: QUERY_MICROS.load(Ordering::Relaxed),
        indexer_lag_ms: INDEXER_LAG_MS.load(Ordering::Relaxed),
        ..SearchMetrics::default()
    };
    for built in indexes.search.built.values() {
        if let Segments::Search(ix) = &built.indexer {
            m.segments += ix.segments() as u64;
            m.merges += ix.merges();
            m.memory_bytes += ix.memory_bytes();
        }
    }
    m
}

/// The segments of an index, by its type.
#[derive(Clone, Debug)]
pub enum Segments {
    /// A `search` index.
    Search(Box<Indexer>),
    /// A `vectorSearch` index.
    Vector(VectorIndexer),
}

impl Segments {
    /// Empty segments of the same definition.
    fn emptied(&self) -> Segments {
        match self {
            Segments::Search(ix) => {
                Segments::Search(Box::new(Indexer::new(ix.definition().clone())))
            }
            Segments::Vector(ix) => Segments::Vector(VectorIndexer::new(ix.definition().clone())),
        }
    }

    fn reshape(&mut self, bounds: &[(Vec<u8>, Vec<u8>)]) -> Result<(), CommandError> {
        match self {
            Segments::Search(ix) => ix.reshape(bounds).map_err(index_error),
            Segments::Vector(ix) => {
                ix.reshape(bounds);
                Ok(())
            }
        }
    }

    /// Gives a `search` index the documents of its synonym collections.
    fn set_synonyms(&mut self, synonyms: &[(String, Vec<Document>)]) -> Result<(), CommandError> {
        if let Segments::Search(ix) = self {
            for (name, docs) in synonyms {
                ix.set_synonyms(name, docs).map_err(index_error)?;
            }
        }
        Ok(())
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
    /// Documents changed since the segments were last written to a file.
    pending: usize,
}

/// Changed documents after which the segments of an index are written again to their file.
const DUMP_EVERY: usize = 1024;

/// Where a node keeps the segments of its search indexes, one immutable file per index (FR-012):
/// the server writes them under `<dbpath>/search`; the logic only hands over the bytes, as
/// [`Shipped::encode`] makes them.
pub trait SegmentStore: Send + Sync + std::fmt::Debug {
    /// Replaces the file of index `id` with `bytes`, whole or not at all.
    fn put(&self, id: &[u8; 12], bytes: &[u8]) -> Result<(), String>;
    /// Removes the file of index `id`; one that is not there is already removed.
    fn remove(&self, id: &[u8; 12]) -> Result<(), String>;
}

/// The indexes this node built, by identifier.
#[derive(Clone, Debug, Default)]
pub struct SearchIndexes {
    built: BTreeMap<[u8; 12], Built>,
    /// The indexes whose build failed on this node, with the version that failed.
    failed: BTreeMap<[u8; 12], Served>,
    /// Segment files received with the SST files of the ranges or read from the node's
    /// directory, not yet served (FR-012, FR-013).
    received: BTreeMap<[u8; 12], Shipped>,
    /// Where the segments of each index are written as an immutable file (FR-012).
    store: Option<std::sync::Arc<dyn SegmentStore>>,
    /// Whether a builder of the node fills new indexes outside the commands (FR-014, T040):
    /// a query then serves what is built and an index not built yet refuses it.
    background: bool,
    /// The definition each index the builder is filling for the first time will serve.
    building: BTreeMap<[u8; 12], Served>,
    /// The indexes whose segments a query or the builder holds now.
    borrowed: std::collections::BTreeSet<[u8; 12]>,
}

/// The segment files of a `search` index as one node sends them to a new replica with the SST
/// files of the ranges (FR-013): the replica serves them and catches up from the mark they
/// cover instead of analyzing every document again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shipped {
    /// The collection.
    pub collection: u32,
    /// The definition version they serve.
    pub version: u32,
    /// The segments, in key order.
    pub files: Vec<SegmentFile>,
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
    /// Why building it failed: the index is then `FAILED` and cannot be queried.
    pub failure: Option<String>,
}

impl SearchIndexes {
    /// What this node serves of index `id`, or the version it failed to build.
    pub fn served(&self, id: &[u8; 12]) -> Option<Served> {
        let built = self.built.get(id).map(|b| Served {
            version: b.version,
            defined_ms: b.defined_ms,
            definition: b.definition.clone(),
            failure: None,
        });
        built.or_else(|| self.failed.get(id).cloned())
    }

    /// The definition this node is building index `id` with when it serves none yet.
    pub fn building(&self, id: &[u8; 12]) -> Option<Served> {
        self.building.get(id).cloned()
    }

    /// Hands the building of new indexes to a builder outside the commands (FR-014, T040).
    pub fn set_background(&mut self) {
        self.background = true;
    }

    /// Whether a builder fills new indexes outside the commands.
    pub fn has_builder(&self) -> bool {
        self.background
    }

    /// Forgets index `id` (dropped), and its file.
    pub fn forget(&mut self, id: &[u8; 12]) -> Result<(), String> {
        self.building.remove(id);
        self.built.remove(id);
        self.failed.remove(id);
        self.received.remove(id);
        self.store.as_ref().map_or(Ok(()), |store| store.remove(id))
    }

    /// Keeps the segments of each index in `store` from now on (FR-012).
    pub fn set_store(&mut self, store: std::sync::Arc<dyn SegmentStore>) {
        self.store = Some(store);
    }

    /// Takes a segment file the store kept before a restart, or one that came with a range's
    /// SST files, to serve its segments from the mark they cover instead of analyzing every
    /// document again (FR-012, FR-013): `false` for a damaged file, which the caller removes so
    /// that the index is built again. A file of a version this node already serves or passed is
    /// left: what the node built is at least as new.
    pub fn restore(&mut self, bytes: &[u8]) -> bool {
        let Ok((id, shipped)) = Shipped::decode(bytes) else { return false };
        if self.built.get(&id).is_none_or(|b| b.version < shipped.version) {
            self.received.insert(id, shipped);
        }
        true
    }

    /// The segment files a snapshot of a range over `intervals` carries with its SST files
    /// (FR-013), each as [`Shipped::encode`] makes it: those of every index this node built
    /// with a segment over one of the intervals. An empty end has no bound.
    pub fn ship_over(&self, intervals: &[(Vec<u8>, Vec<u8>)]) -> Vec<Vec<u8>> {
        let overlaps = |f: &SegmentFile| {
            intervals.iter().any(|(start, end)| {
                (f.end.is_empty() || *start < f.end) && (end.is_empty() || f.start < *end)
            })
        };
        let shipped = self.built.keys().filter_map(|id| Some((id, self.ship(id)?)));
        shipped.filter(|(_, s)| s.files.iter().any(overlaps)).map(|(id, s)| s.encode(id)).collect()
    }

    /// Writes the segments of index `id` to the store, replacing its previous file.
    fn write_file(&self, id: &[u8; 12]) -> Result<(), String> {
        let (Some(store), Some(shipped)) = (&self.store, self.ship(id)) else { return Ok(()) };
        store.put(id, &shipped.encode(id))
    }

    /// The segment files of index `id` as this node built them, covering the versions up to
    /// the mark it applied (FR-012): `None` for an index it has not built, a `vectorSearch`
    /// index or one on a view, which a replica builds from the documents.
    pub fn ship(&self, id: &[u8; 12]) -> Option<Shipped> {
        let b = self.built.get(id)?;
        match &b.indexer {
            Segments::Search(ix) if b.view.is_none() => Some(Shipped {
                collection: b.collection,
                version: b.version,
                files: ix.dump(b.applied),
            }),
            _ => None,
        }
    }

    /// Keeps the segment files of index `id` that arrived with a range's SST files, to serve
    /// them the first time a query needs the index (FR-013).
    pub fn receive(&mut self, id: [u8; 12], shipped: Shipped) {
        self.built.remove(&id);
        self.received.insert(id, shipped);
    }
}

impl Shipped {
    /// The segment file of a range snapshot that carries these segments of index `id` (FR-013).
    pub fn encode(&self, id: &[u8; 12]) -> Vec<u8> {
        let mut e = teseladb_types::Encoder::new();
        e.bytes(id).u32(self.collection).u32(self.version).u32(self.files.len() as u32);
        for f in &self.files {
            e.bytes(&f.start).bytes(&f.end).bytes(&f.bytes);
        }
        e.finish()
    }

    /// The index and segments that a file written by [`Shipped::encode`] carries.
    pub fn decode(b: &[u8]) -> Result<([u8; 12], Shipped), teseladb_types::DecodeError> {
        let mut d = teseladb_types::Decoder::new(b);
        let id: [u8; 12] = d
            .bytes()?
            .try_into()
            .map_err(|_| teseladb_types::DecodeError::InvalidValue { offset: 0 })?;
        let (collection, version, n) = (d.u32()?, d.u32()?, d.u32()?);
        let mut files = Vec::new();
        for _ in 0..n {
            let (start, end) = (d.bytes()?.to_vec(), d.bytes()?.to_vec());
            files.push(SegmentFile { start, end, bytes: d.bytes()?.to_vec() });
        }
        d.finish()?;
        Ok((id, Shipped { collection, version, files }))
    }
}

/// The segments of `entry` on `coll` that arrived with the SST files, when they serve its
/// version and other analyzers did not make them, with the mark they cover.
fn from_received(
    shipped: Shipped,
    coll: &CollectionEntry,
    entry: &SearchIndexEntry,
) -> Option<(Segments, Hlc)> {
    if shipped.version != entry.version || shipped.collection != coll.id {
        return None;
    }
    let (Segments::Search(empty), _, None) = definition_of(entry).ok()? else { return None };
    let applied = shipped
        .files
        .iter()
        .map(|f| Indexer::applied_of(&f.bytes).ok())
        .min()
        .flatten()
        .unwrap_or(Hlc { physical: 0, logical: 0 });
    let ix = Indexer::load(empty.definition().clone(), &shipped.files).ok()?;
    Some((Segments::Search(Box::new(ix)), applied))
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

/// Brings the segments of `entry` on `coll` (of namespace `ns`) up to the resolved marks of its
/// ranges and runs `f` over them. Without a builder (FR-014), it first builds them when this node
/// has none for the definition version; with one, it serves what the builder built, an older
/// definition while a new one is built, and refuses an index not built yet as `refsearch` does
/// while in `INITIAL_SYNC` (measured 02-11-2026, `scratchpad/lane3m/m32.js`).
pub fn with_index<B: Backend + ?Sized, R>(
    backend: &mut B,
    shared: &Shared,
    ns: &Namespace,
    coll: &CollectionEntry,
    entry: &SearchIndexEntry,
    synonyms: &[(String, Vec<Document>)],
    f: impl FnOnce(&Segments, &BTreeMap<Vec<u8>, Document>) -> R,
) -> Result<R, CommandError> {
    if !lock(shared).search.background {
        return with_built(backend, shared, coll, entry, synonyms, f);
    }
    let Some(mut built) = borrow(backend, shared, &entry.id, coll.id)? else {
        return Err(query_error(state_message(entry, coll, ns, "INITIAL_SYNC")));
    };
    let caught =
        catch_up(backend, coll, &mut built).and_then(|()| built.indexer.set_synonyms(synonyms));
    let out = caught.map(|()| f(&built.indexer, &built.stored));
    give_back(shared, entry.id, built, false)?;
    out
}

/// Builds this node's segments of `entry` on `coll` for its definition version, over the
/// documents written up to the resolved marks of its ranges (FR-014, T040): the builder's work.
/// While it runs, the index shows `BUILDING` and queries keep the version served before; a
/// document the definition cannot analyze leaves it `FAILED`.
pub fn build_index<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &Shared,
    coll: &CollectionEntry,
    entry: &SearchIndexEntry,
) -> Result<(), CommandError> {
    let received = lock(shared).search.received.remove(&entry.id);
    let mut built = fresh(coll, entry, received)?;
    let serving = Served {
        version: built.version,
        defined_ms: built.defined_ms,
        definition: built.definition.clone(),
        failure: None,
    };
    lock(shared).search.building.insert(entry.id, serving);
    let caught = catch_up_to(backend, coll, &mut built, false);
    lock(shared).search.building.remove(&entry.id);
    if let Err(e) = caught {
        fail(shared, entry, built, &e);
        return Err(e);
    }
    give_back(shared, entry.id, built, true)
}

/// The segments of index `id` on collection `coll` that this node built, taken out while one
/// operation uses them: an operation that finds them taken waits for them, no longer than its
/// `maxTimeMS`. `None` when there are none.
fn borrow<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &Shared,
    id: &[u8; 12],
    coll: u32,
) -> Result<Option<Built>, CommandError> {
    loop {
        {
            let mut indexes = lock(shared);
            let search = &mut indexes.search;
            if let Some(b) = search.built.remove(id) {
                if b.collection != coll {
                    return Ok(None);
                }
                search.borrowed.insert(*id);
                return Ok(Some(b));
            }
            if !search.borrowed.contains(id) {
                return Ok(None);
            }
        }
        if !backend.pause() {
            return Err(CommandError::new(MAX_TIME_MS_EXPIRED, "operation exceeded time limit"));
        }
    }
}

/// Puts `built` back as the segments of index `id`, unless a newer version was built meanwhile;
/// writes its file when `dump` or when enough documents changed.
fn give_back(
    shared: &Shared,
    id: [u8; 12],
    mut built: Built,
    dump: bool,
) -> Result<(), CommandError> {
    let dump = dump || built.pending >= DUMP_EVERY;
    if dump {
        built.pending = 0;
    }
    let mut indexes = lock(shared);
    let search = &mut indexes.search;
    search.borrowed.remove(&id);
    if search.built.get(&id).is_some_and(|b| b.version > built.version) {
        return Ok(());
    }
    if search.failed.get(&id).is_some_and(|f| f.version <= built.version) {
        search.failed.remove(&id);
    }
    search.built.insert(id, built);
    if dump {
        search.write_file(&id).map_err(|e| {
            CommandError::new(INTERNAL_ERROR, format!("cannot write a search segment: {e}"))
        })?;
    }
    Ok(())
}

/// New segments of `entry` on `coll`: those `received` (a file kept before a restart or shipped
/// with a range) when they serve its version, empty ones otherwise.
fn fresh(
    coll: &CollectionEntry,
    entry: &SearchIndexEntry,
    received: Option<Shipped>,
) -> Result<Built, CommandError> {
    let (indexer, definition, view) = definition_of(entry)?;
    let shipped = received.and_then(|r| from_received(r, coll, entry));
    let (indexer, applied) = shipped.unwrap_or((indexer, Hlc { physical: 0, logical: 0 }));
    Ok(Built {
        collection: coll.id,
        version: entry.version,
        defined_ms: entry.defined_ms,
        definition,
        indexer,
        applied,
        view,
        stored: BTreeMap::new(),
        pending: 0,
    })
}

/// Records that building `built` failed with `e` when a document cannot be analyzed: the index
/// is `FAILED` (measured: `replication failed (action=drop): minGram must be greater than zero`).
fn fail(shared: &Shared, entry: &SearchIndexEntry, built: Built, e: &CommandError) {
    if e.code != UNKNOWN_ERROR {
        return;
    }
    let failed = Served {
        version: built.version,
        defined_ms: built.defined_ms,
        definition: built.definition,
        failure: Some(format!("replication failed (action=drop): {}", e.errmsg)),
    };
    lock(shared).search.failed.insert(entry.id, failed);
}

/// [`with_index`] without a builder: builds the segments in the command when this node has
/// none for the definition version.
fn with_built<B: Backend + ?Sized, R>(
    backend: &mut B,
    shared: &Shared,
    coll: &CollectionEntry,
    entry: &SearchIndexEntry,
    synonyms: &[(String, Vec<Document>)],
    f: impl FnOnce(&Segments, &BTreeMap<Vec<u8>, Document>) -> R,
) -> Result<R, CommandError> {
    let (taken, received) = {
        let mut indexes = lock(shared);
        (indexes.search.built.remove(&entry.id), indexes.search.received.remove(&entry.id))
    };
    let (mut built, fresh_build) = match taken {
        Some(b) if b.version == entry.version && b.collection == coll.id => (b, false),
        _ => (fresh(coll, entry, received)?, true),
    };
    if let Err(e) = catch_up(backend, coll, &mut built) {
        if fresh_build {
            fail(shared, entry, built, &e);
        }
        return Err(e);
    }
    let out = built.indexer.set_synonyms(synonyms).map(|()| f(&built.indexer, &built.stored));
    give_back(shared, entry.id, built, fresh_build)?;
    out
}

/// Applies to `built` what the ranges of its collection wrote after its mark, once every range
/// reaches the read's mark.
fn catch_up<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    built: &mut Built,
) -> Result<(), CommandError> {
    catch_up_to(backend, coll, built, true)
}

/// [`catch_up`]; without `wait`, up to the marks the ranges have resolved now (the builder's fixed
/// mark, FR-014: no read asks it for a later one).
fn catch_up_to<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    built: &mut Built,
    wait: bool,
) -> Result<(), CommandError> {
    let (start, end) = documents_of(coll);
    if behind_history(built.applied, history_floor(backend)?) {
        // The ranges may have dropped the deletions after the mark: start over from a scan of
        // what they hold now (T049).
        built.indexer = built.indexer.emptied();
        built.applied = Hlc { physical: 0, logical: 0 };
        built.stored.clear();
    }
    // The search waits until every range's segments reach the read's mark, its
    // `search_applied_ts` (FR-010), no longer than the command's `maxTimeMS`.
    let target = backend.read_mark().map_or_else(|| backend.cluster_mark(), |m| m.at);
    let lag = target.physical.saturating_sub(built.applied.physical);
    INDEXER_LAG_MS.store(lag, Ordering::Relaxed);
    let ranges = loop {
        let ranges = backend.range_changes(&start, &end, built.applied)?;
        if !wait || ranges.iter().all(|r| r.resolved >= target) {
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
    built.pending += newest.len();
    built.indexer.apply(newest)?;
    built.applied = resolved;
    Ok(())
}

/// The oldest mark whose versions the ranges still keep: the wall clock less the history
/// retention (`teselaHistoryRetention`, D-020); the GC threshold of every range is at or below it.
fn history_floor<B: Backend + ?Sized>(backend: &mut B) -> Result<u64, CommandError> {
    let params = backend.cluster_parameters()?;
    let stored = params.get(super::parameters::HISTORY_RETENTION).map(Vec::as_slice);
    let retention = super::parameters::history_retention_ms(stored);
    Ok(backend.wall_ms().saturating_sub(retention))
}

/// Whether segments that applied the versions up to `applied` can no longer catch up: the GC
/// may have dropped a deletion above their mark, so reading the changes would keep a document
/// that no longer exists. Segments that applied nothing scan the whole state anyway.
pub(crate) fn behind_history(applied: Hlc, floor_ms: u64) -> bool {
    applied.physical != 0 && applied.physical < floor_ms
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
    let start = backend.now_micros();
    let out = answer(backend, shared, ns, stage, spec);
    QUERIES.fetch_add(1, Ordering::Relaxed);
    QUERY_MICROS.fetch_add(backend.now_micros().saturating_sub(start), Ordering::Relaxed);
    out
}

/// What [`source`] answers.
fn answer<B: Backend + ?Sized>(
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
    let failed = lock(shared).search.served(&entry.id);
    if failed.is_some_and(|s| s.version == entry.version && s.failure.is_some()) {
        return Err(query_error(state_message(&entry, &coll, ns, "FAILED")));
    }
    let synonyms = synonym_documents(backend, ns, &entry)?;
    let found = with_index(backend, shared, ns, &coll, &entry, &synonyms, |ix, stored| {
        match (ix, meta_only) {
            (Segments::Search(ix), true) => ix.meta(spec).map(|d| vec![plain(d)]),
            (Segments::Search(ix), false) => {
                ix.search(spec).map(|f| scored(as_read(f, stored, view.is_some()), SEARCH_SCORE))
            }
            (Segments::Vector(_), _) => Err(vector_search::not_search(&name)),
        }
    })?;
    found.map_err(|e| query_error(e.0))
}

/// The documents of the collection of each synonym mapping of `entry`, in the database of `ns`,
/// read at the query's mark (an absent collection has none).
fn synonym_documents<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    entry: &SearchIndexEntry,
) -> Result<Vec<(String, Vec<Document>)>, CommandError> {
    if entry.kind != SearchIndexKind::Search {
        return Ok(Vec::new());
    }
    let (raw, _) = split_definition(&entry.definition)?;
    let def = Definition::parse(&raw).map_err(|e| query_error(e.message("")))?;
    let mut out = Vec::new();
    for s in &def.synonyms {
        let source = Namespace { db: ns.db.clone(), coll: s.collection.clone() };
        let docs = super::aggregate::read_through(backend, &source, &[], None, ReadConcern::Local)?;
        out.push((s.name.clone(), docs));
    }
    Ok(out)
}

/// The error of a query on an index in `state` (measured: `FAILED` after a failed build,
/// `INITIAL_SYNC` while the first build runs).
fn state_message(
    entry: &SearchIndexEntry,
    coll: &CollectionEntry,
    ns: &Namespace,
    state: &str,
) -> String {
    let id: String = entry.id.iter().map(|b| format!("{b:02x}")).collect();
    let u: String = coll.uuid.iter().map(|b| format!("{b:02x}")).collect();
    let uuid = format!("{}-{}-{}-{}-{}", &u[..8], &u[8..12], &u[12..16], &u[16..20], &u[20..]);
    format!(
        "cannot query search index {id} (index {} collection {} ({uuid}) in database {}) while in \
         state {state}",
        entry.name, ns.coll, ns.db
    )
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
    let found = with_index(backend, shared, ns, &coll, &entry, &[], |ix, stored| match ix {
        Segments::Vector(ix) => {
            ix.search(&query).map(|f| scored(as_read(f, stored, on_view), VECTOR_SEARCH_SCORE))
        }
        Segments::Search(_) => Err(vector_search::not_vector(&query.path)),
    })?;
    found.map_err(|e: SearchError| query_error(e.0))
}

/// The documents found, with their score as metadata `name` and their sequence token.
fn scored(found: Vec<Found>, name: &str) -> Vec<Scored> {
    found
        .into_iter()
        .map(|f| {
            let score = Value::Double(f64::from(f.score));
            let mut meta = BTreeMap::from([(name.to_owned(), score)]);
            if let Some(t) = f.token {
                meta.insert(SEQUENCE_TOKEN.to_owned(), Value::String(t));
            }
            if let Some(d) = f.details {
                meta.insert(SCORE_DETAILS.to_owned(), Value::Document(d.to_document()));
            }
            if let Some(h) = f.highlights {
                meta.insert(HIGHLIGHTS.to_owned(), h);
            }
            Scored { doc: f.doc, meta }
        })
        .collect()
}

/// A document without metadata.
fn plain(doc: Document) -> Scored {
    Scored { doc, meta: BTreeMap::new() }
}

/// The `statusDetail` of an index that this node serves: its host and the version it serves.
pub fn status_detail(host: u64, served: &Served) -> Document {
    let (status, queryable) = match served.failure {
        Some(_) => ("FAILED", false),
        None => ("READY", true),
    };
    detail(host, served, status, queryable)
}

/// The `statusDetail` of an index whose first build runs on this node (measured,
/// `scratchpad/lane3m/m32.js`): `BUILDING` and not queryable, with the definition it builds.
pub fn building_detail(host: u64, building: &Served) -> Document {
    detail(host, building, "BUILDING", false)
}

fn detail(host: u64, served: &Served, status: &str, queryable: bool) -> Document {
    // `createdAt` keeps whole seconds (measured).
    let created = served.defined_ms - served.defined_ms.rem_euclid(1000);
    let version = Document::new()
        .with("version", i32::try_from(served.version).unwrap_or(i32::MAX))
        .with("createdAt", Value::DateTime(created));
    let mut main = Document::new()
        .with("status", status)
        .with("queryable", queryable)
        .with("definitionVersion", version)
        .with("definition", served.definition.clone());
    if let Some(message) = &served.failure {
        main.push("message", message.as_str());
    }
    Document::new()
        .with("hostname", format!("{host:024x}"))
        .with("status", status)
        .with("queryable", queryable)
        .with("mainIndex", main)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_behind_the_history_floor_start_over() {
        let at = |physical| Hlc { physical, logical: 0 };
        assert!(!behind_history(at(0), 5_000));
        assert!(behind_history(at(4_999), 5_000));
        assert!(!behind_history(at(5_000), 5_000));
        assert!(!behind_history(at(9_000), 5_000));
    }
}
