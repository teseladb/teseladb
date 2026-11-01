//! The planner at work on a node (010 FR-001, T019): the indexes of a collection as the planner
//! sees them, the plans of a read, their trial on the node's own documents and the run of the
//! plan that wins, as RefDb's classic engine does (measured on a `refrouter` 9.0.4, 01-11-2026).
//! An index scan reads the entries within the plan's intervals in key order (backwards when the
//! plan reads the index the other way), takes each document once and keeps those the rest of
//! the filter matches.

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_index::planner_api::{CatalogIndex, IndexCatalog, IndexState};
use teseladb_index::spec::catalog_index;
use teseladb_keys::{Direction, FieldBounds, Hlc, Interval, KeyBuilder, KeyRange, values_len};
use teseladb_meta::catalog::CollectionEntry;
use teseladb_meta::collection_types::CollectionKind;
use teseladb_meta::indexes::{IndexEntry, IndexState as EntryState};
use teseladb_plan::cost::{TRIAL_RESULTS, Trial, ranked, score, trial_works};
use teseladb_plan::logical::Bound;
use teseladb_plan::rules::{Access, IndexMeta, IndexScan, Request, branch_scans, candidates};
use teseladb_range::read::ReadConcern;

use super::index_maintenance::MULTIKEY_MARKS;
use super::mutation::from_mql;
use super::{Backend, query};
use crate::error::{CommandError, INTERNAL_ERROR};

/// The entries an index scan reads at a time.
const KEY_CHUNK: usize = 256;

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
}

impl PlanIndex {
    /// Unique.
    pub fn unique(&self) -> bool {
        self.catalog.unique
    }

    /// Sparse.
    pub fn sparse(&self) -> bool {
        self.catalog.sparse
    }

    /// Partial.
    pub fn partial(&self) -> bool {
        self.catalog.partial
    }
}

/// The ready indexes of `coll` the planner may consider, in the catalog's order; none for a
/// collection that is not a plain one (its documents' keys are not those of their `_id`).
pub(crate) fn plan_indexes<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
) -> Result<Vec<PlanIndex>, CommandError> {
    if !matches!(super::kinds::kind_of(coll)?, CollectionKind::Normal) {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in backend.indexes(coll.id)? {
        if entry.state != EntryState::Ready {
            continue;
        }
        let spec = RawDoc::from_bytes(&entry.spec)?;
        let ready = IndexState::Ready(Hlc { physical: 0, logical: 0 });
        let catalog = catalog_index(spec, entry.id, entry.buckets, ready)
            .map_err(|e| CommandError::new(INTERNAL_ERROR, e.reason))?;
        let Some(catalog) = catalog else { continue };
        let pattern = Document::from(RawDoc::from_bytes(&catalog.key_pattern)?);
        let ordered = pattern.iter().all(|(_, v)| {
            matches!(v, Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_))
        });
        let plannable = ordered
            && entry.buckets == 0
            && !catalog.hidden
            && !catalog.partial
            && !catalog.sparse
            && spec.get("collation").is_none()
            && pattern.iter().all(|(k, _)| !k.contains('$'));
        let multikey_paths = multikey_paths(backend, (coll.tenant, coll.id), &entry, &pattern)?;
        let key = pattern
            .iter()
            .map(|(k, v)| (k.to_owned(), if query_number(v) < 0.0 { -1 } else { 1 }))
            .collect();
        let meta = IndexMeta {
            name: entry.name.clone(),
            key,
            multikey: multikey_paths.iter().any(|(_, p)| !p.is_empty()),
            plannable,
        };
        let dirs = catalog.def.key_spec().fields.iter().map(|f| f.direction).collect();
        out.push(PlanIndex { entry, pattern, multikey_paths, meta, catalog, dirs });
    }
    Ok(out)
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
fn multikey_paths<B: Backend + ?Sized>(
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
    for (key, _) in rows {
        let rest = key.get(head..).unwrap_or_default();
        let Some(cut) = rest.iter().position(|b| *b == 0) else { continue };
        let field = String::from_utf8_lossy(&rest[..cut]).into_owned();
        let path = String::from_utf8_lossy(&rest[cut + 1..]).into_owned();
        if let Some((_, paths)) = out.iter_mut().find(|(f, _)| *f == field) {
            paths.push(path);
        }
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
    /// Documents read.
    pub docs: u64,
    /// Documents found, in order.
    pub found: Vec<(Vec<u8>, Vec<u8>)>,
    /// The unit of work at which the plan gave each document.
    given_at: Vec<u64>,
    /// What its trial counted, when plans competed.
    pub tried: Option<Tried>,
    /// The unit of work at which the plan read each document.
    docs_at: Vec<u64>,
    /// The entries each scan of an intersection read, in its order.
    pub child_keys: Vec<u64>,
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
}

impl Ran {
    /// The plan as its trial left it (`allPlansExecution`).
    pub fn at_trial(&self) -> Ran {
        let mut r = self.clone();
        if let Some(t) = self.tried {
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
        }
        r
    }
}

/// The plans of `read` on `coll` with `indexes`, the winner first, each with its trial.
pub(crate) fn choose<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    indexes: &[PlanIndex],
    read: &ReadPlan<'_>,
) -> Result<Chosen, CommandError> {
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
        let mut scans = subplanned(backend, coll, indexes, read, branches)?;
        scans.sort_by_key(|s| s.index);
        plans = vec![Access::Or(scans)];
    }
    if plans.len() < 2 {
        let access = plans.pop().unwrap_or(Access::Collscan);
        let winner = run(backend, coll, indexes, read, access, None)?;
        return Ok(Chosen { winner, others: Vec::new() });
    }
    let documents = count_documents(backend, coll, read.concern)?;
    let budget = trial_works(documents);
    let mut trials = Vec::new();
    for access in plans {
        trials.push(run(backend, coll, indexes, read, access, Some(budget))?);
    }
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
        });
    }
    // Two plans or more: the first of the order wins, and runs again without a budget.
    let order = ranked(&trials.iter().map(|t| t.trial).collect::<Vec<_>>());
    let best = &trials[order[0]];
    let mut winner = run(backend, coll, indexes, read, best.access.clone(), None)?;
    winner.tried = best.tried;
    let others = order[1..].iter().map(|i| trials[*i].clone()).collect();
    Ok(Chosen { winner, others })
}

/// The plan the planner chose, and the others it tried, best first.
pub(crate) struct Chosen {
    pub(crate) winner: Ran,
    pub(crate) others: Vec<Ran>,
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
    let documents = count_documents(backend, coll, read.concern)?;
    let budget = trial_works(documents);
    let branch_read = ReadPlan {
        filter: read.filter,
        parsed: read.parsed,
        sort: Vec::new(),
        limit: None,
        hint: None,
        vars: read.vars,
        concern: read.concern,
        intersect: false,
    };
    for scans in branches {
        if scans.len() < 2 {
            out.extend(scans);
            continue;
        }
        let mut trials = Vec::new();
        for s in &scans {
            let access = Access::Index(s.clone());
            trials.push(run(backend, coll, indexes, &branch_read, access, Some(budget))?);
        }
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

/// Runs `access` for `read`: for a trial (`budget` works at most), until it gives a batch, or
/// to its end.
fn run<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    indexes: &[PlanIndex],
    read: &ReadPlan<'_>,
    access: Access,
    budget: Option<u64>,
) -> Result<Ran, CommandError> {
    let collation = None;
    let plan_filter = |f: &Document| -> Result<teseladb_mql::Filter, CommandError> {
        let bytes = f.to_bytes()?;
        teseladb_mql::Filter::parse(RawDoc::from_bytes(&bytes)?, teseladb_mql::FilterUse::Query)
            .map_err(from_mql)
    };
    let ext = super::engines::NodeExtensions;
    let ctx = teseladb_mql::MatchContext { collation, ext: &ext, vars: read.vars };
    let mut ran = Ran {
        access: access.clone(),
        trial: Trial::default(),
        keys: 0,
        docs: 0,
        found: Vec::new(),
        given_at: Vec::new(),
        tried: None,
        docs_at: Vec::new(),
        child_keys: Vec::new(),
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
            return Ok(ran);
        }
        Access::Index(s) => vec![s.clone()],
        Access::Or(list) => list.clone(),
        Access::AndHash { scans, filter } => {
            return intersected(backend, coll, indexes, read, (scans, filter.as_ref()), ran);
        }
    };
    // A plan whose order is not the sort's gives nothing until it has read everything.
    let blocking = !read.sort.is_empty() && !scans.iter().all(|s| s.sorts);
    let batch = if blocking { None } else { budget.map(|_| TRIAL_RESULTS) };
    let limit = if blocking { None } else { read.limit };
    let mut seen: std::collections::BTreeSet<Vec<u8>> = std::collections::BTreeSet::new();
    let mut ended = true;
    'scans: for s in &scans {
        let index = &indexes[s.index];
        ran.child_keys.push(0);
        let residual = s.residual.as_ref().map(&plan_filter).transpose()?;
        for pk in entries(backend, (coll.tenant, coll.id), index, s, read.concern)? {
            if budget.is_some_and(|b| ran.keys >= b) {
                ended = false;
                break 'scans;
            }
            ran.keys += 1;
            if let Some(k) = ran.child_keys.last_mut() {
                *k += 1;
            }
            if !seen.insert(pk.clone()) {
                continue;
            }
            let key = KeyBuilder::document_of_pk(coll.tenant, coll.id, &pk);
            let next = [key.as_bytes(), &[0]].concat();
            let Some((dk, bytes)) =
                backend.scan(key.as_bytes(), &next, 1, read.concern)?.into_iter().next()
            else {
                continue;
            };
            ran.docs += 1;
            ran.docs_at.push(ran.keys);
            let keep = match &residual {
                Some(f) => f.matches(RawDoc::from_bytes(&bytes)?, &ctx).map_err(from_mql)?,
                None => true,
            };
            if keep {
                ran.found.push((dk, bytes));
                ran.given_at.push(ran.keys);
                let n = u64::try_from(ran.found.len()).unwrap_or(u64::MAX);
                if batch.is_some_and(|b| n >= b) || limit.is_some_and(|l| n >= l) {
                    ended = false;
                    break 'scans;
                }
            }
        }
    }
    let input = ran.keys + u64::from(ended);
    let advanced = u64::try_from(ran.found.len()).unwrap_or(u64::MAX);
    let works = if blocking {
        let out = read.limit.map_or(advanced, |l| advanced.min(l));
        ran.given_at = (1..=out).map(|k| input + k).collect();
        if ended { input + out } else { input }
    } else {
        input
    };
    ran.trial =
        Trial { works, advanced, fetches: true, sorts: blocking, intersects: false, eof: ended };
    Ok(ran)
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
            let bytes = f.to_bytes()?;
            Some(
                teseladb_mql::Filter::parse(
                    RawDoc::from_bytes(&bytes)?,
                    teseladb_mql::FilterUse::Query,
                )
                .map_err(from_mql)?,
            )
        }
        None => None,
    };
    let Some((last, held)) = scans.split_last() else { return Ok(ran) };
    let mut sets: Vec<std::collections::BTreeSet<Vec<u8>>> = Vec::new();
    for s in held {
        let pks = entries(backend, (coll.tenant, coll.id), &indexes[s.index], s, read.concern)?;
        let n = u64::try_from(pks.len()).unwrap_or(u64::MAX);
        ran.keys += n;
        ran.child_keys.push(n);
        sets.push(pks.into_iter().collect());
    }
    ran.child_keys.push(0);
    for pk in entries(backend, (coll.tenant, coll.id), &indexes[last.index], last, read.concern)? {
        ran.keys += 1;
        if let Some(k) = ran.child_keys.last_mut() {
            *k += 1;
        }
        if !sets.iter().all(|set| set.contains(&pk)) {
            continue;
        }
        let key = KeyBuilder::document_of_pk(coll.tenant, coll.id, &pk);
        let next = [key.as_bytes(), &[0]].concat();
        let Some((dk, bytes)) =
            backend.scan(key.as_bytes(), &next, 1, read.concern)?.into_iter().next()
        else {
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
    let advanced = u64::try_from(ran.found.len()).unwrap_or(u64::MAX);
    let sorts = !read.sort.is_empty() && !last.sorts;
    ran.trial =
        Trial { works: ran.keys + 1, advanced, fetches: true, sorts, intersects: true, eof: true };
    Ok(ran)
}

/// The primary keys of the entries of `index` within the intervals of `scan`, in its order.
fn entries<B: Backend + ?Sized>(
    backend: &mut B,
    (tenant, collection): (u32, u32),
    index: &PlanIndex,
    scan: &IndexScan,
    concern: ReadConcern,
) -> Result<Vec<Vec<u8>>, CommandError> {
    let ranges = key_ranges((tenant, collection), index, &scan.bounds)?;
    let head = KeyBuilder::index_prefix(tenant, collection, index.entry.id).as_bytes().len();
    let malformed = || CommandError::new(INTERNAL_ERROR, "an index entry that does not decode");
    let mut pks = Vec::new();
    for r in ranges {
        let end = r.end.clone().unwrap_or_else(|| {
            KeyBuilder::index_prefix(tenant, collection, index.entry.id + 1).as_bytes().to_vec()
        });
        let mut from = r.start.clone();
        loop {
            let rows = backend.scan(&from, &end, KEY_CHUNK, concern)?;
            let short = rows.len() < KEY_CHUNK;
            for (key, _) in &rows {
                let rest = key.get(head..).ok_or_else(malformed)?;
                let cut = head + values_len(rest, &index.dirs).map_err(|_| malformed())?;
                pks.push(key[cut..].to_vec());
            }
            match rows.last() {
                Some((k, _)) if !short => from = [k.as_slice(), &[0]].concat(),
                _ => break,
            }
        }
    }
    if scan.backward {
        pks.reverse();
    }
    Ok(pks)
}

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
        limit: (!m.multi).then_some(1),
        hint: super::hints::position(m.hint.as_ref(), &indexes)?,
        vars: m.vars.as_ref(),
        concern: ReadConcern::Local,
        intersect: false,
    };
    match choose(backend, coll, &indexes, &read)?.winner {
        Ran { access: Access::Collscan, .. } => Ok(None),
        winner => {
            let take = if m.multi { usize::MAX } else { 1 };
            Ok(Some(winner.found.into_iter().take(take).map(|(k, _)| k).collect()))
        }
    }
}

/// Filter `filter` as RefDb writes it back after parsing it, each `$expr` optimised.
pub(crate) fn parsed_filter(
    filter: &Document,
    vars: Option<&Document>,
) -> Result<Document, CommandError> {
    let filter = teseladb_exec::expr::optimize::optimize_filter(filter, vars).map_err(from_mql)?;
    let bytes = filter.to_bytes()?;
    let f =
        teseladb_mql::Filter::parse(RawDoc::from_bytes(&bytes)?, teseladb_mql::FilterUse::Query)
            .map_err(from_mql)?;
    Ok(f.parsed_query())
}

/// The documents a read of `coll` finds through the index plan that wins, in its order; `None`
/// when the collection scan wins or no index serves (the caller scans).
pub(crate) fn planned_documents<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    read: &ReadPlan<'_>,
) -> Result<Option<Vec<Vec<u8>>>, CommandError> {
    let indexes = plan_indexes(backend, coll)?;
    if indexes.is_empty() {
        return Ok(None);
    }
    match choose(backend, coll, &indexes, read)?.winner {
        Ran { access: Access::Collscan, .. } => Ok(None),
        winner => Ok(Some(winner.found.into_iter().map(|(_, d)| d).collect())),
    }
}
