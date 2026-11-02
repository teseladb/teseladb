//! The command side of time series (015 FR-006, FR-007, T012, T013): an insert goes, one
//! measurement at a time, to the first bucket of its window and meta value that takes it
//! ([`write`]); reads unpack the buckets their filter may match
//! (`teseladb_exec::timeseries_read`).

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_keys::KeyBuilder;
use teseladb_meta::catalog::CollectionEntry;
use teseladb_meta::collection_types::{CollectionKind, Granularity};
use teseladb_mql::timeseries_write::{BucketSpec, Full};

use super::Backend;
use super::crud::Writer;
use super::mutation::{Bound, DocMutation, MutationResult, TimeSeriesRule, WriteError};
use crate::error::{BAD_VALUE, CommandError, INTERNAL_ERROR, INVALID_OPTIONS, WRITE_CONFLICT};
use teseladb_range::read::ReadConcern;

/// Prefix of the bucket namespaces RefDb used before 9.0.
const LEGACY_BUCKETS: &str = "system.buckets.";

/// Refuses a command on a `system.buckets.*` namespace, as a `refrouter` 9.0.4 does (measured): the
/// buckets live in the time series' own namespace; `create` is named as the shard's command.
pub fn legacy_buckets(db: &str, coll: &str, command: &str) -> Result<(), CommandError> {
    if !coll.starts_with(LEGACY_BUCKETS) {
        return Ok(());
    }
    let shown =
        if command == super::catalog::CREATE { "_shardsvrCreateCollection" } else { command };
    Err(CommandError::new(
        crate::error::LEGACY_TIMESERIES_BUCKETS,
        format!(
            "Command {shown} on namespace {db}.{coll} is not supported. Direct access to timeseries buckets namespaces is not allowed anymore. Please target the main timeseries namespace instead. Use the rawData API to directly read timeseries buckets data in raw format."
        ),
    ))
}

use super::timeseries_catalog::{Pass, Stored, lookup, next_free};
pub use super::timeseries_catalog::{Route, TsStats};
/// The first nine bytes of bucket `id`: its window and meta value.
fn window_meta(id: [u8; 12]) -> [u8; 9] {
    std::array::from_fn(|i| id[i])
}

/// Most buckets of one window and meta value an insert tries before it gives up.
const MAX_BUCKETS: u32 = 1 << 20;

/// `bucketRoundingSeconds` of a granularity (RefDb's documentation of time series
/// granularity: one minute, one hour, one day).
fn rounding_of(g: Granularity) -> i64 {
    match g {
        Granularity::Seconds => 60,
        Granularity::Minutes => 3_600,
        Granularity::Hours => 86_400,
    }
}

/// How time series `coll` lays out its buckets; `None` when it is no time series.
pub fn spec_of(coll: &CollectionEntry) -> Result<Option<BucketSpec>, CommandError> {
    let CollectionKind::TimeSeries(ts) = super::kinds::kind_of(coll)? else { return Ok(None) };
    let rounding = match (ts.bucket_rounding_seconds, ts.granularity) {
        (Some(r), _) => r,
        (None, Some(g)) => rounding_of(g),
        (None, None) => ts.bucket_max_span_seconds,
    };
    Ok(Some(BucketSpec {
        time_field: ts.time_field,
        meta_field: ts.meta_field,
        max_span_seconds: ts.bucket_max_span_seconds,
        rounding_seconds: rounding,
    }))
}

/// The key of bucket `id` of `coll`: buckets are kept in `_id` order, as a clustered collection.
pub fn bucket_key(coll: &CollectionEntry, id: [u8; 12]) -> Result<Vec<u8>, CommandError> {
    let holder = teseladb_bson::Document::new().with("_id", Value::ObjectId(id)).to_bytes()?;
    let raw = RawDoc::from_bytes(&holder)?;
    let id = raw.get("_id").ok_or_else(|| CommandError::new(INTERNAL_ERROR, "an _id"))?;
    Ok(KeyBuilder::document_clustered(coll.tenant, coll.id, id).as_bytes().to_vec())
}

/// The keys of the buckets of `coll` a read within `bounds` may need: buckets are in `_id`
/// order, which starts with their window's start, so a bound on the time narrows the scan to the
/// windows it reaches.
pub fn scan_range(
    coll: &CollectionEntry,
    spec: &BucketSpec,
    bounds: &teseladb_exec::timeseries_read::Bounds,
) -> Result<(Vec<u8>, Vec<u8>), CommandError> {
    let (start, end) = super::query::documents_of(coll);
    let at = |window: i64| -> Result<Vec<u8>, CommandError> {
        let mut id = [0u8; 12];
        id[..4].copy_from_slice(&(window.clamp(0, i64::from(u32::MAX)) as u32).to_be_bytes());
        bucket_key(coll, id)
    };
    let from = match bounds.after {
        Some(t) if spec.window(t) > 0 => at(spec.window(t))?,
        _ => start,
    };
    let to = match bounds.before {
        Some(t) if spec.window(t) + spec.max_span_seconds <= i64::from(u32::MAX) => {
            at(spec.window(t) + spec.max_span_seconds)?
        }
        _ => end,
    };
    Ok((from, to))
}

/// Runs an insert of measurements `docs` into time series `coll`: those of one window and meta
/// value go together, the windows of a meta value in time order as a `refnode` 9.0.4 writes a
/// batch, each into its buckets from the one the catalog routes it to ([`TsStats::route`]). A
/// measurement without a date in the time field is a write error; an ordered insert takes
/// nothing after it. An ordered insert with a measurement the collection refuses is tried whole
/// and aborted, then retried one measurement at a time up to the refused one, which fails
/// alone (measured on a `refnode` 9.0.4 on 02-11-2026: the lookups count twice, one commit per
/// measurement, and the refused one's bucket leaves the catalog).
pub fn write<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &super::Shared,
    w: &mut Writer,
    (coll, spec): (&CollectionEntry, &BucketSpec),
    (docs, ordered, rules): (&[Document], bool, &super::mutation::Rules),
    stmt: u32,
) -> Result<MutationResult, CommandError> {
    let mut total = MutationResult::default();
    // The measurements with a time, and their place in `docs`.
    let mut timed = Vec::with_capacity(docs.len());
    let mut no_time = docs.len();
    for (index, doc) in docs.iter().enumerate() {
        match spec.time_of(doc) {
            Ok(t) => timed.push((index, t)),
            Err(e) => {
                let msg = match e {
                    teseladb_mql::timeseries_write::TsError::NoTime(m) => m,
                    other => format!("{other:?}"),
                };
                total.errors.push(WriteError { index, error: CommandError::new(BAD_VALUE, msg) });
                if ordered {
                    no_time = index;
                    break;
                }
            }
        }
    }
    let batch = Batch { coll, spec, ordered, rules, stmt };
    let cut = match ordered {
        true => super::mutation::timeseries::first_refused(&docs[..no_time], spec, rules),
        false => None,
    };
    if let Some(cut) = cut {
        // The measurement without a time comes after the refused one.
        total.errors.clear();
        write_cut(backend, shared, w, &batch, (docs, &timed), cut, &mut total)?;
    } else {
        let mut pass = Pass::default();
        for (first, group) in groups(spec, docs, &timed)? {
            let failed =
                write_group(backend, shared, w, &batch, (&group, first), &mut pass, &mut total)?;
            if failed && ordered {
                break;
            }
        }
    }
    // Unordered, the errors come batch by batch, after those of the measurements without a
    // time; an ordered insert fails at its first failure only.
    if ordered {
        total.errors.sort_by_key(|e| e.index);
        total.errors.truncate(1);
    }
    Ok(total)
}

/// The first bucket of a window and meta value, and its measurements with their places.
type Group = ([u8; 12], Vec<(usize, Document)>);

/// The measurements of `timed` grouped by the first bucket of their window and meta value, in
/// order inside each group; the meta values in the order they come, each one's windows in time
/// order.
fn groups(
    spec: &BucketSpec,
    docs: &[Document],
    timed: &[(usize, i64)],
) -> Result<Vec<Group>, CommandError> {
    let mut groups: Vec<Group> = Vec::new();
    for &(index, t) in timed {
        let doc = &docs[index];
        let first = spec
            .bucket_id(t, spec.meta_of(doc), 0)
            .map_err(|e| CommandError::new(INTERNAL_ERROR, format!("{e:?}")))?;
        match groups.iter_mut().find(|(id, _)| *id == first) {
            Some((_, g)) => g.push((index, doc.clone())),
            None => groups.push((first, vec![(index, doc.clone())])),
        }
    }
    let mut metas: Vec<[u8; 5]> = Vec::new();
    for (id, _) in &groups {
        let meta: [u8; 5] = std::array::from_fn(|i| id[4 + i]);
        if !metas.contains(&meta) {
            metas.push(meta);
        }
    }
    groups.sort_by_key(|(id, _)| {
        let meta: [u8; 5] = std::array::from_fn(|i| id[4 + i]);
        (metas.iter().position(|m| *m == meta), *id)
    });
    Ok(groups)
}

/// An ordered insert whose measurement at `cut` the collection refuses: the whole batch is tried
/// first and aborted, then the measurements up to the refused one go in one at a time.
fn write_cut<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &super::Shared,
    w: &mut Writer,
    batch: &Batch<'_>,
    (docs, timed): (&[Document], &[(usize, i64)]),
    cut: usize,
    total: &mut MutationResult,
) -> Result<(), CommandError> {
    let mut pass = Pass::attempt();
    for (first, group) in groups(batch.spec, docs, timed)? {
        attempt_group(backend, shared, batch, (&group, first), &mut pass)?;
    }
    with_stats(shared, batch.coll.id, |s| s.abort(&pass));
    for &(index, t) in timed.iter().take_while(|(i, _)| *i <= cut) {
        let doc = &docs[index];
        let first = batch
            .spec
            .bucket_id(t, batch.spec.meta_of(doc), 0)
            .map_err(|e| CommandError::new(INTERNAL_ERROR, format!("{e:?}")))?;
        let one = [(index, doc.clone())];
        let failed =
            write_group(backend, shared, w, batch, (&one, first), &mut Pass::default(), total)?;
        if failed {
            break;
        }
    }
    Ok(())
}

/// Runs `f` on the bucket catalog of time series `id`.
fn with_stats<R>(shared: &super::Shared, id: u32, f: impl FnOnce(&mut TsStats) -> R) -> R {
    f(super::lock(shared).timeseries.entry(id).or_default())
}

/// The buckets stored for the window and meta value of bucket `first`.
fn window_buckets<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    spec: &BucketSpec,
    first: [u8; 12],
) -> Result<Vec<Stored>, CommandError> {
    let mut last = first;
    last[9..].copy_from_slice(&MAX_BUCKETS.to_be_bytes()[1..]);
    let (mut next, end) = (bucket_key(coll, first)?, bucket_key(coll, last)?);
    let mut out = Vec::new();
    loop {
        let rows = backend.scan(&next, &end, super::query::SCAN_CHUNK, ReadConcern::Local)?;
        let short = rows.len() < super::query::SCAN_CHUNK;
        for (key, bytes) in rows {
            next = [key.as_slice(), &[0]].concat();
            out.extend(Stored::of(spec, bytes));
        }
        if short {
            return Ok(out);
        }
    }
}

/// The earliest time of the measurements of `group`.
fn earliest(spec: &BucketSpec, group: &[(usize, Document)]) -> i64 {
    group.iter().filter_map(|(_, d)| spec.time_of(d).ok()).min().unwrap_or(i64::MAX)
}

/// Why bucket `b` cannot take the first of `docs`, if it cannot.
fn full_for(spec: &BucketSpec, b: &Stored, docs: &[Document]) -> Option<Full> {
    let raw = RawDoc::from_bytes(&b.bytes).ok()?;
    match spec.append_all([0; 12], Some(raw), docs) {
        Err(teseladb_mql::timeseries_write::TsError::Full(why)) => Some(why),
        _ => None,
    }
}

/// The aborted try of `group`, measurements of one window and meta value whose first bucket is
/// `first`: its lookup counts, and an open bucket it finds full closes; its bucket opens only
/// for the rest of the try.
fn attempt_group<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &super::Shared,
    batch: &Batch<'_>,
    (group, first): (&[(usize, Document)], [u8; 12]),
    pass: &mut Pass,
) -> Result<(), CommandError> {
    let Batch { coll, spec, .. } = *batch;
    let (route, open) =
        with_stats(shared, coll.id, |s| (s.route(first, pass), s.open_bucket(window_meta(first))));
    let buckets = window_buckets(backend, coll, spec, first)?;
    let into = match route {
        Route::New => None,
        Route::Open => buckets.iter().find(|b| b.n == open),
        Route::Query { .. } => lookup(&buckets, earliest(spec, group), spec.meta_field.is_some()),
    };
    let docs: Vec<Document> = group.iter().map(|(_, d)| d.clone()).collect();
    let full = into.and_then(|b| full_for(spec, b, &docs));
    with_stats(shared, coll.id, |s| {
        if let Route::Query { .. } = route {
            s.looked_up(first, into.is_some());
            if into.is_some() {
                s.landed(first, full.is_some(), pass);
            }
        }
        if let Some(why) = full {
            s.closed_in_attempt(why);
        }
        s.provisional(first, pass);
    });
    Ok(())
}

/// What every group of an insert of measurements shares: the collection, its buckets, the
/// insert's order, the write's rules and its statement number.
#[derive(Clone, Copy)]
struct Batch<'a> {
    coll: &'a CollectionEntry,
    spec: &'a BucketSpec,
    ordered: bool,
    rules: &'a super::mutation::Rules,
    stmt: u32,
}

/// The number of the next new bucket of the window and meta value of `first` after bucket `n`,
/// reading the stored ones the first time.
fn after<B: Backend + ?Sized>(
    backend: &mut B,
    (coll, spec): (&CollectionEntry, &BucketSpec),
    first: [u8; 12],
    buckets: &mut Option<Vec<Stored>>,
    n: u32,
) -> Result<u32, CommandError> {
    let stored = match buckets.take() {
        Some(b) => b,
        None => window_buckets(backend, coll, spec, first)?,
    };
    let free = next_free(&stored).max(n + 1);
    *buckets = Some(stored);
    Ok(free)
}

/// Writes `group`, measurements of one window and meta value whose first bucket is `first`,
/// into `total`: from the bucket the catalog routes it to, and past a full one into a new
/// bucket, each write the measurements one bucket takes, which go in or fail together
/// (measured on a `refnode` 9.0.4 on 02-11-2026). Unordered, a failed write fails the rest of the
/// group with its error; ordered, the measurements before the failing one go in and it fails
/// alone, as the update of their bucket. A bucket whose write failed leaves the catalog. Whether
/// one failed.
fn write_group<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &super::Shared,
    w: &mut Writer,
    batch: &Batch<'_>,
    (group, first): (&[(usize, Document)], [u8; 12]),
    pass: &mut Pass,
    total: &mut MutationResult,
) -> Result<bool, CommandError> {
    let Batch { coll, spec, ordered, rules, stmt } = *batch;
    use super::mutation::timeseries::{
        BUCKET_EXISTED, KEPT_OPEN_LARGE, STOPPED_BY_COUNT, STOPPED_BY_SCHEMA, STOPPED_BY_SIZE,
        UPDATE_FAILED,
    };
    let mut pending = group;
    let (t, meta) = (spec.time_of(&pending[0].1), spec.meta_of(&pending[0].1).cloned());
    let t = t.map_err(|e| CommandError::new(INTERNAL_ERROR, format!("{e:?}")))?;
    let key = window_meta(first);
    let (route, open) = with_stats(shared, coll.id, |s| (s.route(first, pass), s.open_bucket(key)));
    let mut buckets: Option<Vec<Stored>> = None;
    // A bucket a lookup found, whose room the first write tells.
    let mut landing = false;
    let mut n = match route {
        Route::Open => open,
        Route::New => {
            let stored = window_buckets(backend, coll, spec, first)?;
            let n = next_free(&stored);
            buckets = Some(stored);
            n
        }
        Route::Query { .. } => {
            let stored = window_buckets(backend, coll, spec, first)?;
            let landed =
                lookup(&stored, earliest(spec, group), spec.meta_field.is_some()).map(|b| b.n);
            with_stats(shared, coll.id, |s| s.looked_up(first, landed.is_some()));
            landing = landed.is_some();
            let n = landed.unwrap_or_else(|| next_free(&stored));
            buckets = Some(stored);
            n
        }
    };
    let mut first_commit = Some(route);
    let mut closing = None;
    // Ordered, the failure that waits for the measurements before it to go in.
    let mut failure: Option<WriteError> = None;
    while !pending.is_empty() {
        if n >= MAX_BUCKETS {
            return Err(CommandError::new(WRITE_CONFLICT, "every bucket of the window is full"));
        }
        let bucket = spec
            .bucket_id(t, meta.as_ref(), n)
            .map_err(|e| CommandError::new(INTERNAL_ERROR, format!("{e:?}")))?;
        let mut rules = rules.clone();
        rules.timeseries = Some(TimeSeriesRule { spec: spec.clone(), bucket });
        let docs = pending.iter().map(|(_, d)| d.clone()).collect();
        let mutation = DocMutation::Insert { docs, ordered, rules };
        let b = Bound { mutation, keys: vec![bucket_key(coll, bucket)?] };
        // Each write of the statement its own number: that of its first measurement.
        let at = u32::try_from(pending[0].0).unwrap_or(u32::MAX);
        let r = match w.write(backend, &b, stmt.saturating_add(at)) {
            Err(e) if super::mutation::timeseries::is_refusal(&e) => {
                closing = super::mutation::timeseries::refusal(&e);
                let landed = std::mem::take(&mut landing);
                with_stats(shared, coll.id, |s| {
                    if closing == Some(Full::Schema) {
                        s.closed_for_schema(key, n);
                    }
                    if landed {
                        s.landed(first, true, pass);
                    }
                });
                n = after(backend, (coll, spec), first, &mut buckets, n)?;
                continue;
            }
            written => written?,
        };
        if std::mem::take(&mut landing) {
            with_stats(shared, coll.id, |s| s.landed(first, false, pass));
        }
        if r.n > 0 && !r.retried {
            let existed = r.n_modified & BUCKET_EXISTED != 0;
            let mut g = super::lock(shared);
            let stats = g.timeseries.entry(coll.id).or_default();
            if let Some(route) = first_commit.take() {
                stats.first_commit(route, existed, closing.is_some());
            }
            stats.commit((bucket, n), existed, closing, r.n, pass);
            if r.n_modified & KEPT_OPEN_LARGE != 0 {
                stats.kept_open_large += 1;
            }
            closing = match r.n_modified {
                f if f & STOPPED_BY_COUNT != 0 => Some(Full::Count),
                f if f & STOPPED_BY_SIZE != 0 => Some(Full::Size),
                f if f & STOPPED_BY_SCHEMA != 0 => {
                    stats.closed_for_schema(key, n);
                    Some(Full::Schema)
                }
                _ => None,
            };
        }
        total.n += r.n;
        total.retried |= r.retried;
        total.warnings.extend(r.warnings);
        if let Some(e) = r.errors.into_iter().next() {
            with_stats(shared, coll.id, |s| s.cleared(bucket));
            if ordered {
                let mut error = e.error;
                if e.index > 0 && !error.errmsg.starts_with(UPDATE_FAILED) {
                    error.errmsg = format!("{UPDATE_FAILED}{}", error.errmsg);
                }
                failure = Some(WriteError { index: pending[e.index].0, error });
                pending = &pending[..e.index];
                continue;
            }
            for (index, _) in pending {
                total.errors.push(WriteError { index: *index, error: e.error.clone() });
            }
            return Ok(true);
        }
        let taken = usize::try_from(r.n).unwrap_or(0).min(pending.len());
        pending = &pending[taken..];
        if !pending.is_empty() {
            n = after(backend, (coll, spec), first, &mut buckets, n)?;
        }
    }
    let failed = failure.is_some();
    total.errors.extend(failure);
    Ok(failed)
}

/// Whether `filter` names only the meta field (`m` or `m.<path>`), also inside `$and`, `$or` and
/// `$nor`.
fn meta_only(meta: Option<&str>, filter: &Document) -> bool {
    filter.iter().all(|(k, v)| match (k, v) {
        ("$and" | "$or" | "$nor", Value::Array(items)) => items.iter().all(|i| match i {
            Value::Document(d) => meta_only(meta, d),
            _ => false,
        }),
        (k, _) => {
            meta.is_some_and(|m| k == m || k.strip_prefix(m).is_some_and(|r| r.starts_with('.')))
        }
    })
}

/// The refusals of an update of time series measurements, in a `refrouter` 9.0.4's order (measured
/// on 31-10-2026): an upsert, a single update, a filter on another field than the meta value, a
/// replacement, a pipeline, an update of another field.
fn check_update(
    spec: &BucketSpec,
    m: &super::crud::Modify,
    update: &Value,
) -> Result<(), CommandError> {
    let refuse = |msg: String| Err(CommandError::new(INVALID_OPTIONS, msg));
    let meta = spec.meta_field.as_deref().unwrap_or_default();
    if m.upsert {
        return refuse("Cannot perform an upsert on a time-series collection".into());
    }
    if !m.multi {
        return refuse("Cannot perform a non-multi update on a time-series collection".into());
    }
    if !meta_only(spec.meta_field.as_deref(), &m.filter) {
        return refuse(format!(
            "Cannot perform an update or delete on a time-series collection when querying on a field that is not the metaField '{meta}'"
        ));
    }
    let ops = match update {
        Value::Document(d) if d.iter().next().is_some_and(|(k, _)| k.starts_with('$')) => d,
        Value::Array(_) => {
            return refuse(
                "Cannot perform an update on a time-series collection using a pipeline update"
                    .into(),
            );
        }
        _ => {
            return refuse(
                "Cannot perform an update on a time-series collection using a replacement document"
                    .into(),
            );
        }
    };
    let touches_meta = |path: &str| {
        spec.meta_field
            .as_deref()
            .is_some_and(|m| path == m || path.strip_prefix(m).is_some_and(|r| r.starts_with('.')))
    };
    for (op, fields) in ops.iter() {
        let Value::Document(fields) = fields else { continue };
        for (path, v) in fields.iter() {
            let renamed = op == "$rename" && matches!(v, Value::String(to) if !touches_meta(to));
            if !touches_meta(path) || renamed {
                return refuse(format!(
                    "Cannot perform an update on a time-series collection which updates a field that is not the metaField '{meta}'"
                ));
            }
        }
    }
    Ok(())
}

/// The refusals of update statement `m` on time series `spec` (none for another collection or a
/// delete).
pub fn check_statement(
    spec: Option<&BucketSpec>,
    m: &super::crud::Modify,
) -> Result<(), CommandError> {
    match (spec, &m.update) {
        (Some(spec), Some(u)) => check_update(spec, m, u),
        _ => Ok(()),
    }
}

/// One bucket as a statement reads it: its key, `_id`, meta value, measurements (without the
/// meta value) and the `data` it had, which its rewrite checks.
struct Read {
    key: Vec<u8>,
    id: [u8; 12],
    meta: Option<Value>,
    rows: Vec<teseladb_bson::Document>,
    data: Value,
}

fn read_bucket(spec: &BucketSpec, key: Vec<u8>, bytes: &[u8]) -> Result<Read, CommandError> {
    let raw = RawDoc::from_bytes(bytes)?;
    let doc = teseladb_bson::Document::from(raw);
    // A bucket is written with its `_id` and its `data`.
    let malformed = || CommandError::new(INTERNAL_ERROR, "a bucket without _id or data");
    let id = match doc.get("_id") {
        Some(Value::ObjectId(id)) => *id,
        _ => return Err(malformed()),
    };
    let data = doc.get("data").ok_or_else(malformed)?;
    let rows =
        spec.measurements(raw).map_err(|e| CommandError::new(INTERNAL_ERROR, format!("{e:?}")))?;
    Ok(Read { key, id, meta: doc.get("meta").cloned(), rows, data: data.clone() })
}

/// Rewrites bucket `b` as `new` (none: deleted), if it still holds what was read; `false` when
/// another write changed it first.
fn rewrite<B: Backend + ?Sized>(
    backend: &mut B,
    w: &mut Writer,
    rules: &super::mutation::Rules,
    b: &Read,
    new: Option<teseladb_bson::Document>,
    stmt: u32,
) -> Result<bool, CommandError> {
    use super::mutation::Image;
    let filter = teseladb_bson::Document::new()
        .with("_id", Value::ObjectId(b.id))
        .with("data", b.data.clone());
    let mutation = match new {
        Some(mut doc) => {
            doc.remove("_id");
            DocMutation::Update {
                filter,
                update: Value::Document(doc),
                array_filters: None,
                multi: false,
                upsert: None,
                image: Image::None,
                rules: rules.clone(),
            }
        }
        None => {
            DocMutation::Delete { filter, multi: false, image: Image::None, rules: rules.clone() }
        }
    };
    let r = w.write(backend, &Bound { mutation, keys: vec![b.key.clone()] }, stmt)?;
    if let Some(e) = r.errors.into_iter().next() {
        return Err(e.error);
    }
    Ok(r.n == 1)
}

/// How many times a statement reads a bucket again when another write changed it first.
const MAX_REREADS: usize = 64;

/// An update or delete statement over the measurements of time series `coll` (015 FR-007): a
/// delete takes the matching measurements out of their buckets, an update changes the meta value
/// of whole buckets, each bucket rewritten if no other write changed it since it was read.
pub fn modify<B: Backend + ?Sized>(
    backend: &mut B,
    w: &mut Writer,
    (coll, spec): (&CollectionEntry, &BucketSpec),
    m: &super::crud::Modify,
    rules: &super::mutation::Rules,
    stmt: u32,
) -> Result<MutationResult, CommandError> {
    if let Some(u) = &m.update {
        check_update(spec, m, u)?;
    }
    let bounds = teseladb_exec::timeseries_read::Bounds::of(spec, &m.filter);
    let millis = i64::try_from(backend.now_ms()).unwrap_or(i64::MAX);
    let seconds = u32::try_from(millis / 1000).unwrap_or(u32::MAX);
    let now = teseladb_mql::update::Now { millis, timestamp: (seconds, 1) };
    let statement = Statement { spec, rules, m, stmt, now };
    let (mut next, end) = scan_range(coll, spec, &bounds)?;
    let collation = collation(rules)?;
    let collator = collation.as_ref().map(|c| c as &dyn teseladb_bson::Collator);
    let mut total = MutationResult::default();
    loop {
        let rows = backend.scan(&next, &end, super::query::SCAN_CHUNK, ReadConcern::Local)?;
        let short = rows.len() < super::query::SCAN_CHUNK;
        for (key, bytes) in rows {
            next = [key.as_slice(), &[0]].concat();
            if !bounds
                .may_match(spec, RawDoc::from_bytes(&bytes)?, collator)
                .map_err(|e| CommandError::new(INTERNAL_ERROR, format!("{e:?}")))?
            {
                continue;
            }
            let done = bucket(backend, w, &statement, key, bytes, &mut total)?;
            if done {
                return Ok(total);
            }
        }
        if short {
            return Ok(total);
        }
    }
}

/// A statement over the buckets of a time series.
struct Statement<'a> {
    spec: &'a BucketSpec,
    rules: &'a super::mutation::Rules,
    m: &'a super::crud::Modify,
    stmt: u32,
    /// The time of the write, for `$currentDate`.
    now: teseladb_mql::update::Now,
}

/// What a statement does to one bucket: its new content (`None`: deleted), the measurements it
/// matched and modified, and the measurement a `findAndModify` returns.
struct Change {
    new: Option<Option<teseladb_bson::Document>>,
    n: i64,
    modified: i64,
    value: Option<teseladb_bson::Document>,
}

fn mql(e: teseladb_mql::MqlError) -> CommandError {
    super::mutation::from_mql(e)
}

/// The collation of a statement's rules.
fn collation(
    rules: &super::mutation::Rules,
) -> Result<Option<teseladb_collation::Collation>, CommandError> {
    let Some(c) = &rules.collation else { return Ok(None) };
    let bytes = c.to_bytes()?;
    Ok(match teseladb_collation::parse_collation(RawDoc::from_bytes(&bytes)?)? {
        teseladb_collation::ParsedCollation::Simple => None,
        teseladb_collation::ParsedCollation::Collation(c, _) => Some(c),
    })
}

impl Statement<'_> {
    /// The parsed filter and its context's collation.
    fn filter(&self) -> Result<(teseladb_mql::Filter, Vec<u8>), CommandError> {
        let bytes = self.m.filter.to_bytes()?;
        let f = teseladb_mql::Filter::parse(
            RawDoc::from_bytes(&bytes)?,
            teseladb_mql::FilterUse::Query,
        )
        .map_err(mql)?;
        Ok((f, bytes))
    }

    /// The measurements of `b` a delete takes out, and what is left.
    fn delete(&self, b: &Read, bytes: &[u8]) -> Result<Change, CommandError> {
        let (f, _) = self.filter()?;
        let c = collation(self.rules)?;
        let ctx = teseladb_mql::MatchContext {
            collation: c.as_ref().map(|c| c as &dyn teseladb_bson::Collator),
            ext: &super::engines::NodeExtensions,
            vars: self.m.vars.as_ref(),
        };
        let unpacked = self
            .spec
            .unpack(RawDoc::from_bytes(bytes)?)
            .map_err(|e| CommandError::new(INTERNAL_ERROR, format!("{e:?}")))?;
        let (mut kept, mut value, mut n) = (Vec::new(), None, 0);
        for (row, doc) in b.rows.iter().zip(unpacked) {
            let db = doc.to_bytes()?;
            let take = (self.m.multi || n == 0)
                && f.matches(RawDoc::from_bytes(&db)?, &ctx).map_err(mql)?;
            if take {
                n += 1;
                value = value.or(Some(doc));
            } else {
                kept.push(row.clone());
            }
        }
        let new = match kept.is_empty() {
            true => None,
            false => Some(
                self.spec
                    .build(b.id, b.meta.as_ref(), &kept)
                    .map_err(|e| CommandError::new(INTERNAL_ERROR, format!("{e:?}")))?,
            ),
        };
        let value = value.filter(|_| self.m.image != super::mutation::Image::None);
        Ok(Change { new: (n > 0).then_some(new), n, modified: 0, value })
    }

    /// An update of the meta value of every measurement of `b`, if its meta value matches.
    fn update(&self, b: &Read, u: &Value) -> Result<Change, CommandError> {
        let none = Change { new: None, n: 0, modified: 0, value: None };
        let Some(field) = self.spec.meta_field.as_deref() else { return Ok(none) };
        let mut meta = teseladb_bson::Document::new();
        if let Some(v) = &b.meta {
            meta.push(field, v.clone());
        }
        let mb = meta.to_bytes()?;
        let doc = RawDoc::from_bytes(&mb)?;
        let (f, _) = self.filter()?;
        let c = collation(self.rules)?;
        let ctx = teseladb_mql::MatchContext {
            collation: c.as_ref().map(|c| c as &dyn teseladb_bson::Collator),
            ext: &super::engines::NodeExtensions,
            vars: self.m.vars.as_ref(),
        };
        let Some(position) = f.match_position(doc, &ctx).map_err(mql)? else { return Ok(none) };
        let holder = teseladb_bson::Document::new().with("u", u.clone());
        let hb = holder.to_bytes()?;
        let h = RawDoc::from_bytes(&hb)?;
        let raw_u = h.get("u").ok_or_else(|| CommandError::new(INTERNAL_ERROR, "an update"))?;
        let parsed = teseladb_mql::update::Update::parse(raw_u, None).map_err(mql)?;
        let uctx = teseladb_mql::update::UpdateContext {
            matcher: ctx,
            position,
            now: self.now,
            insert: false,
        };
        let out = parsed.apply(doc, &uctx).map_err(mql)?;
        let n = b.rows.len() as i64;
        if !out.modified {
            return Ok(Change { new: None, n, modified: 0, value: None });
        }
        let bucket = self
            .spec
            .build(b.id, out.doc.get(field), &b.rows)
            .map_err(|e| CommandError::new(INTERNAL_ERROR, format!("{e:?}")))?;
        Ok(Change { new: Some(Some(bucket)), n, modified: n, value: None })
    }
}

/// The statement on one bucket, read again while other writes change it; `true` when a single
/// delete is done.
fn bucket<B: Backend + ?Sized>(
    backend: &mut B,
    w: &mut Writer,
    s: &Statement<'_>,
    key: Vec<u8>,
    mut bytes: Vec<u8>,
    total: &mut MutationResult,
) -> Result<bool, CommandError> {
    for _ in 0..MAX_REREADS {
        let b = read_bucket(s.spec, key.clone(), &bytes)?;
        let change = match &s.m.update {
            None => s.delete(&b, &bytes)?,
            Some(u) => s.update(&b, u)?,
        };
        let written = match change.new {
            None => true,
            Some(new) => rewrite(backend, w, s.rules, &b, new, s.stmt)?,
        };
        if written {
            total.n += change.n;
            total.n_modified += change.modified;
            total.value = total.value.take().or(change.value);
            return Ok(change.n > 0 && !s.m.multi && s.m.update.is_none());
        }
        let (_, versions) = backend.read_versions(std::slice::from_ref(&key))?;
        match versions.into_iter().next().flatten() {
            Some((_, v)) => bytes = v,
            None => return Ok(false),
        }
    }
    Err(CommandError::new(WRITE_CONFLICT, "the bucket kept changing during the write"))
}

/// The default index of a time series with a meta field: `{<meta>: 1, <time>: 1}` (measured on a
/// `refrouter` 9.0.4: `create` makes it).
pub fn default_index(spec: &BucketSpec) -> Option<Document> {
    let meta = spec.meta_field.as_deref()?;
    let key = Document::new().with(meta, 1).with(spec.time_field.as_str(), 1);
    let name = format!("{meta}_1_{}_1", spec.time_field);
    Some(Document::new().with("v", 2).with("key", key).with("name", name))
}

/// `path` under the meta field, as the bucket names it (`meta.<rest>`).
fn meta_path(spec: &BucketSpec, path: &str) -> Option<String> {
    let meta = spec.meta_field.as_deref()?;
    match path.strip_prefix(meta) {
        Some("") => Some("meta".to_owned()),
        Some(rest) if rest.starts_with('.') => Some(format!("meta{rest}")),
        _ => None,
    }
}

/// Whether an index direction is a number: ascending (`> 0`) or descending.
fn direction(v: &Value) -> Option<bool> {
    match v {
        Value::Int32(n) => Some(*n > 0),
        Value::Int64(n) => Some(*n > 0),
        Value::Double(n) => Some(*n > 0.0),
        _ => None,
    }
}

/// The bucket key of index key `key` of time series `spec`; `measured` says whether it names a
/// measurement field (not the meta value nor the time).
fn bucket_key_of(spec: &BucketSpec, key: &Document) -> Result<(Document, bool), CommandError> {
    let mut out = Document::new();
    let mut measured = false;
    for (path, dir) in key.iter() {
        if let Some(p) = meta_path(spec, path) {
            out.push(p, dir.clone());
            continue;
        }
        let is_time = path == spec.time_field;
        measured |= !is_time;
        match (direction(dir), dir) {
            (Some(up), _) => {
                let (min, max) = (format!("control.min.{path}"), format!("control.max.{path}"));
                let sign = if up { 1 } else { -1 };
                let (a, b) = if up { (min, max) } else { (max, min) };
                out.push(a, sign);
                out.push(b, sign);
            }
            (None, Value::String(s)) if s == "2dsphere" && !is_time => {
                out.push(format!("data.{path}"), "2dsphere_bucket");
            }
            (None, Value::String(s)) if s == "text" => {
                let msg = "Text indexes are not supported on time-series collections";
                return Err(CommandError::new(INVALID_OPTIONS, msg));
            }
            (None, other) => {
                let shown = super::display(other);
                let spec_text = super::display(&Value::Document(key.clone()));
                let allowed = if is_time {
                    "the time field must be ascending or descending (numbers only)"
                } else {
                    "measurement fields must be ascending or descending (numbers only), or '2dsphere'"
                };
                return Err(CommandError::new(
                    crate::error::CANNOT_CREATE_INDEX,
                    format!(
                        "BadValue: Invalid index spec for time-series collection: {spec_text}. Indexes on {allowed}: {path}: {shown}"
                    ),
                ));
            }
        }
    }
    Ok((out, measured))
}

/// `path` under the meta field as the bucket names it, for the partial filters.
pub(super) fn meta_path_of(spec: &BucketSpec, path: &str) -> Option<String> {
    meta_path(spec, path)
}

/// The bucket specification of index `index` of time series `spec`, or RefDb's refusal
/// (measured on a `refrouter` 9.0.4): a key on the meta value goes to `meta`, one on the time or a
/// measurement field to its least and greatest value in `control`; one on a measurement field, or
/// with a partial filter, keeps the index as it was asked in `originalSpec`.
pub fn bucket_index(spec: &BucketSpec, index: &Document) -> Result<Document, CommandError> {
    if index.get("unique").and_then(crate::types::truthy) == Some(true) {
        let msg = "Unique indexes are not supported on time-series collections";
        return Err(CommandError::new(INVALID_OPTIONS, msg));
    }
    let partial = match index.get("partialFilterExpression") {
        Some(Value::Document(f)) => Some(f),
        _ => None,
    };
    // The filter parses as a filter before it is translated to the buckets (`$in` without an
    // array is RefDb's parse error, measured on a `refnode` 9.0.4).
    if let Some(f) = partial {
        super::partial_filter::parse_error(f)?;
    }
    let on_meta = |f: &Document| f.iter().all(|(k, _)| meta_path(spec, k).is_some());
    if index.get("expireAfterSeconds").is_some() && !partial.is_some_and(on_meta) {
        let msg = "TTL indexes on time-series collections require a partialFilterExpression on the metaField";
        return Err(CommandError::new(INVALID_OPTIONS, msg));
    }
    let Some(Value::Document(key)) = index.get("key") else {
        return Err(CommandError::new(INTERNAL_ERROR, "an index without key"));
    };
    if index.get("expireAfterSeconds").is_some() {
        if key.len() > 1 {
            let msg = format!(
                "TTL indexes are single-field indexes, compound indexes do not support TTL. Index spec: {}",
                super::display(&Value::Document(key.clone()))
            );
            return Err(CommandError::new(crate::error::CANNOT_CREATE_INDEX, msg));
        }
        if key.iter().any(|(path, _)| path != spec.time_field) {
            let msg = "TTL indexes on non-time fields are not supported on time-series collections";
            return Err(CommandError::new(INVALID_OPTIONS, msg));
        }
    }
    let (bucket_key, measured) = bucket_key_of(spec, key)?;
    if measured && index.get("sparse").is_some() {
        let msg = "Sparse indexes are not supported on time-series measurements";
        return Err(CommandError::new(INVALID_OPTIONS, msg));
    }
    // A TTL index reads the earliest time of each bucket (measured on a `refnode` 9.0.4).
    let bucket_key = match (index.get("expireAfterSeconds"), key.iter().next()) {
        (Some(_), Some((path, dir))) => {
            Document::new().with(format!("control.min.{path}"), dir.clone())
        }
        _ => bucket_key,
    };
    let mut out = Document::new();
    for (k, v) in index.iter() {
        match k {
            "key" => out.push("key", bucket_key.clone()),
            "partialFilterExpression" => out.push(
                k,
                super::timeseries_filter::bucket_filter(spec, partial.unwrap_or(&Document::new()))?,
            ),
            _ => out.push(k, v.clone()),
        }
    }
    if measured || partial.is_some() {
        let mut original = Document::new();
        for (k, v) in index.iter().filter(|(k, _)| *k != "v") {
            original.push(k, v.clone());
        }
        original.push("v", index.get("v").cloned().unwrap_or(Value::Int32(2)));
        out.push("originalSpec", original);
    }
    Ok(out)
}

/// Index `stored` of time series `spec` as `listIndexes` shows it without `rawData`: as it was
/// asked (`originalSpec`), or with its key taken back to the measurements' fields.
pub fn user_index(spec: &BucketSpec, stored: &Document) -> Document {
    if let Some(Value::Document(original)) = stored.get("originalSpec") {
        let mut out =
            Document::new().with("v", original.get("v").cloned().unwrap_or(Value::Int32(2)));
        for (k, v) in original.iter().filter(|(k, _)| *k != "v") {
            out.push(k, v.clone());
        }
        for (k, v) in stored.iter() {
            if !["v", "key", "name", "partialFilterExpression", "originalSpec"].contains(&k)
                && out.get(k).is_none()
            {
                out.push(k, v.clone());
            }
        }
        // Shown in the order RefDb stores an index's fields, as `listIndexes` gives it.
        return super::indexes::in_stored_order(&out);
    }
    let mut out = Document::new();
    for (k, v) in stored.iter() {
        match (k, v) {
            ("key", Value::Document(key)) => {
                let mut user = Document::new();
                for (path, dir) in key.iter() {
                    if let Some(rest) = path.strip_prefix("meta") {
                        let meta = spec.meta_field.as_deref().unwrap_or_default();
                        user.push(format!("{meta}{rest}"), dir.clone());
                    } else if let Some(field) = path
                        .strip_prefix("control.min.")
                        .or_else(|| path.strip_prefix("control.max."))
                        && user.get(field).is_none()
                    {
                        user.push(field, dir.clone());
                    }
                }
                out.push("key", user);
            }
            _ => out.push(k, v.clone()),
        }
    }
    out
}
