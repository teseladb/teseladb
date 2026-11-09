//! The planner at work on a node (010 FR-001, T019): the indexes of a collection as the planner
//! sees them, the plans of a read, their trial on the node's own documents and the run of the
//! plan that wins, as RefDb's classic engine does (measured on a `refrouter` 9.0.4, 01-11-2026).
//! An index scan reads the entries within the plan's intervals in key order (backwards when the
//! plan reads the index the other way), takes each document once and keeps those the rest of
//! the filter matches.

use teseladb_index::planner_api::{CatalogIndex, IndexCatalog, IndexState};
use teseladb_index::spec::catalog_index;
use teseladb_keys::{
    DOCUMENT_BUCKETS, DOCUMENTS, Direction, FieldBounds, Hlc, Interval, KeyBuilder, KeyRange,
    next_prefix, values_len,
};
use teseladb_meta::catalog::CollectionEntry;
use teseladb_meta::collection_types::CollectionKind;
use teseladb_meta::indexes::{IndexEntry, IndexState as EntryState};
use teseladb_plan::checker::{FieldWalk, Visit, walk};
use teseladb_plan::cost::{TRIAL_RESULTS, TRIAL_WORKS, Trial, ranked, score, trial_works};
use teseladb_plan::logical::Bound;
use teseladb_plan::rules::{
    Access, IndexMeta, IndexScan, Request, branch_scans, candidates, joined_or,
};
use teseladb_range::read::ReadConcern;
use teseladb_bson::{Document, RawDoc, RawValue, Value};

use super::index_maintenance::MULTIKEY_MARKS;
use super::mutation::from_mql;
use super::{Backend, Span, query};
use crate::error::{CommandError, INTERNAL_ERROR};

/// The entries an index scan reads at a time.
const KEY_CHUNK: usize = 256;

/// The field of the profiler that holds how long planning took (`planningTimeMicros`).
pub(crate) const PLANNING_TIME: &str = "planningTimeMicros";

/// The field of a noted plan that lists the indexes its winner scans, for `$indexStats`
/// ([`crate::ops::OpReport::used`]) of a write, which knows no namespace but the command's:
/// `{names}`. A control character starts it, so that no field of RefDb's can be it.
pub(crate) const USED_INDEXES: &str = "\u{1}used";

/// The field of a noted plan that says several plans competed (`fromMultiPlanner` of
/// `$queryStats`); a control character starts it, as [`USED_INDEXES`].
pub(crate) const MULTI_PLANNED: &str = "\u{1}multi";
/// The field of a noted plan that says its plan came from the plan cache (`fromPlanCache` of
/// `$queryStats`), as [`USED_INDEXES`].
pub(crate) const FROM_CACHE: &str = "\u{1}cached";
/// The field of a noted plan that holds why its cached plan was dropped (`replanReason`), as
/// [`USED_INDEXES`]; `null` when it was not.
pub(crate) const REPLANNED: &str = "\u{1}replanned";
/// How many times the works of its entry's trial a cached plan may work before it gives a batch
/// or ends; past that, the plans compete again (`internalQueryCacheEvictionRatio`).
const EVICTION_RATIO: u64 = 10;
/// The field of a noted plan that says its index reads in the order of the read's sort.
pub(crate) const SORT_SERVED: &str = "\u{1}sorted";

/// What a read or write of one `_id` notes: the index of `_id`, used without planning (with
/// `planned`, the time it spent planning, none, as a write's plan).
pub(crate) fn used_id_index(planned: bool) -> Document {
    let names = vec![Value::from(ID_INDEX)];
    let used = Document::new().with(USED_INDEXES, Document::new().with("names", names));
    match planned {
        true => used.with(PLANNING_TIME, 0i64),
        false => used,
    }
}

/// The names of the indexes `access` scans.
pub(crate) fn scanned(indexes: &[PlanIndex], access: &Access) -> Vec<String> {
    let scans: Vec<&IndexScan> = match access {
        Access::Collscan => Vec::new(),
        Access::Index(s) => vec![s],
        Access::Or(list) => list.iter().collect(),
        Access::AndHash { scans, .. } => scans.iter().collect(),
    };
    let mut names: Vec<String> = Vec::new();
    for s in scans {
        let name = indexes[s.index].entry.name.clone();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// The name of the index of `_id`.
const ID_INDEX: &str = "_id_";

/// An index of a collection as the planner and `explain` see it.
#[derive(Clone, Debug)]
pub(crate) struct PlanIndex {
    /// The catalog's entry.
    pub entry: IndexEntry,
    /// Its key pattern as written.
    pub pattern: Document,
    /// The paths through arrays of each field (`multiKeyPaths`).
    pub multikey_paths: Vec<(String, Vec<String>)>,
    /// What the planner knows of it.
    pub meta: IndexMeta,
    /// Its definition for key intervals.
    catalog: CatalogIndex,
    /// The directions of its fields.
    dirs: Vec<Direction>,
    /// The index of `_id`, whose entries are the collection's documents.
    id_index: bool,
    /// The collection keeps its documents in `_id` order without hash bucket (a clustered
    /// collection, 015 FR-009, or one created in that order, D-153): the entries of its index of
    /// `_id` are read as an interval.
    ordered: bool,
    /// The collection is clustered: `explain` shows the scans of its `_id` as a clustered scan.
    clustered: bool,
}

impl PlanIndex {
    /// Unique.
    pub fn unique(&self) -> bool {
        self.catalog.unique
    }

    /// The index of `_id` of a clustered collection, whose entries are its documents (015
    /// FR-009): `explain` shows its scans as RefDb's `CLUSTERED_IXSCAN`. A collection created in
    /// `_id` order (D-153) shows the index `_id_` of any collection.
    pub fn clustered(&self) -> bool {
        self.clustered
    }

    /// Sparse.
    pub fn sparse(&self) -> bool {
        self.catalog.sparse
    }

    /// Partial.
    pub fn partial(&self) -> bool {
        self.catalog.partial
    }

    /// The index as the plan cache key sees it (H-028): a hidden one counts too (measured on a
    /// `refrouter` 9.0.4).
    pub(crate) fn key_index(&self) -> super::plan_hash::KeyIndex {
        let collation = RawDoc::from_bytes(&self.entry.spec).ok().and_then(|spec| {
            let c = match Document::from(spec).get("collation") {
                Some(Value::Document(c)) => c.clone(),
                _ => return None,
            };
            super::views::spec_of(Some(&c)).ok().flatten()
        });
        super::plan_hash::KeyIndex {
            fields: self.pattern.iter().map(|(k, _)| k.to_owned()).collect(),
            sparse: self.catalog.sparse,
            collation,
        }
    }
}

/// The indexes of a read as the plan cache key sees them (H-028).
pub(crate) fn key_indexes(indexes: &[PlanIndex]) -> Vec<super::plan_hash::KeyIndex> {
    indexes.iter().map(PlanIndex::key_index).collect()
}

std::thread_local! {
    /// The indexes query settings allow on one collection while a command runs (010 FR-023):
    /// the collection's identifier and the allowed names or key patterns.
    static ALLOWED: std::cell::RefCell<Option<(u32, Vec<Value>)>> =
        const { std::cell::RefCell::new(None) };
}

/// Runs `f` with only the indexes `allowed` (names or key patterns) plannable on collection
/// `coll`.
pub(crate) fn with_allowed_indexes<R>(coll: u32, allowed: Vec<Value>, f: impl FnOnce() -> R) -> R {
    ALLOWED.with(|a| *a.borrow_mut() = Some((coll, allowed)));
    let out = f();
    ALLOWED.with(|a| *a.borrow_mut() = None);
    out
}

/// Whether query settings let the planner use index `name` with key `pattern` of `coll`.
fn allowed_by_settings(coll: u32, name: &str, pattern: &Document) -> bool {
    ALLOWED.with(|a| match &*a.borrow() {
        Some((c, allowed)) if *c == coll => allowed.iter().any(|v| match v {
            Value::String(n) => n == name,
            Value::Document(k) => k == pattern,
            _ => false,
        }),
        _ => true,
    })
}

/// The ready indexes of `coll` the planner may consider, in the catalog's order: the index of
/// `_id` first, as RefDb lists it; none for a collection that is not a plain one (its
/// documents' keys are not those of their `_id`).
pub(crate) fn plan_indexes<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
) -> Result<Vec<PlanIndex>, CommandError> {
    if !matches!(
        super::kinds::kind_of(coll)?,
        CollectionKind::Normal | CollectionKind::Clustered { .. }
    ) {
        return Ok(Vec::new());
    }
    stored_indexes(backend, coll)
}

/// The indexes of `coll` as they are stored, whatever its kind: those of a time series index
/// its buckets (015 FR-008).
pub(crate) fn stored_indexes<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
) -> Result<Vec<PlanIndex>, CommandError> {
    // A collection with a shard key keeps `_id` in an index of its own (018 FR-001).
    let sharded = super::sharding::is_sharded(coll);
    let in_order = in_id_order(coll)?;
    let is_clustered = matches!(super::kinds::kind_of(coll)?, CollectionKind::Clustered { .. });
    let id_entry = IndexEntry {
        id: if sharded { teseladb_range::capped::ID_INDEX } else { DOCUMENTS },
        collection: coll.id,
        name: ID_INDEX.to_owned(),
        spec: super::catalog::id_index(coll)?.to_bytes()?,
        buckets: 0,
        state: EntryState::Ready,
    };
    let mut out = Vec::new();
    for entry in std::iter::once(id_entry).chain(backend.indexes(coll.id)?) {
        if entry.state != EntryState::Ready {
            continue;
        }
        let id_index = entry.id == DOCUMENTS;
        let spec = RawDoc::from_bytes(&entry.spec)?;
        let ready = IndexState::Ready(Hlc { physical: 0, logical: 0 });
        let catalog = catalog_index(spec, entry.id, entry.buckets, ready)
            .map_err(|e| CommandError::new(INTERNAL_ERROR, e.reason))?;
        let Some(mut catalog) = catalog else { continue };
        // The index of `_id` is unique without saying so in its definition.
        let of_id = id_index || (sharded && entry.id == teseladb_range::capped::ID_INDEX);
        catalog.unique |= of_id;
        let pattern = Document::from(RawDoc::from_bytes(&catalog.key_pattern)?);
        // An ordered field, or a hashed one, which an equality scans by its hash (010 T048).
        let ordered = pattern.iter().all(|(_, v)| {
            matches!(v, Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_))
                || is_hashed(v)
        });
        let plannable = ordered
            && entry.buckets == 0
            && !catalog.hidden
            && !catalog.partial
            && !catalog.sparse
            && spec.get("collation").is_none()
            && pattern.iter().all(|(k, _)| !k.contains('$'))
            && allowed_by_settings(coll.id, &entry.name, &pattern);
        let multikey_paths = if of_id {
            pattern.iter().map(|(k, _)| (k.to_owned(), Vec::new())).collect()
        } else {
            multikey_paths(backend, (coll.tenant, coll.id), &entry, &pattern)?
        };
        let key = pattern
            .iter()
            .map(|(k, v)| {
                let dir = match v {
                    _ if is_hashed(v) => teseladb_plan::rules::HASHED,
                    _ if query_number(v) < 0.0 => -1,
                    _ => 1,
                };
                (k.to_owned(), dir)
            })
            .collect();
        let meta = IndexMeta {
            name: entry.name.clone(),
            key,
            multikey: multikey_paths.iter().any(|(_, p)| !p.is_empty()),
            plannable,
        };
        let dirs = catalog.def.key_spec().fields.iter().map(|f| f.direction).collect();
        let (ordered, clustered) = (in_order && id_index, is_clustered && id_index);
        out.push(PlanIndex {
            entry,
            pattern,
            multikey_paths,
            meta,
            catalog,
            dirs,
            id_index,
            ordered,
            clustered,
        });
    }
    Ok(out)
}

/// Whether an index of key `pattern` keeps the paths through arrays of its fields: btree and
/// `2dsphere` indexes do, the others (hashed, text, 2d, wildcard) do not (RefDb r9.0.4
/// `collection_impl.cpp`, `indexTypeSupportsPathLevelMultikeyTracking`).
pub(crate) fn tracks_multikey_paths(pattern: &Document) -> bool {
    pattern
        .iter()
        .all(|(k, v)| !k.ends_with("$**") && !matches!(v, Value::String(s) if s != GEO_2DSPHERE))
}

/// Whether the `IXSCAN` of an index of key `pattern` shows `multiKeyPaths`: those that keep the
/// paths, and wildcard indexes, whose stage names the path it expanded.
pub(crate) fn shows_multikey_paths(pattern: &Document) -> bool {
    !pattern.iter().any(|(_, v)| matches!(v, Value::String(s) if s != GEO_2DSPHERE))
}

/// The index type that keeps the paths through arrays besides btree.
const GEO_2DSPHERE: &str = "2dsphere";

/// Whether an index field is hashed.
fn is_hashed(v: &Value) -> bool {
    matches!(v, Value::String(s) if s == "hashed")
}

fn query_number(v: &Value) -> f64 {
    match v {
        Value::Int32(n) => f64::from(*n),
        Value::Int64(n) => *n as f64,
        Value::Double(x) => *x,
        _ => 1.0,
    }
}

/// The paths through arrays of each field of index `entry`, from the marks its writes left.
pub(crate) fn multikey_paths<B: Backend + ?Sized>(
    backend: &mut B,
    (tenant, collection): (u32, u32),
    entry: &IndexEntry,
    pattern: &Document,
) -> Result<Vec<(String, Vec<String>)>, CommandError> {
    let mut out: Vec<(String, Vec<String>)> =
        pattern.iter().map(|(k, _)| (k.to_owned(), Vec::new())).collect();
    let start = KeyBuilder::index_prefix(tenant, collection, MULTIKEY_MARKS + entry.id);
    let end = KeyBuilder::index_prefix(tenant, collection, MULTIKEY_MARKS + entry.id + 1);
    let head = start.as_bytes().len();
    let rows = backend.scan(start.as_bytes(), end.as_bytes(), usize::MAX, ReadConcern::Local)?;
    // A mark is the field, a zero byte and the path, for a field of the index: the writes of the
    // index leave no other.
    for (key, _) in rows {
        let mut parts = key.get(head..).unwrap_or_default().splitn(2, |b| *b == 0);
        let field = String::from_utf8_lossy(parts.next().unwrap_or_default()).into_owned();
        let path = String::from_utf8_lossy(parts.next().unwrap_or_default()).into_owned();
        out.iter_mut().filter(|(f, _)| *f == field).for_each(|(_, paths)| paths.push(path.clone()));
    }
    Ok(out)
}

/// What a read asks of the planner.
pub(crate) struct ReadPlan<'a> {
    /// The filter as given (what matches).
    pub filter: &'a Document,
    /// The filter as RefDb writes it back after parsing it.
    pub parsed: &'a Document,
    /// The sort.
    pub sort: Vec<(String, i32)>,
    /// The documents the read takes at most after its skip (`None`: all).
    pub limit: Option<u64>,
    /// The index `hint` names, by position.
    pub hint: Option<usize>,
    /// The command's variables.
    pub vars: Option<&'a Document>,
    /// The read concern.
    pub concern: ReadConcern,
    /// Whether plans may intersect index scans (`internalQueryPlannerEnableHashIntersection`).
    pub intersect: bool,
    /// Whether the read wants the index key each document was found by (`{$meta: "indexKey"}`).
    pub index_keys: bool,
    /// Whether it gives out the index key alone (`returnKey`): a plan whose filter its bounds
    /// answer and whose index has the sort's fields reads no document, and RefDb's score gives
    /// it the bonus of a plan without `FETCH` (measured on a `refrouter` 9.0.4 on 05-11-2026).
    pub key_only: bool,
}

/// A plan and what running it counted.
#[derive(Clone, Debug)]
pub(crate) struct Ran {
    /// How it reads.
    pub access: Access,
    /// Its trial (or its run, for the winner).
    pub trial: Trial,
    /// Index entries read.
    pub keys: u64,
    /// The entries among them outside the plan's intervals, read on the way to the next interval
    /// or to the end of a scan of several intervals.
    pub past: u64,
    /// Documents read.
    pub docs: u64,
    /// Documents found, in order.
    pub found: Vec<(Vec<u8>, Vec<u8>)>,
    /// The index key each of them was found by, when the read wants them
    /// ([`ReadPlan::index_keys`]): none for a collection scan or an intersection.
    pub index_keys: Vec<Option<Document>>,
    /// The unit of work at which the plan gave each document.
    given_at: Vec<u64>,
    /// What its trial counted, when plans competed.
    pub tried: Option<Tried>,
    /// The unit of work at which the plan read each document.
    docs_at: Vec<u64>,
    /// The entries each scan of an intersection read, in its order.
    pub child_keys: Vec<u64>,
    /// The entries each of those scans read outside its intervals.
    pub child_past: Vec<u64>,
    /// The entries each of those scans dropped as repeats of a document it gave (a multikey
    /// index, `dupsDropped`).
    pub child_dups: Vec<u64>,
    /// What each scan of a `$or` gave the `OR`: its entries, or the documents its own filter
    /// kept.
    pub child_given: Vec<u64>,
    /// Seeks of its scans: one each, and one more for each key outside the intervals of a scan
    /// that checks its keys.
    pub seeks: u64,
}

/// What a plan counted at the end of the trial, and its score.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tried {
    /// Index entries read.
    pub keys: u64,
    /// Documents read.
    pub docs: u64,
    /// Documents given.
    pub returned: u64,
    /// RefDb's score.
    pub score: f64,
    /// Units of work of the trial.
    pub works: u64,
}

impl Ran {
    /// The plan as trial `t` left it (`allPlansExecution`).
    pub fn at_trial(&self, t: Tried) -> Ran {
        let mut r = self.clone();
        r.keys = t.keys;
        r.docs = t.docs;
        // The scans of an intersection read one after the other.
        let mut left = t.keys;
        for k in &mut r.child_keys {
            let took = (*k).min(left);
            *k = took;
            left -= took;
        }
        let kept = usize::try_from(t.returned).unwrap_or(usize::MAX);
        r.found = self.found.iter().take(kept).cloned().collect();
        r.index_keys.truncate(kept);
        r
    }

    /// The plan as its trial left it, or as it ran when it had no trial.
    pub fn tried_view(&self) -> Ran {
        self.tried.map_or_else(|| self.clone(), |t| self.at_trial(t))
    }
}

/// The plans of `read` on `coll` with `indexes`, the winner first, each with its trial; `None`
/// when no index serves and the collection scan is the one plan (the caller scans).
pub(crate) fn choose<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    indexes: &[PlanIndex],
    read: &ReadPlan<'_>,
) -> Result<Option<Chosen>, CommandError> {
    let started = backend.now_micros();
    let metas: Vec<IndexMeta> = indexes.iter().map(|i| i.meta.clone()).collect();
    let request = Request {
        parsed: Some(read.parsed),
        sort: &read.sort,
        hint: read.hint,
        intersect: read.intersect,
    };
    let mut plans = candidates(&request, &metas);
    if let [Access::Or(_)] = plans.as_slice()
        && let Some(branches) = branch_scans(&request, &metas)
    {
        // The clauses' scans join in the order of their indexes (measured).
        let scans = subplanned(backend, coll, indexes, read, branches)?;
        plans = vec![Access::Or(joined_or(scans, &metas, &read.sort))];
    }
    if plans.len() < 2 {
        let Some(access) = plans.pop().filter(|a| *a != Access::Collscan) else {
            return Ok(None);
        };
        let planning_micros = backend.now_micros().saturating_sub(started);
        let winner = run(backend, coll, indexes, read, access, None)?;
        return Ok(Some(Chosen { winner, others: Vec::new(), planning_micros }));
    }
    let mut trials = trial(backend, coll, indexes, read, &plans)?;
    // The trial ends at the round where a plan gave a batch of documents or reached its end.
    let stop = trials.iter().map(|t| t.trial.works).min().unwrap_or(0);
    for t in &mut trials {
        t.trial = at_round(t, stop);
        t.tried = Some(Tried {
            keys: t.keys.min(stop),
            docs: u64::try_from(t.docs_at.iter().filter(|w| **w <= stop).count())
                .unwrap_or(u64::MAX),
            returned: t.trial.advanced,
            score: score(&t.trial),
            works: t.trial.works,
        });
    }
    // Two plans or more: the first of the order wins, and runs again without a budget.
    let order = ranked(&trials.iter().map(|t| t.trial).collect::<Vec<_>>());
    let best = &trials[order[0]];
    let planning_micros = backend.now_micros().saturating_sub(started);
    let mut winner = run(backend, coll, indexes, read, best.access.clone(), None)?;
    winner.tried = best.tried;
    let others = order[1..].iter().map(|i| trials[*i].clone()).collect();
    Ok(Some(Chosen { winner, others, planning_micros }))
}

/// The works the first round of a trial gives each plan; each round doubles them.
const FIRST_ROUND_WORKS: u64 = 128;

/// The documents that end the trial of `read`'s plans: RefDb's [`TRIAL_RESULTS`], or the read's
/// limit when it is smaller (`getTrialPeriodNumToReturn`, measured on a `refnode` 9.0.4: a `findOne`
/// ends its trial, and keeps its cached plan, at its first document).
fn trial_results(read: &ReadPlan<'_>) -> u64 {
    read.limit.filter(|l| *l > 0).map_or(TRIAL_RESULTS, |l| l.min(TRIAL_RESULTS))
}

/// Whether a plan's trial ended before its budget: it gave the trial's documents (`results`) or
/// reached its end.
fn ended(t: &Ran, results: u64) -> bool {
    t.trial.eof || t.trial.advanced >= results
}

/// The trials of `plans` up to the round where one of them ends. RefDb alternates the plans and
/// stops at that round; here every plan runs again with twice the works until one ends, so none
/// reads more than twice that round whatever the order of the plans (a plan of a broad index tried
/// before one that ends at once read the whole budget, 157.000 entries, on the internal cluster of
/// TeselaDB Cloud). The budget is RefDb's, [`trial_works`] of the collection's documents, which
/// passes its least, [`TRIAL_WORKS`], only past 33.333 documents: they are counted, which reads
/// them all, only when no plan ended within that least.
fn trial<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    indexes: &[PlanIndex],
    read: &ReadPlan<'_>,
    plans: &[Access],
) -> Result<Vec<Ran>, CommandError> {
    let mut budget = TRIAL_WORKS;
    let mut counted = false;
    let mut works = FIRST_ROUND_WORKS;
    loop {
        let mut trials = Vec::with_capacity(plans.len());
        for access in plans {
            trials.push(run(backend, coll, indexes, read, access.clone(), Some(works))?);
        }
        if trials.iter().any(|t| ended(t, trial_results(read))) {
            return Ok(trials);
        }
        if works >= budget && !counted {
            budget = trial_works(count_documents(backend, coll, read.concern)?);
            counted = true;
        }
        if works >= budget {
            return Ok(trials);
        }
        works = works.saturating_mul(2).min(budget);
    }
}

/// The plan the planner chose, and the others it tried, best first.
pub(crate) struct Chosen {
    pub(crate) winner: Ran,
    pub(crate) others: Vec<Ran>,
    /// How long choosing the winner took, its trials included, in microseconds
    /// (`planningTimeMicros`).
    pub(crate) planning_micros: u64,
}

/// The scan of each clause of a rooted `$or` that wins its own trial (`SUBPLAN`).
fn subplanned<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    indexes: &[PlanIndex],
    read: &ReadPlan<'_>,
    branches: Vec<Vec<IndexScan>>,
) -> Result<Vec<IndexScan>, CommandError> {
    let mut out = Vec::new();
    let branch_read = ReadPlan {
        filter: read.filter,
        parsed: read.parsed,
        sort: Vec::new(),
        limit: None,
        hint: None,
        vars: read.vars,
        concern: read.concern,
        intersect: false,
        index_keys: false,
        key_only: false,
    };
    for scans in branches {
        if scans.len() < 2 {
            out.extend(scans);
            continue;
        }
        let plans: Vec<Access> = scans.iter().cloned().map(Access::Index).collect();
        let trials = trial(backend, coll, indexes, &branch_read, &plans)?;
        let stop = trials.iter().map(|t| t.trial.works).min().unwrap_or(0);
        let cut: Vec<Trial> = trials.iter().map(|t| at_round(t, stop)).collect();
        let best = ranked(&cut).first().copied().unwrap_or(0);
        out.push(scans[best].clone());
    }
    Ok(out)
}

/// The documents of `coll`.
fn count_documents<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    concern: ReadConcern,
) -> Result<u64, CommandError> {
    let (start, end) = query::documents_of(coll);
    Ok(u64::try_from(backend.scan(&start, &end, usize::MAX, concern)?.len()).unwrap_or(u64::MAX))
}

/// The trial of `t` cut at round `stop`: the works up to it and the documents given by then.
fn at_round(t: &Ran, stop: u64) -> Trial {
    let given = t.given_at.iter().filter(|w| **w <= stop).count();
    Trial {
        works: stop.min(t.trial.works),
        advanced: u64::try_from(given).unwrap_or(u64::MAX),
        eof: t.trial.eof && t.trial.works <= stop,
        ..t.trial
    }
}

/// Whether scans `scans` give a key-only read (`returnKey`) without reading a document: no
/// filter is left over the bounds, and each index has the fields of the sort.
fn reads_keys_only(read: &ReadPlan<'_>, scans: &[IndexScan], indexes: &[PlanIndex]) -> bool {
    read.key_only
        && scans.iter().all(|s| {
            s.residual.is_none()
                && read.sort.iter().all(|(f, _)| indexes[s.index].pattern.get(f).is_some())
        })
}

/// The most index entries the run of a read with a limit reads first (fewer when the limit is
/// smaller); each further read takes four times as many.
const FIRST_LIMITED_ENTRIES: usize = 16;

/// Runs `access` for `read`: for a trial (`budget` works at most), until it gives a batch, or
/// to its end. A read with a limit that needs no sort, or whose index scan already gives the
/// sort's order, reads its index entries a few at a time, as RefDb's index scan does, and stops
/// at its limit: before, a `findOne` by an index of equality read every entry of its value to give
/// one document, and a YCSB scan (`_id` from a value, by `_id`, with a limit) read every document
/// after that value.
fn run<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    indexes: &[PlanIndex],
    read: &ReadPlan<'_>,
    access: Access,
    budget: Option<u64>,
) -> Result<Ran, CommandError> {
    let in_order = read.sort.is_empty() || matches!(&access, Access::Index(s) if s.sorts);
    // A scan in the read's order with nothing left to filter gives exactly what it reads: its
    // first read takes the whole limit instead of growing to it and reading it all again.
    let exact = matches!(&access, Access::Index(s)
        if s.residual.is_none() && (read.sort.is_empty() || s.sorts));
    let mut most = match (budget, read.limit) {
        (Some(b), _) => Some(usize::try_from(b.saturating_add(1)).unwrap_or(usize::MAX)),
        (None, Some(l)) if exact => Some(usize::try_from(l).unwrap_or(usize::MAX).max(1)),
        (None, Some(l)) if in_order => {
            Some(usize::try_from(l).unwrap_or(usize::MAX).clamp(1, FIRST_LIMITED_ENTRIES))
        }
        (None, _) => None,
    };
    loop {
        let (ran, cut) = run_reading(backend, coll, indexes, read, access.clone(), budget, most)?;
        match (cut, most) {
            (true, Some(m)) => most = Some(m.saturating_mul(4)),
            _ => return Ok(ran),
        }
    }
}

/// [`run`] reading at most `most` entries of an index scan of one interval; with it, whether the
/// run used them all and the scan has more, so the caller reads again with more.
fn run_reading<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    indexes: &[PlanIndex],
    read: &ReadPlan<'_>,
    access: Access,
    budget: Option<u64>,
    most: Option<usize>,
) -> Result<(Ran, bool), CommandError> {
    let collation = None;
    let plan_filter = |f: &Document| -> Result<teseladb_mql::Filter, CommandError> {
        let bytes = super::query::raw_bytes(f)?;
        let raw = RawDoc::from_bytes_with(&bytes, crate::msg::command_limits())?;
        teseladb_mql::Filter::parse(raw, teseladb_mql::FilterUse::Query).map_err(from_mql)
    };
    let ext = super::engines::NodeExtensions;
    let ctx = teseladb_mql::MatchContext { collation, ext: &ext, vars: read.vars };
    let mut ran = Ran {
        access: access.clone(),
        trial: Trial::default(),
        keys: 0,
        past: 0,
        docs: 0,
        found: Vec::new(),
        index_keys: Vec::new(),
        given_at: Vec::new(),
        tried: None,
        docs_at: Vec::new(),
        child_keys: Vec::new(),
        child_past: Vec::new(),
        child_dups: Vec::new(),
        child_given: Vec::new(),
        seeks: 0,
    };
    let scans: Vec<IndexScan> = match &access {
        Access::Collscan => {
            let limit = read.limit.filter(|_| read.sort.is_empty());
            let examined = query::for_each_match_counted(
                backend,
                query::documents_of(coll),
                read.filter,
                None,
                read.vars,
                read.concern,
                |key, bytes| {
                    ran.found.push((key, bytes.to_vec()));
                    Ok(limit.is_none_or(|l| (ran.found.len() as u64) < l))
                },
            )?;
            ran.docs = examined;
            let advanced = u64::try_from(ran.found.len()).unwrap_or(u64::MAX);
            ran.trial = Trial {
                works: examined + 1,
                advanced,
                fetches: false,
                sorts: !read.sort.is_empty(),
                intersects: false,
                eof: limit.is_none_or(|l| advanced < l),
            };
            return Ok((ran, false));
        }
        Access::Index(s) => vec![s.clone()],
        Access::Or(list) => list.clone(),
        Access::AndHash { scans, filter } => {
            return Ok((
                intersected(backend, coll, indexes, read, (scans, filter.as_ref()), ran)?,
                false,
            ));
        }
    };
    // A plan whose order is not the sort's gives nothing until it has read everything.
    let blocking = !read.sort.is_empty() && !scans.iter().all(|s| s.sorts);
    let batch = if blocking { None } else { budget.map(|_| TRIAL_RESULTS) };
    let limit = if blocking { None } else { read.limit };
    let mut seen: std::collections::BTreeSet<Vec<u8>> = std::collections::BTreeSet::new();
    let mut ended = true;
    let mut cut = false;
    let mut last_past = false;
    'scans: for s in &scans {
        let index = &indexes[s.index];
        // This scan's counters: the last of each list.
        let child = ran.child_keys.len();
        ran.child_keys.push(0);
        ran.child_past.push(0);
        ran.child_dups.push(0);
        ran.child_given.push(0);
        let residual = s.residual.as_ref().map(&plan_filter).transpose()?;
        let decode = read.index_keys && !index.id_index;
        let scanned =
            entries(backend, (coll.tenant, coll.id), index, s, (read.concern, decode), most)?;
        ran.seeks += scanned.seeks;
        let more = scanned.more;
        // A multikey scan drops the other entries of a document it gave before fetching it
        // again (`dupsDropped` of its `IXSCAN`, measured on a `refnode` 9.0.4).
        let mut own: std::collections::BTreeSet<Vec<u8>> = std::collections::BTreeSet::new();
        for entry in scanned.entries {
            if budget.is_some_and(|b| ran.keys >= b) {
                ended = false;
                break 'scans;
            }
            ran.keys += 1;
            ran.child_keys[child] += 1;
            last_past = entry.is_none();
            let Some(Entry { pk, key: index_key, doc, .. }) = entry else {
                ran.past += 1;
                ran.child_past[child] += 1;
                continue;
            };
            if index.meta.multikey && !own.insert(pk.clone()) {
                ran.child_dups[child] += 1;
                continue;
            }
            // A scan without filter gives its entries to the `OR`, which drops those it has
            // seen; one with a filter fetches each document first.
            let seen_before = seen.contains(&pk);
            if residual.is_none() {
                ran.child_given[child] += 1;
                if seen_before {
                    continue;
                }
            }
            let Some((dk, bytes)) = fetched(backend, coll, (&pk, doc), read.concern)? else {
                continue;
            };
            ran.docs += 1;
            ran.docs_at.push(ran.keys);
            let keep = match &residual {
                Some(f) => f.matches(RawDoc::from_bytes(&bytes)?, &ctx).map_err(from_mql)?,
                None => true,
            };
            if keep && residual.is_some() {
                ran.child_given[child] += 1;
            }
            if keep && !seen_before {
                seen.insert(pk);
                if read.index_keys {
                    let key = match index.id_index {
                        true => id_key(&bytes)?,
                        false => index_key,
                    };
                    ran.index_keys.push(key);
                }
                ran.found.push((dk, bytes));
                ran.given_at.push(ran.keys);
                let n = u64::try_from(ran.found.len()).unwrap_or(u64::MAX);
                if batch.is_some_and(|b| n >= b) || limit.is_some_and(|l| n >= l) {
                    ended = false;
                    break 'scans;
                }
            }
        }
        // Every entry read and the scan goes on past them: the caller reads again with more.
        if more {
            cut = true;
            break 'scans;
        }
    }
    // A key read past the last interval is the work that ends the scan (measured).
    let ends_past = ended && last_past;
    let input = ran.keys + u64::from(ended && !ends_past);
    let advanced = u64::try_from(ran.found.len()).unwrap_or(u64::MAX);
    let works = if blocking {
        let out = read.limit.map_or(advanced, |l| advanced.min(l));
        ran.given_at = (1..=out).map(|k| input + k).collect();
        if ended { input + out } else { input }
    } else {
        input
    };
    ran.trial = Trial {
        works,
        advanced,
        fetches: !reads_keys_only(read, &scans, indexes),
        sorts: blocking,
        intersects: false,
        eof: ended,
    };
    Ok((ran, cut))
}

/// Whether `coll` keeps its documents in `_id` order without hash bucket (a clustered collection,
/// 015 FR-009, or one created in that order, D-153): its document keys are the keys of its index
/// of `_id`.
pub(crate) fn in_id_order(coll: &CollectionEntry) -> Result<bool, CommandError> {
    super::catalog::in_id_order(coll)
}

/// The document an index entry with primary key `pk` names, with its key. An index entry and its
/// document are written in the same write, but a read that is not at a snapshot reads the index
/// and the documents one after the other: a delete in between leaves an entry whose document is
/// gone (`None`), which the read skips, as RefDb skips a record deleted while a read yields.
fn indexed_document<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    pk: &[u8],
    concern: ReadConcern,
) -> Result<Option<(Vec<u8>, Vec<u8>)>, CommandError> {
    // The buckets of a time series are kept in `_id` order without hash bucket, as the
    // documents of a collection with a shard key (015 FR-008).
    let unbucketed = super::sharding::is_sharded(coll)
        || in_id_order(coll)?
        || matches!(super::kinds::kind_of(coll)?, CollectionKind::TimeSeries(_));
    let key = match unbucketed {
        true => KeyBuilder::document_of_sharded_pk(coll.tenant, coll.id, pk),
        false => KeyBuilder::document_of_pk(coll.tenant, coll.id, pk),
    };
    let next = [key.as_bytes(), &[0]].concat();
    Ok(backend.scan(key.as_bytes(), &next, 1, concern)?.into_iter().next())
}

/// The document of an entry: the one it was read with, or [`indexed_document`].
fn fetched<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    (pk, doc): (&[u8], Option<(Vec<u8>, Vec<u8>)>),
    concern: ReadConcern,
) -> Result<Option<(Vec<u8>, Vec<u8>)>, CommandError> {
    match doc {
        Some(read) => Ok(Some(read)),
        None => indexed_document(backend, coll, pk, concern),
    }
}

/// Runs an intersection of index scans (`AND_HASH`): the primary keys of all but the last scan
/// held, the last one's entries streamed in its order, and each document found in all of them
/// read and filtered.
fn intersected<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    indexes: &[PlanIndex],
    read: &ReadPlan<'_>,
    (scans, filter): (&[IndexScan], Option<&Document>),
    mut ran: Ran,
) -> Result<Ran, CommandError> {
    let ext = super::engines::NodeExtensions;
    let ctx = teseladb_mql::MatchContext { collation: None, ext: &ext, vars: read.vars };
    let filter = match filter {
        Some(f) => {
            let bytes = super::query::raw_bytes(f)?;
            Some(
                teseladb_mql::Filter::parse(
                    RawDoc::from_bytes_with(&bytes, crate::msg::command_limits())?,
                    teseladb_mql::FilterUse::Query,
                )
                .map_err(from_mql)?,
            )
        }
        None => None,
    };
    let (last, held) = scans
        .split_last()
        .ok_or_else(|| CommandError::new(INTERNAL_ERROR, "an intersection without scans"))?;
    let mut sets: Vec<std::collections::BTreeSet<Vec<u8>>> = Vec::new();
    for s in held {
        let read_entries = entries(
            backend,
            (coll.tenant, coll.id),
            &indexes[s.index],
            s,
            (read.concern, false),
            None,
        )?
        .entries;
        let n = u64::try_from(read_entries.len()).unwrap_or(u64::MAX);
        let outside = read_entries.iter().filter(|e| e.is_none()).count();
        let past = u64::try_from(outside).unwrap_or(u64::MAX);
        let pks: std::collections::BTreeSet<Vec<u8>> =
            read_entries.into_iter().flatten().map(|e| e.pk).collect();
        ran.keys += n;
        ran.past += past;
        ran.child_keys.push(n);
        ran.child_past.push(past);
        sets.push(pks);
    }
    let (mut last_keys, mut last_past) = (0, 0);
    let last_index = &indexes[last.index];
    let scanned =
        entries(backend, (coll.tenant, coll.id), last_index, last, (read.concern, false), None)?;
    for entry in scanned.entries {
        ran.keys += 1;
        last_keys += 1;
        let Some(Entry { pk, doc, .. }) = entry else {
            ran.past += 1;
            last_past += 1;
            continue;
        };
        if !sets.iter().all(|set| set.contains(&pk)) {
            continue;
        }
        let Some((dk, bytes)) = fetched(backend, coll, (&pk, doc), read.concern)? else {
            continue;
        };
        // A document both scans hold comes out of the intersection one unit of work later.
        ran.docs += 1;
        ran.docs_at.push(ran.keys + 1);
        let keep = match &filter {
            Some(f) => f.matches(RawDoc::from_bytes(&bytes)?, &ctx).map_err(from_mql)?,
            None => true,
        };
        if keep {
            ran.found.push((dk, bytes));
            ran.given_at.push(ran.keys + 1);
        }
    }
    ran.child_keys.push(last_keys);
    ran.child_past.push(last_past);
    let advanced = u64::try_from(ran.found.len()).unwrap_or(u64::MAX);
    let sorts = !read.sort.is_empty() && !last.sorts;
    ran.trial =
        Trial { works: ran.keys + 1, advanced, fetches: true, sorts, intersects: true, eof: true };
    Ok(ran)
}

/// The entries of `index` a scan of `scan` examines, in its order: the primary key of each
/// entry within its intervals, and `None` for an entry outside them. A scan of one interval stops
/// at its end; a scan of several reads, after each interval, the next entry, which is outside
/// them when it lies before the next interval or after the last (measured on a `refrouter` 9.0.4:
/// `_id` in `[2, 5, 7]` over `_id` 1 to 8 examines 6 keys).
fn entries<B: Backend + ?Sized>(
    backend: &mut B,
    (tenant, collection): (u32, u32),
    index: &PlanIndex,
    scan: &IndexScan,
    (concern, decode): (ReadConcern, bool),
    most: Option<usize>,
) -> Result<Scanned, CommandError> {
    if checks_keys(scan) {
        return walked(backend, (tenant, collection), index, scan, concern, decode);
    }
    let ranges = key_ranges((tenant, collection), index, &scan.bounds)?;
    let points = (ranges.as_slice(), scan.backward);
    if let Some(read) = point_entries(backend, (tenant, collection), index, points, concern)? {
        return Ok(read);
    }
    let (first, past) = index_span((tenant, collection), index);
    // With `most`, a scan of one interval stops reading at that many entries: those after them
    // are past what the caller takes (a trial's budget), and reading them made each trial of a
    // range as long as the range (every unexpired session of a TTL index). A backward one reads
    // from the interval's end down (H-003).
    if let (Some(most), [r]) = (most, ranges.as_slice()) {
        let way = (Some(most), scan.backward);
        let read = range_entries(backend, (tenant, collection), index, r, (concern, decode), way)?;
        let more = read.len() >= most;
        return Ok(Scanned { entries: read.into_iter().map(Some).collect(), seeks: 1, more });
    }
    let mut lists = Vec::new();
    for r in &ranges {
        let mut pks = range_entries(
            backend,
            (tenant, collection),
            index,
            r,
            (concern, decode),
            (None, false),
        )?;
        if scan.backward {
            pks.reverse();
        }
        lists.push(pks);
    }
    let several = ranges.len() > 1;
    let order: Vec<usize> = match scan.backward {
        true => (0..ranges.len()).rev().collect(),
        false => (0..ranges.len()).collect(),
    };
    let mut out = Vec::new();
    for (step, &i) in order.iter().enumerate() {
        out.extend(lists[i].drain(..).map(Some));
        if !several {
            continue;
        }
        // The stretch between this interval and the next one the scan reads, or the index's end.
        let next = order.get(step + 1).copied();
        let end_of = |r: &KeyRange| r.end.clone().unwrap_or_else(|| past.clone());
        let (from, to) = match (scan.backward, next) {
            (false, Some(n)) => (end_of(&ranges[i]), ranges[n].start.clone()),
            (false, None) => (end_of(&ranges[i]), past.clone()),
            (true, Some(n)) => (end_of(&ranges[n]), ranges[i].start.clone()),
            (true, None) => (first.clone(), ranges[i].start.clone()),
        };
        if from < to && any_entry(backend, tenant, index, (&from, &to), concern)? {
            out.push(None);
        }
    }
    Ok(Scanned { entries: out, seeks: 1, more: false })
}

/// The entries a scan examined, in its order, and its seeks.
struct Scanned {
    entries: Vec<Option<Entry>>,
    seeks: u64,
    /// Whether the scan stopped at the entries it was asked for and has more after them.
    more: bool,
}

/// Whether a scan of `scan` checks each key against the intervals of a field after one with a
/// range, which its key ranges cannot hold.
fn checks_keys(scan: &IndexScan) -> bool {
    let all = vec![Bound::all()];
    let range = scan.bounds.iter().position(|(_, b)| !b.iter().all(Bound::is_point));
    range.is_some_and(|i| scan.bounds[i + 1..].iter().any(|(_, b)| *b != all))
}

/// The entries of a scan that checks its keys ([`checks_keys`]): every entry from its first
/// interval to its last, walked as RefDb's index bounds checker walks them
/// ([`teseladb_plan::checker`]), and the next entry of the index when the walk runs out of
/// them, which is outside the leading field's intervals.
fn walked<B: Backend + ?Sized>(
    backend: &mut B,
    (tenant, collection): (u32, u32),
    index: &PlanIndex,
    scan: &IndexScan,
    concern: ReadConcern,
    decode: bool,
) -> Result<Scanned, CommandError> {
    let ranges = key_ranges((tenant, collection), index, &scan.bounds)?;
    let (first, past) = index_span((tenant, collection), index);
    let (Some(low), Some(high)) = (ranges.first(), ranges.last()) else {
        return Ok(Scanned { entries: Vec::new(), seeks: 1, more: false });
    };
    let span = KeyRange { start: low.start.clone(), end: high.end.clone() };
    let mut read =
        range_entries(backend, (tenant, collection), index, &span, (concern, true), (None, false))?;
    if scan.backward {
        read.reverse();
    }
    let fields: Vec<FieldWalk> = scan
        .bounds
        .iter()
        .zip(&index.meta.key)
        .map(|((_, b), (_, dir))| FieldWalk {
            bounds: b.clone(),
            ascending: (*dir < 0) == scan.backward,
        })
        .collect();
    let values: Vec<Vec<Value>> =
        read.iter().map(|e| e.values.clone().unwrap_or_default()).collect();
    let w = walk(&fields, &values);
    let mut slots: Vec<Option<Entry>> = read.into_iter().map(Some).collect();
    let mut out = Vec::new();
    for v in &w.visits {
        match v {
            Visit::Within(i) => {
                let entry = slots.get_mut(*i).and_then(Option::take).map(|e| match decode {
                    true => e,
                    false => Entry { key: None, values: None, ..e },
                });
                out.push(entry);
            }
            Visit::Outside(_) => out.push(None),
        }
    }
    if !w.done {
        let end = span.end.clone().unwrap_or_else(|| past.clone());
        let (from, to) = match scan.backward {
            false => (end, past),
            true => (first, span.start.clone()),
        };
        if from < to && any_entry(backend, tenant, index, (&from, &to), concern)? {
            out.push(None);
        }
    }
    Ok(Scanned { entries: out, seeks: w.seeks, more: false })
}

/// Whether `index` has an entry after the last interval of `scan`, a forward scan.
pub(crate) fn key_after<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    index: &PlanIndex,
    scan: &IndexScan,
) -> Result<bool, CommandError> {
    // A `distinct` has no sort: its scan goes forward, to the end of the index.
    let ranges = key_ranges((coll.tenant, coll.id), index, &scan.bounds)?;
    let (_, past) = index_span((coll.tenant, coll.id), index);
    let from = ranges.last().and_then(|r| r.end.clone()).unwrap_or_else(|| past.clone());
    Ok(from < past && any_entry(backend, coll.tenant, index, (&from, &past), ReadConcern::Local)?)
}

/// An entry an index scan reads: the document's primary key and, when the read wants it, the
/// entry's key as `{$meta: "indexKey"}` gives it.
pub(crate) struct Entry {
    pk: Vec<u8>,
    key: Option<Document>,
    /// The entry's values as stored, when decoded.
    values: Option<Vec<Value>>,
    /// The document's key and bytes, when the entry was read with them: an entry of the index of
    /// `_id` is its document.
    doc: Option<(Vec<u8>, Vec<u8>)>,
}

/// The key of a document in the index of `_id`: `{_id: value}`.
fn id_key(doc: &[u8]) -> Result<Option<Document>, CommandError> {
    let doc = RawDoc::from_bytes(doc)?;
    Ok(doc.get("_id").map(|id| Document::new().with("_id", Value::from(id))))
}

impl PlanIndex {
    /// The values of an entry of this index whose encoded values are `values` and whose stored
    /// value is `stored` (type bits, after the primary key in a unique index); `None` when they
    /// do not decode (a collation's).
    fn values_of(&self, values: &[u8], stored: &[u8]) -> Option<Vec<Value>> {
        let bits = match self.catalog.unique {
            true => teseladb_keys::space::split_unique_value(stored).ok()?.1,
            false => teseladb_keys::TypeBits::from_bytes(stored.to_vec()),
        };
        teseladb_keys::decode(values, &bits, &self.dirs).ok()
    }

    /// The key of an entry of values `values`, each field with its value as RefDb gives
    /// `{$meta: "indexKey"}`.
    fn key_from(&self, values: &[Value]) -> Document {
        let names = self.pattern.iter().map(|(k, _)| k);
        // The key of an empty array is `undefined`; RefDb gives it back as `null` (measured).
        let shown = values.iter().map(|v| match v {
            Value::Undefined => Value::Null,
            other => other.clone(),
        });
        names.zip(shown).fold(Document::new(), |d, (k, v)| d.with(k, v))
    }
}

/// The first key of `index` and the first key after it.
fn index_span((tenant, collection): (u32, u32), index: &PlanIndex) -> (Vec<u8>, Vec<u8>) {
    let id = index.entry.id;
    (
        KeyBuilder::index_prefix(tenant, collection, id).as_bytes().to_vec(),
        KeyBuilder::index_prefix(tenant, collection, id + 1).as_bytes().to_vec(),
    )
}

/// The primary keys of the entries of `index` within key range `r`, in key order.
fn range_entries<B: Backend + ?Sized>(
    backend: &mut B,
    (tenant, collection): (u32, u32),
    index: &PlanIndex,
    r: &KeyRange,
    (concern, decode): (ReadConcern, bool),
    (most, backward): (Option<usize>, bool),
) -> Result<Vec<Entry>, CommandError> {
    let (_, past) = index_span((tenant, collection), index);
    let end = r.end.clone().unwrap_or(past);
    if index.ordered {
        let span = (r.start.as_slice(), end.as_slice());
        return ordered_entries(backend, (tenant, collection), span, concern, (most, backward));
    }
    if index.id_index {
        if let Some(one) = id_point(backend, (tenant, collection), (&r.start, &end), concern)? {
            return Ok(one);
        }
        let span = (r.start.as_slice(), end.as_slice());
        let read = id_entries(backend, (tenant, collection), span, concern, (most, backward))?;
        return Ok(read
            .into_iter()
            .map(|(pk, doc)| Entry { pk, key: None, values: None, doc: Some(doc) })
            .collect());
    }
    let head = KeyBuilder::index_prefix(tenant, collection, index.entry.id).as_bytes().len();
    let malformed = || CommandError::new(INTERNAL_ERROR, "an index entry that does not decode");
    let mut pks = Vec::new();
    // Forward from `from`; backward below `to` (H-003).
    let (mut from, mut to) = (r.start.clone(), end.clone());
    loop {
        let chunk = most.map_or(KEY_CHUNK, |m| m.saturating_sub(pks.len()).clamp(1, KEY_CHUNK));
        let rows = match backward {
            true => backend.scan_back(&r.start, &to, chunk, concern)?,
            false => backend.scan(&from, &end, chunk, concern)?,
        };
        let short = rows.len() < chunk;
        for (key, value) in &rows {
            let rest = key.get(head..).ok_or_else(malformed)?;
            let cut = head + values_len(rest, &index.dirs).map_err(|_| malformed())?;
            let values = match decode {
                true => index.values_of(&key[head..cut], value),
                false => None,
            };
            let found = values.as_ref().map(|v| index.key_from(v));
            // A unique index keeps the primary key in the entry's value, not in its key.
            let pk = match index.catalog.unique {
                true => teseladb_keys::space::split_unique_value(value).map_err(|_| malformed())?.0,
                false => &key[cut..],
            };
            pks.push(Entry { pk: pk.to_vec(), key: found, values, doc: None });
        }
        match rows.last() {
            _ if most.is_some_and(|m| pks.len() >= m) => break,
            Some((k, _)) if !short && backward => to.clone_from(k),
            Some((k, _)) if !short => from = [k.as_slice(), &[0]].concat(),
            _ => break,
        }
    }
    Ok(pks)
}

/// Whether `index` has an entry in `[from, to)`.
fn any_entry<B: Backend + ?Sized>(
    backend: &mut B,
    tenant: u32,
    index: &PlanIndex,
    (from, to): (&[u8], &[u8]),
    concern: ReadConcern,
) -> Result<bool, CommandError> {
    if !index.id_index {
        return Ok(!backend.scan(from, to, 1, concern)?.is_empty());
    }
    let spans = gap_spans(tenant, index.entry.collection, (from, to));
    Ok(backend.scan_spans(&spans, concern)?.iter().any(|rows| !rows.is_empty()))
}

/// The spans that hold the keys of the index of `_id` in `[from, to)`: one in each bucket of the
/// documents, of one key, which a read takes in one trip.
fn gap_spans(tenant: u32, collection: u32, (from, to): (&[u8], &[u8])) -> Vec<Span> {
    let docs = KeyBuilder::index_prefix(tenant, collection, DOCUMENTS);
    (0..DOCUMENT_BUCKETS)
        .map(|bucket| {
            let (start, end) = in_bucket(docs.as_bytes(), bucket, (from, to));
            Span { start, end, limit: 1, backward: false }
        })
        .collect()
}

/// The pk of the one document of the index of `_id` in `[from, to)`, when that range is one
/// value (see [`id_point`]).
fn point_pk<'a>(docs: &[u8], (from, to): (&'a [u8], &[u8])) -> Option<&'a [u8]> {
    let pk = from.strip_prefix(docs)?;
    (next_prefix(from).as_deref() == Some(to)).then_some(pk)
}

/// The entries of a scan of the index of `_id` whose intervals are all single values, with the
/// stretch after each as [`entries`] reads it; `None` for another scan.
///
/// The documents go by the hash of their `_id`, so whether a stretch holds an entry is asked of
/// the 16 buckets. One read of each bucket covers the stretches between the values, up to
/// [`stretch_limit`] entries, and the stretch after the last value, which may run to the end of
/// the collection, takes one entry of each bucket (010 T054). They go in one trip with the
/// values. A bucket that holds more entries than its read took leaves the stretches past its last
/// entry open, and those take one entry of that bucket each in a second trip.
fn point_entries<B: Backend + ?Sized>(
    backend: &mut B,
    (tenant, collection): (u32, u32),
    index: &PlanIndex,
    (ranges, backward): (&[KeyRange], bool),
    concern: ReadConcern,
) -> Result<Option<Scanned>, CommandError> {
    // In `_id` order, each value's interval is read as one, without buckets.
    if !index.id_index || index.ordered || ranges.len() < 2 {
        return Ok(None);
    }
    let docs = KeyBuilder::index_prefix(tenant, collection, DOCUMENTS);
    let (first, past) = index_span((tenant, collection), index);
    let end_of = |r: &KeyRange| r.end.clone().unwrap_or_else(|| past.clone());
    let Some(pks) = ranges
        .iter()
        .map(|r| point_pk(docs.as_bytes(), (&r.start, &end_of(r))).map(<[u8]>::to_vec))
        .collect::<Option<Vec<Vec<u8>>>>()
    else {
        return Ok(None);
    };
    let order: Vec<usize> = match backward {
        true => (0..ranges.len()).rev().collect(),
        false => (0..ranges.len()).collect(),
    };
    // The stretch after each value in the scan's order, as `entries` reads it, in key order.
    let stretches: Vec<Option<(Vec<u8>, Vec<u8>)>> = order
        .iter()
        .enumerate()
        .map(|(step, &i)| {
            let (from, to) = match (backward, order.get(step + 1).copied()) {
                (false, Some(n)) => (end_of(&ranges[i]), ranges[n].start.clone()),
                (false, None) => (end_of(&ranges[i]), past.clone()),
                (true, Some(n)) => (end_of(&ranges[n]), ranges[i].start.clone()),
                (true, None) => (first.clone(), ranges[i].start.clone()),
            };
            (from < to).then_some((from, to))
        })
        .collect();
    let mut spans: Vec<Span> = pks
        .iter()
        .map(|pk| {
            let key = KeyBuilder::document_of_pk(tenant, collection, pk).as_bytes().to_vec();
            let end = [key.as_slice(), &[0]].concat();
            Span { start: key, end, limit: 1, backward: false }
        })
        .collect();
    let last = order.len() - 1;
    let outer_at = spans.len();
    if let Some((from, to)) = &stretches[last] {
        spans.extend(gap_spans(tenant, collection, (from, to)));
    }
    let outer_end = spans.len();
    let between = Between::new(&stretches[..last]);
    let limit = stretch_limit(ranges.len());
    if let Some((lo, hi)) = between.bounds() {
        spans.extend((0..DOCUMENT_BUCKETS).map(|bucket| {
            let (start, end) = in_bucket(docs.as_bytes(), bucket, (lo, hi));
            Span { start, end, limit, backward: false }
        }));
    }
    let rows = backend.scan_spans(&spans, concern)?;
    let mut held = vec![false; order.len()];
    held[last] = rows.get(outer_at..outer_end).is_some_and(|r| r.iter().any(|r| !r.is_empty()));
    let head = docs.as_bytes().len();
    let mut open = Vec::new();
    for (bucket, read) in (0..DOCUMENT_BUCKETS).zip(rows.iter().skip(outer_end)) {
        let keys: Vec<Vec<u8>> = read
            .iter()
            .map(|(k, _)| [docs.as_bytes(), k.get(head + 1..).unwrap_or_default()].concat())
            .collect();
        for k in &keys {
            if let Some(step) = between.holding(k) {
                held[step] = true;
            }
        }
        // A read that took all it could leaves open the stretches past its last entry.
        if let (true, Some(reached)) = (read.len() >= limit, keys.last()) {
            open.extend(between.past(reached).map(|step| (step, bucket)));
        }
    }
    open.retain(|(step, _)| !held[*step]);
    if !open.is_empty() {
        let probes: Vec<Span> = open
            .iter()
            .filter_map(|&(step, bucket)| {
                let (from, to) = stretches[step].as_ref()?;
                let (start, end) = in_bucket(docs.as_bytes(), bucket, (from, to));
                Some(Span { start, end, limit: 1, backward: false })
            })
            .collect();
        let found = backend.scan_spans(&probes, concern)?;
        for (&(step, _), r) in open.iter().zip(&found) {
            held[step] |= !r.is_empty();
        }
    }
    let mut out = Vec::new();
    for (step, &i) in order.iter().enumerate() {
        let found = rows.get(i).into_iter().flatten().map(|(k, v)| Entry {
            pk: pks[i].clone(),
            key: None,
            values: None,
            doc: Some((k.clone(), v.clone())),
        });
        out.extend(found.map(Some));
        if held[step] {
            out.push(None);
        }
    }
    Ok(Some(Scanned { entries: out, seeks: 1, more: false }))
}

/// The most entries one read of a bucket takes for the stretches between `values` single values:
/// with the documents dense around the values, a bucket holds a few between each two, and with
/// them sparse the read stops there and the open stretches take one entry each.
fn stretch_limit(values: usize) -> usize {
    2 * values + 1
}

/// The stretches between the values of a scan, by the step of the value they follow, in key
/// order: they do not overlap, since the values are distinct and sorted.
struct Between<'a> {
    /// `(from, to, step)` of each stretch that is not empty, sorted by `from`.
    sorted: Vec<(&'a [u8], &'a [u8], usize)>,
}

impl<'a> Between<'a> {
    fn new(stretches: &'a [Option<(Vec<u8>, Vec<u8>)>]) -> Between<'a> {
        let mut sorted: Vec<_> = stretches
            .iter()
            .enumerate()
            .filter_map(|(step, s)| s.as_ref().map(|(f, t)| (f.as_slice(), t.as_slice(), step)))
            .collect();
        sorted.sort_unstable();
        Between { sorted }
    }

    /// The keys from the start of the first stretch to the end of the last.
    fn bounds(&self) -> Option<(&'a [u8], &'a [u8])> {
        Some((self.sorted.first()?.0, self.sorted.last()?.1))
    }

    /// The step of the stretch that holds `key`, if any.
    fn holding(&self, key: &[u8]) -> Option<usize> {
        let at = self.sorted.partition_point(|(from, _, _)| *from <= key);
        let (_, to, step) = self.sorted.get(at.checked_sub(1)?)?;
        (key < *to).then_some(*step)
    }

    /// The steps of the stretches with keys past `key`.
    fn past(&self, key: &[u8]) -> impl Iterator<Item = usize> + '_ {
        let key = key.to_vec();
        self.sorted.iter().filter(move |(_, to, _)| *to > key.as_slice()).map(|(_, _, s)| *s)
    }
}

/// Key range `[from, to)` of the index of `_id`, whose keys follow the documents' prefix
/// `docs`, as the keys of the documents of `bucket`; a key past that prefix ends the bucket.
fn in_bucket(docs: &[u8], bucket: u8, (from, to): (&[u8], &[u8])) -> (Vec<u8>, Vec<u8>) {
    let at = |k: &[u8]| -> Vec<u8> {
        match k.strip_prefix(docs) {
            Some(rest) => [docs, &[bucket], rest].concat(),
            // The span of the index ends at the next index's prefix, after every bucket.
            None => [docs, &[bucket + 1]].concat(),
        }
    };
    (at(from), at(to))
}

/// The entry of the index of `_id` for one value, with its document, read in one bucket; `None`
/// when `[from, to)` is not one value. Such a range holds every key that starts with the value's
/// encoding, and since encodings are self-delimiting that is the value's own document alone, in
/// the bucket of its hash.
fn id_point<B: Backend + ?Sized>(
    backend: &mut B,
    (tenant, collection): (u32, u32),
    (from, to): (&[u8], &[u8]),
    concern: ReadConcern,
) -> Result<Option<Vec<Entry>>, CommandError> {
    let docs = KeyBuilder::index_prefix(tenant, collection, DOCUMENTS);
    let Some(pk) = from.strip_prefix(docs.as_bytes()) else { return Ok(None) };
    if next_prefix(from).as_deref() != Some(to) {
        return Ok(None);
    }
    let key = KeyBuilder::document_of_pk(tenant, collection, pk);
    // A point read, as RefDb's `IDHACK` plan reads one `_id` (H-013): the node reads the key
    // with its filters instead of opening a scan.
    let key = key.as_bytes().to_vec();
    let found = backend.points(std::slice::from_ref(&key), concern)?;
    Ok(Some(
        found
            .into_iter()
            .flatten()
            .map(|v| Entry {
                pk: pk.to_vec(),
                key: None,
                values: None,
                doc: Some((key.clone(), v)),
            })
            .collect(),
    ))
}

/// The documents of `[from, to)` of a collection kept in `_id` order (015 FR-009, H-027): its
/// document keys are the keys of its index of `_id`, so the interval is read as it is, from its
/// end down when `backward`, a chunk at a time and no further than `most` documents.
fn ordered_entries<B: Backend + ?Sized>(
    backend: &mut B,
    (tenant, collection): (u32, u32),
    (from, to): (&[u8], &[u8]),
    concern: ReadConcern,
    (most, backward): (Option<usize>, bool),
) -> Result<Vec<Entry>, CommandError> {
    let head = KeyBuilder::index_prefix(tenant, collection, DOCUMENTS).as_bytes().len();
    let (mut lo, mut hi) = (from.to_vec(), to.to_vec());
    let mut out = Vec::new();
    loop {
        let chunk = most.map_or(KEY_CHUNK, |m| m.saturating_sub(out.len()).clamp(1, KEY_CHUNK));
        let rows = match backward {
            true => backend.scan_back(&lo, &hi, chunk, concern)?,
            false => backend.scan(&lo, &hi, chunk, concern)?,
        };
        let short = rows.len() < chunk;
        let last = rows.last().map(|(k, _)| k.clone());
        for (key, value) in rows {
            let pk = key.get(head..).unwrap_or_default().to_vec();
            out.push(Entry { pk, key: None, values: None, doc: Some((key, value)) });
        }
        match last {
            _ if most.is_some_and(|m| out.len() >= m) => break,
            Some(k) if !short && backward => hi = k,
            Some(k) if !short => lo = [k.as_slice(), &[0]].concat(),
            _ => break,
        }
    }
    Ok(out)
}

/// The documents of the index of `_id` in `[from, to)` with their primary keys, in key order: the
/// first `most` when it says how many the caller takes. The documents go by the hash of their
/// `_id` into [`DOCUMENT_BUCKETS`] buckets, each in key order, so they are merged as sorted lists:
/// a few entries of each bucket are asked together, the lowest goes out each time, and only a
/// bucket that runs out is read again. To give `n` documents the read takes little more than `n`
/// (H-009b of the native tests: YCSB-E's scans of up to 100 read some 5.400 rows). An entry of
/// this index is its document, which the caller keeps instead of reading it again.
fn id_entries<B: Backend + ?Sized>(
    backend: &mut B,
    (tenant, collection): (u32, u32),
    (from, to): (&[u8], &[u8]),
    concern: ReadConcern,
    (most, backward): (Option<usize>, bool),
) -> Result<IdRows, CommandError> {
    let docs = KeyBuilder::index_prefix(tenant, collection, DOCUMENTS);
    let head = docs.as_bytes().len();
    let buckets = usize::from(DOCUMENT_BUCKETS);
    // Each bucket holds about a sixteenth of what the read gives: two more cover an uneven share.
    let per = most.map_or(KEY_CHUNK, |m| (m / buckets + 2).clamp(1, KEY_CHUNK));
    let spans: Vec<Span> = (0..DOCUMENT_BUCKETS)
        .map(|bucket| {
            let (start, end) = in_bucket(docs.as_bytes(), bucket, (from, to));
            Span { start, end, limit: per, backward }
        })
        .collect();
    let reads = backend.scan_spans(&spans, concern)?;
    let mut lanes: Vec<Lane> =
        spans.into_iter().zip(reads).map(|(span, rows)| Lane::new(span, rows, per)).collect();
    let pk_of = |key: &[u8]| key.get(head + 1..).unwrap_or_default().to_vec();
    let mut found: IdRows = Vec::new();
    while most.is_none_or(|m| found.len() < m) {
        for lane in &mut lanes {
            lane.refill(backend, per, concern)?;
        }
        // The next key in the read's direction: the lowest, or backward the highest (H-003).
        let heads = lanes
            .iter()
            .enumerate()
            .filter_map(|(i, l)| l.rows.front().map(|(k, _)| (i, pk_of(k))));
        let next = match backward {
            true => heads.max_by(|a, b| a.1.cmp(&b.1)),
            false => heads.min_by(|a, b| a.1.cmp(&b.1)),
        };
        let Some((i, pk)) = next else { break };
        let row = lanes[i].rows.pop_front().expect("the lane's first row");
        found.push((pk, row));
    }
    Ok(found)
}

/// One bucket of a read of the index of `_id`: the rows read and not given yet, and where its read
/// goes on when it may hold more.
struct Lane {
    rows: std::collections::VecDeque<(Vec<u8>, Vec<u8>)>,
    /// Where the read goes on: forward, its first key; backward, the key it stops below.
    next: Option<Vec<u8>>,
    span: Span,
}

impl Lane {
    fn new(span: Span, rows: Vec<(Vec<u8>, Vec<u8>)>, asked: usize) -> Lane {
        let next = Self::after(&rows, asked, span.backward);
        Lane { rows: rows.into(), next, span }
    }

    /// Where the read goes on after `rows`, when they filled what was asked and the bucket may
    /// hold more: the key after the last one, or backward the last one itself (excluded).
    fn after(rows: &[(Vec<u8>, Vec<u8>)], asked: usize, backward: bool) -> Option<Vec<u8>> {
        match rows.last() {
            Some((k, _)) if rows.len() >= asked && backward => Some(k.clone()),
            Some((k, _)) if rows.len() >= asked => Some([k.as_slice(), &[0]].concat()),
            _ => None,
        }
    }

    /// Reads on once its rows ran out and the bucket may hold more.
    fn refill<B: Backend + ?Sized>(
        &mut self,
        backend: &mut B,
        asked: usize,
        concern: ReadConcern,
    ) -> Result<(), CommandError> {
        if !self.rows.is_empty() {
            return Ok(());
        }
        let Some(next) = self.next.take() else { return Ok(()) };
        let rows = match self.span.backward {
            true => backend.scan_back(&self.span.start, &next, asked, concern)?,
            false => backend.scan(&next, &self.span.end, asked, concern)?,
        };
        self.next = Self::after(&rows, asked, self.span.backward);
        self.rows.extend(rows);
        Ok(())
    }
}

/// The documents of a read of the index of `_id`: each primary key with its document's key and
/// bytes.
type IdRows = Vec<(Vec<u8>, (Vec<u8>, Vec<u8>))>;

/// The key intervals of `index` for `bounds`.
fn key_ranges(
    (tenant, collection): (u32, u32),
    index: &PlanIndex,
    bounds: &[(String, Vec<Bound>)],
) -> Result<Vec<KeyRange>, CommandError> {
    let mut holder = Document::new();
    for (i, (_, list)) in bounds.iter().enumerate() {
        for (j, b) in list.iter().enumerate() {
            holder.push(format!("{i}_{j}_l"), b.low.clone());
            holder.push(format!("{i}_{j}_h"), b.high.clone());
        }
    }
    let bytes = holder.to_bytes()?;
    let raw = RawDoc::from_bytes(&bytes)?;
    let mut fields: Vec<Vec<Interval<'_>>> = Vec::new();
    for (i, (_, list)) in bounds.iter().enumerate() {
        let mut intervals = Vec::new();
        for (j, b) in list.iter().enumerate() {
            let low = raw.get(&format!("{i}_{j}_l")).unwrap_or(RawValue::MinKey);
            let high = raw.get(&format!("{i}_{j}_h")).unwrap_or(RawValue::MaxKey);
            intervals.push(Interval {
                low,
                low_inclusive: b.low_inclusive,
                high,
                high_inclusive: b.high_inclusive,
            });
        }
        fields.push(intervals);
    }
    let catalog = IndexCatalog { tenant, collection, indexes: vec![index.catalog.clone()] };
    Ok(catalog.bounds(&index.catalog, &FieldBounds { fields }, None))
}

/// The keys of the documents a write of `coll` takes through the index plan that wins, in its
/// order (the first alone unless `multi`); `None` when the collection scan wins, no index
/// serves or the statement has a collation (the caller scans).
pub(crate) fn planned_keys<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    m: &super::crud::Modify,
) -> Result<Option<Vec<Vec<u8>>>, CommandError> {
    if m.collation.is_some() {
        return Ok(None);
    }
    let indexes = plan_indexes(backend, coll)?;
    if indexes.is_empty() {
        return Ok(None);
    }
    let parsed = parsed_filter(&m.filter, m.vars.as_ref())?;
    let read = ReadPlan {
        filter: &m.filter,
        parsed: &parsed,
        sort: Vec::new(),
        // Plans compete without the limit of one (measured).
        limit: None,
        hint: super::hints::position(m.hint.as_ref(), &indexes)?,
        vars: m.vars.as_ref(),
        concern: ReadConcern::Local,
        intersect: false,
        index_keys: false,
        key_only: false,
    };
    let take = if m.multi { usize::MAX } else { 1 };
    let started = backend.now_micros();
    let chosen = choose(backend, coll, &indexes, &read)?;
    let planning = chosen
        .as_ref()
        .map_or_else(|| backend.now_micros().saturating_sub(started), |c| c.planning_micros);
    let mut noted =
        Document::new().with(PLANNING_TIME, i64::try_from(planning).unwrap_or(i64::MAX));
    if let Some(c) = &chosen {
        let names: Vec<Value> =
            scanned(&indexes, &c.winner.access).into_iter().map(Value::from).collect();
        noted.push(USED_INDEXES, Document::new().with("names", names));
    }
    backend.note_plan(noted);
    Ok(chosen.map(|c| c.winner.found.into_iter().take(take).map(|(k, _)| k).collect()))
}

/// Filter `filter` as RefDb writes it back after parsing it, each `$expr` optimised.
pub(crate) fn parsed_filter(
    filter: &Document,
    vars: Option<&Document>,
) -> Result<Document, CommandError> {
    let filter = teseladb_exec::expr::optimize::optimize_filter(filter, vars).map_err(from_mql)?;
    let bytes = super::query::raw_bytes(&filter)?;
    let raw = RawDoc::from_bytes_with(&bytes, crate::msg::command_limits())?;
    let f = teseladb_mql::Filter::parse(raw, teseladb_mql::FilterUse::Query).map_err(from_mql)?;
    Ok(f.parsed_query())
}

/// Where a read leaves what its plans' trial learned (010 FR-002): the node, the collection
/// and `createdFromQuery` with the read's shape.
pub(crate) struct CacheNote<'a> {
    /// The node.
    pub shared: &'a super::Shared,
    /// The collection.
    pub ns: &'a teseladb_meta::catalog::Namespace,
    /// `createdFromQuery`.
    pub from: Document,
    /// The read's shape.
    pub shape: String,
}

/// The documents a read of `coll` finds through the index plan that wins, in its order; `None`
/// when the collection scan wins or no index serves (the caller scans). When plans competed, the
/// node's plan cache learns of it (`note`).
pub(crate) fn planned_documents<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    read: &ReadPlan<'_>,
    note: CacheNote<'_>,
) -> Result<Option<Vec<Vec<u8>>>, CommandError> {
    Ok(planned_keyed(backend, coll, read, note)?.map(|(docs, _)| docs))
}

/// [`planned_documents`] with the index key each document was found by, when `read` wants them
/// ([`ReadPlan::index_keys`]; empty otherwise).
pub(crate) fn planned_keyed<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    read: &ReadPlan<'_>,
    note: CacheNote<'_>,
) -> Result<Option<Planned>, CommandError> {
    let indexes = plan_indexes(backend, coll)?;
    // The hashes of the read's shape, as `$planCacheStats` shows them, for the profiler.
    let hash = super::plan_hash::shape_hash(&note.shape);
    let ns = super::names::full(note.ns);
    let digits = super::plan_hash::indexability(read.parsed, &key_indexes(&indexes), None);
    let cache_key = super::plan_hash::cache_key_hash(&note.shape, &digits);
    let hashes = Document::new()
        .with("planCacheShapeHash", hash.clone())
        .with("queryHash", hash)
        .with("planCacheKey", cache_key.clone());
    let report = |plan: Option<Document>, planning: u64| {
        let mut d = hashes.clone();
        d.push("filter", read.parsed.clone());
        for (k, v) in plan.unwrap_or_default().iter() {
            d.push(k, v.clone());
        }
        d.push(PLANNING_TIME, i64::try_from(planning).unwrap_or(i64::MAX));
        d
    };
    let started = backend.now_micros();
    if indexes.is_empty() {
        let planning = backend.now_micros().saturating_sub(started);
        backend.note_plan(report(None, planning));
        return Ok(None);
    }
    // A hinted read plans nothing and never takes the cached plan (measured).
    let catalog = super::plan_cache::catalog_signature(coll, &indexes);
    let cached = match read.hint {
        Some(_) => None,
        None => super::lock(note.shared).plan_cache.cached(&ns, &note.shape, &catalog),
    };
    // The entry keeps the plan of the read that learned it, with that read's values: the plan
    // runs with the bounds and residual this read gives it, or the read plans from scratch.
    let outcome = match cached
        .and_then(|(access, works)| Some((rebound(&access, &indexes, read)?, works)))
    {
        Some((access, works)) => Some(cached_run(backend, coll, &indexes, read, access, works)?),
        None => None,
    };
    let (chosen, from_cache, replanned) = match outcome {
        Some(Cached::Kept(c)) => (Some(*c), true, None),
        Some(Cached::Replan(expected, reached)) => {
            // The cached plan worked too much: its entry goes, the plans compete again and
            // what they learn is not kept (measured).
            super::lock(note.shared).plan_cache.clear(&ns, Some(&note.shape));
            let reason = format!(
                "cached plan was less efficient than expected: expected trial execution to \
                 take {expected} works but it took at least {reached} works"
            );
            (choose(backend, coll, &indexes, read)?, false, Some(reason))
        }
        None => (choose(backend, coll, &indexes, read)?, false, None),
    };
    let Some(chosen) = chosen else {
        let planning = backend.now_micros().saturating_sub(started);
        backend.note_plan(report(None, planning));
        return Ok(None);
    };
    let plan = super::explain::plan_report(note.ns, read.parsed, &indexes, &chosen.winner)
        .with(MULTI_PLANNED, !chosen.others.is_empty())
        .with(FROM_CACHE, from_cache)
        .with(REPLANNED, replanned.clone().map_or(Value::Null, Value::from))
        .with(SORT_SERVED, super::explain::sorted(&chosen.winner));
    backend.note_plan(report(Some(plan), chosen.planning_micros));
    let now = i64::try_from(backend.wall_ms()).unwrap_or(i64::MAX);
    {
        let mut g = super::lock(note.shared);
        for name in scanned(&indexes, &chosen.winner.access) {
            g.index_usage.used(&ns, &name, now);
        }
    }
    // A replanned read leaves no entry (measured).
    if replanned.is_none()
        && let Some(mut learned) = super::explain::learned(
            note.ns,
            read.parsed,
            &indexes,
            &chosen,
            (note.from, note.shape),
        )
    {
        learned.catalog = catalog;
        learned.key = cache_key;
        let now = i64::try_from(backend.now_ms()).unwrap_or(i64::MAX);
        super::lock(note.shared).plan_cache.learn(&ns, learned, now);
    }
    let keys = chosen.winner.index_keys;
    Ok(Some((chosen.winner.found.into_iter().map(|(_, d)| d).collect(), keys)))
}

/// The plan of `read` with the shape of `cached`: the candidate of this read that scans the same
/// indexes the same way, with this read's bounds and residual; `None` when none does.
fn rebound(cached: &Access, indexes: &[PlanIndex], read: &ReadPlan<'_>) -> Option<Access> {
    let metas: Vec<IndexMeta> = indexes.iter().map(|i| i.meta.clone()).collect();
    let request = Request {
        parsed: Some(read.parsed),
        sort: &read.sort,
        hint: read.hint,
        intersect: read.intersect,
    };
    candidates(&request, &metas).into_iter().find(|a| same_shape(a, cached))
}

/// Whether two plans scan the same indexes the same way, whatever their values.
fn same_shape(a: &Access, b: &Access) -> bool {
    let scans = |x: &[IndexScan], y: &[IndexScan]| {
        x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same_scan(x, y))
    };
    match (a, b) {
        (Access::Collscan, Access::Collscan) => true,
        (Access::Index(x), Access::Index(y)) => same_scan(x, y),
        (Access::Or(x), Access::Or(y)) => scans(x, y),
        (Access::AndHash { scans: x, .. }, Access::AndHash { scans: y, .. }) => scans(x, y),
        _ => false,
    }
}

fn same_scan(a: &IndexScan, b: &IndexScan) -> bool {
    a.index == b.index
        && a.backward == b.backward
        && a.sorts == b.sorts
        && a.assigned == b.assigned
        && a.preds == b.preds
        && a.bounds.iter().map(|(f, _)| f).eq(b.bounds.iter().map(|(f, _)| f))
}

/// What became of the plan the cache holds for a read.
enum Cached {
    /// It gave a batch or ended within [`EVICTION_RATIO`] times its entry's works: its run.
    Kept(Box<Chosen>),
    /// It did not: the works its entry expected and those it reached, after which RefDb plans
    /// again (`replanReason`).
    Replan(u64, u64),
}

/// The run of `access`, the plan the cache holds for `read`, whose entry's trial worked `works`,
/// as RefDb's cached plan runs: a trial of [`EVICTION_RATIO`] times those works that keeps the
/// plan when it gives a batch or ends.
fn cached_run<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    indexes: &[PlanIndex],
    read: &ReadPlan<'_>,
    access: Access,
    works: u64,
) -> Result<Cached, CommandError> {
    let started = backend.now_micros();
    let limit = works.saturating_mul(EVICTION_RATIO).max(1);
    let trial = run(backend, coll, indexes, read, access.clone(), Some(limit))?;
    let winner = if trial.trial.eof {
        trial
    } else if trial.trial.advanced >= trial_results(read) {
        run(backend, coll, indexes, read, access, None)?
    } else {
        return Ok(Cached::Replan(works, limit));
    };
    let planning_micros = backend.now_micros().saturating_sub(started);
    Ok(Cached::Kept(Box::new(Chosen { winner, others: Vec::new(), planning_micros })))
}

/// The documents a plan found, in order, and the index key of each when the read wants them.
pub(crate) type Planned = (Vec<Vec<u8>>, Vec<Option<Document>>);

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn scan(index: usize, field: &str, at: i32) -> IndexScan {
        let v = Value::Int32(at);
        let bound = Bound { low: v.clone(), low_inclusive: true, high: v, high_inclusive: true };
        IndexScan {
            index,
            bounds: vec![(field.to_owned(), vec![bound])],
            backward: false,
            residual: None,
            sorts: false,
            assigned: 1,
            preds: vec![0],
        }
    }

    #[test]
    fn the_shape_of_a_plan_is_its_indexes_and_their_use_not_its_values() {
        assert!(same_shape(&Access::Collscan, &Access::Collscan));
        assert!(same_shape(&Access::Index(scan(1, "a", 1)), &Access::Index(scan(1, "a", 2))));
        assert!(!same_shape(&Access::Index(scan(1, "a", 1)), &Access::Index(scan(2, "a", 1))));
        assert!(!same_shape(&Access::Index(scan(1, "a", 1)), &Access::Index(scan(1, "b", 1))));
        let backward = IndexScan { backward: true, ..scan(1, "a", 1) };
        assert!(!same_shape(&Access::Index(backward), &Access::Index(scan(1, "a", 1))));
        let or = |x, y| Access::Or(vec![scan(1, "a", x), scan(2, "b", y)]);
        assert!(same_shape(&or(1, 2), &or(3, 4)));
        assert!(!same_shape(&or(1, 2), &Access::Or(vec![scan(1, "a", 1)])));
        let hash =
            |x| Access::AndHash { scans: vec![scan(1, "a", x), scan(2, "b", x)], filter: None };
        assert!(same_shape(&hash(1), &hash(2)));
        assert!(!same_shape(&hash(1), &or(1, 1)));
        assert!(!same_shape(&Access::Collscan, &Access::Index(scan(1, "a", 1))));
    }
}
