//! `$changeStream` (014 FR-001, FR-002, FR-013): an `aggregate` whose first stage is
//! `$changeStream` opens a tailable `awaitData` cursor that `getMore` continues. Each `getMore`
//! reads the changes of the collections in its scope after the mark the cursor read up to, up to
//! the lowest resolved mark of their ranges (`Backend::changes`), turns them into events in the
//! merge order, runs the stages after `$changeStream` on each event and returns them with the
//! `postBatchResumeToken`. The replies, waits and errors are those of a `refrouter` 9.0.4 (measured on
//! 01-11): the first batch is always empty, an empty `getMore` waits 1 s or its `maxTimeMS`, and an
//! event whose `_id` the stages changed kills the cursor with `ChangeStreamFatalError`.

use std::collections::{BTreeMap, VecDeque};

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_changestream::changes::{Change, Versioned, changes, collection_of};
use teseladb_changestream::cursor::StreamPosition;
use teseladb_changestream::ddl::{ddl_key, invalidate_of};
use teseladb_changestream::event::{ChangeEvent, Namespace as EventNs};
use teseladb_changestream::order::EventKey;
use teseladb_changestream::resolved::below;
use teseladb_changestream::stage::{
    BeforeChange, CHANGE_STREAM, ChangeStreamSpec, FullDocument, SPLIT_LARGE_EVENT, Scope,
    StageError, Start, check_rest,
};
use teseladb_changestream::token::{EventKind, Position, ResumeToken};
use teseladb_hlc::Hlc;
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_meta::collection_types::CollectionKind;
use teseladb_meta::ddl_log::{DdlChange, DdlRecord};
use teseladb_range::read::ReadConcern;

use super::change_stream_ddl::Watch;
use super::query::{QueryCursor, documents_of, keep};
use super::{Backend, Ctx, kinds, names};
use crate::error::{self, CommandError};

/// What an empty `getMore` waits without `maxTimeMS` (measured: 1 s).
pub const DEFAULT_AWAIT_MS: u64 = 1_000;
/// The database a cluster-wide stream is opened on.
const ADMIN: &str = "admin";
/// The database of the cluster's configuration.
const CONFIG: &str = "config";
/// The databases whose collections a cluster-wide stream leaves out.
pub(crate) const INTERNAL_DBS: [&str; 3] = [ADMIN, CONFIG, "local"];
/// The prefix of a system collection.
pub(crate) const SYSTEM_PREFIX: &str = "system.";
/// The read concern a change stream accepts.
const MAJORITY: &str = "majority";
/// Characters of a string a fatal error's message shows before `...` (measured).
const SHOWN_STRING: usize = 150;

/// A change stream cursor.
#[derive(Clone, Debug, PartialEq)]
pub struct ChangeStream {
    scope: Scope,
    /// Database it was opened on (empty for the cluster).
    db: String,
    /// Collection, for a stream of one.
    coll: Option<String>,
    spec: ChangeStreamSpec,
    position: StreamPosition,
    /// Events read and not returned yet, with their tokens.
    pending: VecDeque<(ResumeToken, Document)>,
    /// The stages after `$changeStream`, without `$changeStreamSplitLargeEvent`.
    stages: Vec<Document>,
    /// `$changeStreamSplitLargeEvent` ends the pipeline (014 FR-011).
    split: bool,
    /// The mark the stream starts at, which `ChangeStreamHistoryLost` names.
    resume_ts: Hlc,
    /// The stream resumes after this fragment of this event.
    resume_fragment: Option<(EventKey, u32)>,
    /// The token of the `invalidate` that ended the stream: nothing is read after it, and the
    /// cursor closes once the events before it are returned, with it as `postBatchResumeToken`.
    invalidated: Option<ResumeToken>,
}

fn stage_error(e: StageError) -> CommandError {
    let code = error::code(e.code).unwrap_or(error::INTERNAL_ERROR);
    CommandError::new(code, e.message)
}

/// The name of the first stage of `stages`.
pub fn first_stage<'a>(stages: &[RawValue<'a>]) -> Option<&'a str> {
    match stages.first() {
        Some(RawValue::Document(d)) => d.iter().next().map(|(k, _)| k),
        _ => None,
    }
}

/// Refuses a `$changeStream` after the first stage of a pipeline (measured).
pub fn not_first(stages: &[RawValue<'_>]) -> Result<(), CommandError> {
    let later = stages.iter().skip(1).any(|s| {
        matches!(s, RawValue::Document(d) if d.iter().next().is_some_and(|(k, _)| k == CHANGE_STREAM))
    });
    if later {
        // `check_rest` refuses a `$changeStream` with the error of one that is not first.
        check_rest(&[CHANGE_STREAM]).map_err(stage_error)?;
    }
    Ok(())
}

/// Whether `cmd` is an `aggregate` that opens a change stream.
pub fn opens(cmd: &Document) -> bool {
    let is_aggregate = cmd.iter().next().is_some_and(|(k, _)| k == super::aggregate::AGGREGATE);
    let first = match cmd.get("pipeline") {
        Some(Value::Array(stages)) => stages.first(),
        _ => None,
    };
    let opens = matches!(first, Some(Value::Document(d)) if d.iter().next().is_some_and(|(k, _)| k == CHANGE_STREAM));
    is_aggregate && opens
}

/// Refuses a read concern other than `majority`, as a `refrouter` 9.0.4 does.
fn check_read_concern(cmd: &Document) -> Result<(), CommandError> {
    let Some(Value::Document(rc)) = cmd.get("readConcern") else { return Ok(()) };
    match rc.get("level") {
        Some(Value::String(level)) if level != MAJORITY => Err(CommandError::new(
            error::INVALID_OPTIONS,
            format!(
                "Command aggregate does not support {{ readConcern: {{ level: \"{level}\", provenance: \"clientSupplied\" }} }} :: caused by :: Aggregation stage $changeStream cannot run with a readConcern other than 'majority'. Current readConcern: {level}"
            ),
        )),
        _ => Ok(()),
    }
}

/// Refuses the internal namespaces.
fn check_namespace(ns: &Namespace, scope: Scope) -> Result<(), CommandError> {
    let internal = |what: String| {
        CommandError::new(
            error::INVALID_NAMESPACE,
            format!("$changeStream may not be opened on the internal {what}"),
        )
    };
    if (ns.db == ADMIN && scope != Scope::Cluster) || ns.db == CONFIG {
        return Err(internal(format!("{} database", ns.db)));
    }
    if scope == Scope::Collection && ns.coll.starts_with(SYSTEM_PREFIX) {
        return Err(internal(format!("{} collection", names::full(ns))));
    }
    Ok(())
}

/// Refuses a view and a time series.
fn check_collection<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
) -> Result<(), CommandError> {
    let Some(c) = backend.collection(ns)? else { return Ok(()) };
    kinds::not_a_view(ns, &c)?;
    if matches!(kinds::kind_of(&c)?, CollectionKind::TimeSeries(_)) {
        return Err(CommandError::new(
            error::COMMAND_NOT_SUPPORTED,
            format!("Cannot run aggregation on timeseries with namespace {}", names::full(ns)),
        ));
    }
    Ok(())
}

/// The stages after `$changeStream`, checked; `$changeStreamSplitLargeEvent` left out, and
/// whether it was there.
fn rest_stages(stages: &[RawValue<'_>]) -> Result<(Vec<Document>, bool), CommandError> {
    let mut names_of = Vec::new();
    let mut out = Vec::new();
    for s in stages.iter().skip(1) {
        let RawValue::Document(d) = s else {
            return Err(CommandError::new(
                error::TYPE_MISMATCH,
                "Each element of the 'pipeline' array must be an object",
            ));
        };
        let Some((name, arg)) = d.iter().next() else {
            return Err(CommandError::new(
                error::STAGE_ONE_FIELD,
                "A pipeline stage specification object must contain exactly one field.",
            ));
        };
        names_of.push(name);
        if name == SPLIT_LARGE_EVENT {
            if !matches!(arg, RawValue::Document(a) if a.iter().next().is_none()) {
                return Err(CommandError::new(
                    error::CS_SPLIT_SPEC,
                    "$changeStreamSplitLargeEvent spec should be an empty object",
                ));
            }
            continue;
        }
        out.push(Document::from(*d));
    }
    check_rest(&names_of).map_err(stage_error)?;
    parse_rest(&out)?;
    Ok((out, names_of.contains(&SPLIT_LARGE_EVENT)))
}

fn parse_rest(stages: &[Document]) -> Result<teseladb_exec::pipeline::Pipeline, CommandError> {
    let bytes: Vec<Vec<u8>> = stages.iter().map(Document::to_bytes).collect::<Result<_, _>>()?;
    let raw: Vec<RawValue<'_>> = bytes
        .iter()
        .map(|b| RawDoc::from_bytes(b).map(RawValue::Document))
        .collect::<Result<_, _>>()?;
    let ctx = teseladb_exec::expr::Ctx::checked(std::iter::empty::<&str>());
    teseladb_exec::pipeline::Pipeline::parse_command(&raw, &ctx, true)
        .map_err(super::mutation::from_mql)
}

/// Opens a change stream: `aggregate` on `ns` (`on_collection` false for `aggregate: 1`) with
/// pipeline `stages`, whose first stage is `$changeStream`.
pub fn open<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    on_collection: bool,
    stages: &[RawValue<'_>],
) -> Result<Document, CommandError> {
    check_read_concern(ctx.cmd)?;
    let ns_text = names::full(ns);
    let scope = match (on_collection, ns.db == ADMIN) {
        (true, _) => Scope::Collection,
        (false, true) => Scope::Cluster,
        (false, false) => Scope::Database,
    };
    let arg = match stages.first() {
        Some(RawValue::Document(d)) => d.get(CHANGE_STREAM).map(Value::from).unwrap_or(Value::Null),
        _ => Value::Null,
    };
    let spec = ChangeStreamSpec::parse(&arg, scope, &ns_text).map_err(stage_error)?;
    // `aggregate: 1` on `admin` watches the cluster only with `allChangesForCluster`.
    let scope = match scope {
        Scope::Cluster if !spec.all_for_cluster => Scope::Database,
        other => other,
    };
    check_namespace(ns, scope)?;
    if scope == Scope::Collection {
        check_collection(&mut *ctx.backend, ns)?;
    }
    let (rest, split) = rest_stages(stages)?;
    let resume_fragment = match &spec.start {
        Start::ResumeAfter(Position::Event(t)) | Start::StartAfter(Position::Event(t)) => {
            t.fragment.map(|f| (t.event.clone(), f))
        }
        _ => None,
    };
    let position = StreamPosition::new(&spec.start, ctx.backend.cluster_mark());
    // A resume point the cluster no longer keeps fails at the first `getMore` (measured).
    let resume_ts = match &spec.start {
        Start::Now => ctx.backend.cluster_mark(),
        Start::At(ts) => *ts,
        Start::ResumeAfter(p) | Start::StartAfter(p) => p.ts(),
    };
    let token = position.token().encode();
    let mut stream = ChangeStream {
        scope,
        db: if scope == Scope::Cluster { String::new() } else { ns.db.clone() },
        coll: on_collection.then(|| ns.coll.clone()),
        spec,
        position,
        pending: VecDeque::new(),
        stages: rest,
        split,
        resume_fragment,
        resume_ts,
        invalidated: None,
    };
    resumed_on_end(&mut stream, ctx.backend.random())?;
    let mut state = QueryCursor::buffered(Vec::new());
    state.stream = Some(Box::new(stream));
    let id = keep(ctx, &ns_text, state, false, false)?;
    let cursor = Document::new()
        .with("firstBatch", Vec::<Value>::new())
        .with("postBatchResumeToken", Document::new().with("_data", token))
        .with("id", id)
        .with("ns", ns_text);
    Ok(Document::new().with("cursor", cursor).with("ok", 1.0))
}

/// A batch of a change stream: its events, its `postBatchResumeToken` and whether the stream
/// ended (an `invalidate`: the cursor closes).
pub type StreamBatch = (Vec<Document>, Document, bool);

/// The next batch of a change stream cursor and its `postBatchResumeToken`: at most `batch`
/// events (`None`: no limit) and 16 MiB, waiting for some up to `max_time` (1 s by default).
/// Like a `refrouter`, a batch with events holds every event written before the `getMore`: it reads
/// on, within the same wait, until the resolved mark passes the node's clock at its start, which
/// goes some 200 ms ahead of it (014 FR-004).
pub fn more<B: Backend + ?Sized>(
    backend: &mut B,
    stream: &mut ChangeStream,
    batch: Option<usize>,
    max_time: Option<u64>,
) -> Result<StreamBatch, CommandError> {
    let deadline = backend.now_ms().saturating_add(max_time.unwrap_or(DEFAULT_AWAIT_MS));
    let started = backend.cluster_mark();
    let mut out = Vec::new();
    let mut last = None;
    loop {
        if stream.pending.is_empty() {
            poll(backend, stream).map_err(resumable)?;
        }
        let mut bytes = 0usize;
        while batch.is_none_or(|b| out.len() < b) {
            let Some((token, doc)) = stream.pending.pop_front() else { break };
            let n = teseladb_changestream::split_large::size(&doc).unwrap_or(usize::MAX);
            if bytes + n > super::query::MAX_BATCH_BYTES && !out.is_empty() {
                stream.pending.push_front((token, doc));
                break;
            }
            bytes += n;
            out.push(doc);
            last = Some(token);
        }
        // Events left waiting: the batch is full (or the stream ended with them still to give).
        let full = !stream.pending.is_empty();
        let ended = stream.invalidated.is_some() && !full;
        let caught_up = !out.is_empty() && stream.position.from() >= started;
        if full || ended || caught_up || backend.now_ms() >= deadline || !backend.pause() {
            break;
        }
    }
    if let Some(end) = stream.invalidated.as_ref().filter(|_| stream.pending.is_empty()) {
        let pbrt = Document::new().with("_data", end.encode());
        return Ok((out, pbrt, true));
    }
    stream.position.batch_ended(last.as_ref(), stream.pending.is_empty());
    let pbrt = Document::new().with("_data", stream.position.token().encode());
    Ok((out, pbrt, false))
}

/// The codes after which a driver resumes the stream (`ResumableChangeStreamError`): the node or
/// the network failed, or a range moved, as RefDb's `isResumableChangeStreamError` lists them.
const RESUMABLE: [i32; 18] = [
    6, 7, 63, 89, 91, 133, 134, 150, 189, 234, 262, 9001, 10107, 11600, 11602, 13388, 13435, 13436,
];
/// The label of an error after which a driver resumes the stream.
pub const RESUMABLE_LABEL: &str = "ResumableChangeStreamError";

/// `e` with `ResumableChangeStreamError` if a driver resumes after it (014 FR-006).
fn resumable(e: CommandError) -> CommandError {
    match RESUMABLE.contains(&e.code.code) && !e.labels.contains(&RESUMABLE_LABEL) {
        true => e.with_label(RESUMABLE_LABEL),
        false => e,
    }
}

/// Refuses a stream that would read from `from` when the ranges no longer keep the versions of
/// that mark: below the wall clock less the history retention (`teselaHistoryRetention`, 24 h by
/// default, D-020) (014 FR-006, FR-007).
fn check_history<B: Backend + ?Sized>(
    backend: &mut B,
    from: Hlc,
    resume: Hlc,
) -> Result<(), CommandError> {
    let params = backend.cluster_parameters()?;
    let stored = params.get(super::parameters::HISTORY_RETENTION).map(Vec::as_slice);
    let retention = super::parameters::history_retention_ms(stored);
    let floor = backend.wall_ms().saturating_sub(retention);
    if from.physical < floor {
        return Err(history_lost(resume));
    }
    Ok(())
}

/// The label of an error after which a driver does not resume the stream.
pub const NON_RESUMABLE_LABEL: &str = "NonResumableChangeStreamError";

/// `ChangeStreamHistoryLost` of a stream resuming at `resume`, as a `refrouter` 9.0.4 answers a
/// `getMore` (measured on 01-11 after the oplog rolled over).
fn history_lost(resume: Hlc) -> CommandError {
    let (time, increment) = teseladb_hlc::to_timestamp(resume);
    CommandError::new(
        error::CHANGE_STREAM_HISTORY_LOST,
        format!(
            "Executor error during getMore :: caused by :: Resume of change stream was not possible, as the resume point may no longer be in the oplog (resumeTimestamp: Timestamp({time}, {increment}))"
        ),
    )
    .with_label(NON_RESUMABLE_LABEL)
}

/// Whether namespace `ns` is one `stream` reads.
fn ns_in_scope(stream: &ChangeStream, ns: &Namespace) -> bool {
    match stream.scope {
        Scope::Collection => ns.db == stream.db && Some(&ns.coll) == stream.coll.as_ref(),
        Scope::Database => ns.db == stream.db && !ns.coll.starts_with(SYSTEM_PREFIX),
        Scope::Cluster => {
            !INTERNAL_DBS.contains(&ns.db.as_str()) && !ns.coll.starts_with(SYSTEM_PREFIX)
        }
    }
}

/// Whether collection `c` has documents a stream reads.
fn has_documents(c: &CollectionEntry) -> bool {
    matches!(kinds::kind_of(c), Ok(CollectionKind::Normal | CollectionKind::Capped { .. }))
}

/// What a read of the stream knows of a collection over the marks it reads: its name now (or
/// when it was dropped), its renames and its drop (014 FR-014). A change of a collection dropped
/// since the stream's mark is still read, up to the drop.
struct Watched {
    ns: Namespace,
    entry: CollectionEntry,
    /// Each rename: where it is in the merge, and the names before and after.
    renames: Vec<(EventKey, Namespace, Namespace)>,
    /// Where its drop is in the merge.
    dropped: Option<EventKey>,
    /// Each `collMod`: where it is in the merge, and the options before it.
    modifies: Vec<(EventKey, Vec<u8>)>,
}

impl Watched {
    /// The collection's name at event `key`.
    fn ns_at(&self, key: &EventKey) -> &Namespace {
        self.renames.iter().find(|(at, ..)| at > key).map_or(&self.ns, |(_, from, _)| from)
    }

    /// Whether the collection kept pre- and post-images (`changeStreamPreAndPostImages`) when
    /// the event at `key` was written (014 FR-008).
    fn images_at(&self, key: &EventKey) -> bool {
        let options = self
            .modifies
            .iter()
            .find(|(at, _)| at > key)
            .map_or(self.entry.options.as_slice(), |(_, before)| before.as_slice());
        images_enabled(options)
    }

    /// Whether any of its names is one `stream` reads.
    fn ever_in(&self, stream: &ChangeStream) -> bool {
        ns_in_scope(stream, &self.ns)
            || self.renames.iter().any(|(_, a, b)| ns_in_scope(stream, a) || ns_in_scope(stream, b))
    }
}

fn entry_key(c: &CollectionEntry) -> (u32, u32) {
    (c.tenant, c.id)
}

/// Whether options `options` keep pre- and post-images.
fn images_enabled(options: &[u8]) -> bool {
    let Ok(o) = RawDoc::from_bytes(options) else { return false };
    match o.get("changeStreamPreAndPostImages") {
        Some(RawValue::Document(d)) => matches!(d.get("enabled"), Some(RawValue::Boolean(true))),
        _ => false,
    }
}

/// The collections a read of `stream` reads: those of the catalog now and those the catalog
/// changes in `records` dropped, with their renames.
fn watched_of(
    stream: &ChangeStream,
    now: Vec<(Namespace, CollectionEntry)>,
    records: &[DdlRecord],
) -> Vec<Watched> {
    let mut by_id: BTreeMap<(u32, u32), Watched> = now
        .into_iter()
        .map(|(ns, entry)| {
            let w = Watched { ns, entry, renames: Vec::new(), dropped: None, modifies: Vec::new() };
            ((entry_key(&w.entry)), w)
        })
        .collect();
    let mut add = |ns: &Namespace, c: &CollectionEntry| {
        by_id.entry((c.tenant, c.id)).or_insert_with(|| Watched {
            ns: ns.clone(),
            entry: c.clone(),
            renames: Vec::new(),
            dropped: None,
            modifies: Vec::new(),
        });
        (c.tenant, c.id)
    };
    let mut drops = Vec::new();
    let mut renames = Vec::new();
    let mut modifies = Vec::new();
    for r in records {
        let key = ddl_key(r.mark, r.op);
        match &r.change {
            DdlChange::Drop { ns, collection } => {
                drops.push((add(ns, collection), key, ns.clone()))
            }
            DdlChange::Rename { from, to, collection, target } => {
                if let Some(t) = target {
                    drops.push((add(to, t), key.clone(), to.clone()));
                }
                renames.push((add(to, collection), key, from.clone(), to.clone()));
            }
            DdlChange::Modify { ns, collection, .. } => {
                modifies.push((add(ns, collection), key, collection.options.clone()));
            }
            _ => {}
        }
    }
    for (id, key, before) in modifies {
        if let Some(w) = by_id.get_mut(&id) {
            w.modifies.push((key, before));
        }
    }
    // A dropped collection's name is the one it had when it was dropped.
    for (id, key, ns) in drops {
        if let Some(w) = by_id.get_mut(&id) {
            w.dropped = Some(key);
            w.ns = ns;
        }
    }
    for (id, key, from, to) in renames {
        if let Some(w) = by_id.get_mut(&id) {
            w.renames.push((key, from, to));
        }
    }
    by_id.into_values().filter(|w| has_documents(&w.entry) && w.ever_in(stream)).collect()
}

/// Reads the changes after the stream's mark into its pending events: the documents' changes
/// and the catalog's, merged by their place, up to the lowest resolved mark; an `invalidate`
/// ends the read (014 FR-005, FR-014).
fn poll<B: Backend + ?Sized>(
    backend: &mut B,
    stream: &mut ChangeStream,
) -> Result<(), CommandError> {
    if stream.invalidated.is_some() {
        return Ok(());
    }
    let from = stream.position.from();
    let resume = stream.resume_ts.max(from);
    check_history(backend, from, resume)?;
    // The catalog changes first: a collection of the catalog read after them was created at or
    // below their mark, or its changes come above it.
    let ddl = backend.catalog_changes(from)?;
    if from < ddl.floor {
        return Err(history_lost(resume));
    }
    let tenant = backend.tenant();
    let now: Vec<(Namespace, CollectionEntry)> =
        backend.collections()?.into_iter().filter(|(_, c)| c.tenant == tenant).collect();
    let records: Vec<DdlRecord> =
        ddl.records.into_iter().filter(|r| r.mark <= ddl.resolved).collect();
    let colls = watched_of(stream, now, &records);
    let mut resolved = ddl.resolved;
    let mut rows = Vec::new();
    for w in &colls {
        let (start, end) = documents_of(&w.entry);
        let (more, mark) = backend.changes(&start, &end, from)?;
        resolved = resolved.min(mark);
        rows.extend(more);
    }
    if resolved <= from {
        return Ok(());
    }
    let by_id: BTreeMap<(u32, u32), &Watched> =
        colls.iter().map(|w| ((w.entry.tenant, w.entry.id), w)).collect();
    let versions = rows.into_iter().filter(|r| r.ts <= resolved).map(|r| Versioned {
        key: r.key,
        ts: r.ts,
        value: r.value,
    });
    let found = changes(versions).map_err(|e| {
        CommandError::new(error::INTERNAL_ERROR, format!("a change stream read a bad meta: {e:?}"))
    })?;
    let mut events: Vec<(ChangeEvent, bool)> = Vec::new();
    // The document each key's last change left, for the pre-image of a later write of the same
    // transaction.
    let mut left: BTreeMap<&[u8], (&EventKey, Option<&Vec<u8>>)> = BTreeMap::new();
    for change in &found {
        let key = change.event.key.as_slice();
        let prior = left.insert(key, (&change.event, change.doc.as_ref()));
        let Some(w) = collection_of(key).and_then(|id| by_id.get(&id)) else {
            continue;
        };
        let ns = w.ns_at(&change.event);
        if w.dropped.as_ref().is_some_and(|d| change.event > *d) || !ns_in_scope(stream, ns) {
            continue;
        }
        let same_txn = prior.filter(|(at, _)| {
            at.ts == change.event.ts && at.txn == change.event.txn && at.txn != 0
        });
        let images = Images { on: w.images_at(&change.event), prior: same_txn.map(|(_, d)| d) };
        events.push((event_of(backend, stream, change, ns, w.entry.uuid, images)?, false));
    }
    let watch = Watch {
        scope: stream.scope,
        db: &stream.db,
        coll: stream.coll.as_deref(),
        expanded: stream.spec.expanded,
    };
    for r in records.iter().filter(|r| r.mark <= resolved) {
        let ends = super::change_stream_ddl::ends(&watch, r);
        match super::change_stream_ddl::event_of(&watch, r)? {
            Some(e) => events.push((e, ends)),
            None if ends => {
                let token = ResumeToken::new(ddl_key(r.mark, r.op), EventKind::Drop);
                events.push((invalidate_event(&token), true));
            }
            None => {}
        }
    }
    events.sort_by(|a, b| a.0.token.event.cmp(&b.0.token.event));
    for (event, ends) in events {
        let resumes_inside =
            stream.resume_fragment.as_ref().is_some_and(|(at, _)| *at == event.token.event);
        if !stream.position.wants(&event.token.event) && !resumes_inside {
            continue;
        }
        if event.kind() != EventKind::Invalidate {
            let doc = event.to_document(stream.spec.expanded);
            for out in run_stages(stream, doc, backend.random())? {
                queue(stream, &event.token, out)?;
            }
        }
        if ends {
            let ts = event.token.event.ts;
            push_invalidate(stream, &event.token, backend.random())?;
            stream.position.read_to(below(ts).unwrap_or(ts));
            return Ok(());
        }
    }
    stream.position.read_to(resolved);
    Ok(())
}

/// Queues event `doc` of token `token`: in fragments with `$changeStreamSplitLargeEvent`, after
/// the fragment the stream resumed from (014 FR-011); without it, an event too large to
/// serialize fails as on a `refrouter` 9.0.4 (measured on 01-11).
fn queue(
    stream: &mut ChangeStream,
    token: &ResumeToken,
    doc: Document,
) -> Result<(), CommandError> {
    use teseladb_changestream::split_large::{MAX_SERIALIZED_BYTES, SplitError, size, split};
    let too_large = |n: usize| {
        CommandError::new(
            error::BSON_OBJECT_TOO_LARGE,
            format!(
                "Executor error during getMore :: caused by :: Serializing Document failed :: caused by :: Size {n} exceeds maximum {MAX_SERIALIZED_BYTES}"
            ),
        )
    };
    if !stream.split {
        let n = size(&doc)?;
        if n > MAX_SERIALIZED_BYTES {
            return Err(too_large(n));
        }
        stream.pending.push_back((token.clone(), doc));
        return Ok(());
    }
    let parts = split(&doc, token).map_err(|e| match e {
        SplitError::Bson(e) => CommandError::from(e),
        SplitError::FieldTooLarge(n) => too_large(n),
    })?;
    let done = match stream.resume_fragment.take() {
        Some((at, f)) if at == token.event => f,
        other => {
            stream.resume_fragment = other;
            0
        }
    };
    for (t, d) in parts.into_iter().filter(|(t, _)| t.fragment.is_none_or(|n| n > done)) {
        stream.pending.push_back((t, d));
    }
    Ok(())
}

/// The `invalidate` after the event of `token`.
fn invalidate_event(token: &ResumeToken) -> ChangeEvent {
    let inv = invalidate_of(token);
    ChangeEvent::catalog(inv, EventNs { db: String::new(), coll: None })
}

/// Ends `stream` with the `invalidate` after the event of `token`: the stages after
/// `$changeStream` may filter it out, and the cursor closes all the same (measured on 01-11).
fn push_invalidate(
    stream: &mut ChangeStream,
    token: &ResumeToken,
    seed: u64,
) -> Result<(), CommandError> {
    let event = invalidate_event(token);
    let doc = event.to_document(stream.spec.expanded);
    for out in run_stages(stream, doc, seed)? {
        queue(stream, &event.token, out)?;
    }
    stream.invalidated = Some(event.token);
    Ok(())
}

/// A stream resumed after the `drop` or `rename` of its collection, or the `dropDatabase` of its
/// database, starts with the `invalidate` after it (measured on 01-11).
fn resumed_on_end(stream: &mut ChangeStream, seed: u64) -> Result<(), CommandError> {
    let (Start::ResumeAfter(Position::Event(t)) | Start::StartAfter(Position::Event(t))) =
        &stream.spec.start
    else {
        return Ok(());
    };
    let ends = match stream.scope {
        Scope::Collection => matches!(t.kind, EventKind::Drop | EventKind::Rename),
        Scope::Database => t.kind == EventKind::DropDatabase,
        Scope::Cluster => false,
    };
    if ends {
        let t = t.clone();
        push_invalidate(stream, &t, seed)?;
    }
    Ok(())
}

/// What the images of an event come from: whether its collection kept them when it was written,
/// and, for a later write of a document in the same transaction, the document the write before
/// it left.
struct Images<'a> {
    on: bool,
    prior: Option<Option<&'a Vec<u8>>>,
}

/// The event of `change` of collection `ns`, with the images its stream asks for (014 FR-008):
/// the pre-image is the version before the change and the post-image the change's document,
/// when the collection kept images at the write; otherwise `null`, or `NoMatchingDocument` with
/// `required` (measured on 01-11).
fn event_of<B: Backend + ?Sized>(
    backend: &mut B,
    stream: &ChangeStream,
    change: &Change,
    ns: &Namespace,
    uuid: [u8; 16],
    images: Images<'_>,
) -> Result<ChangeEvent, CommandError> {
    let key = &change.event.key;
    let earlier = match images.prior {
        Some(doc) => doc.cloned(),
        None if change.doc.is_some() && stream.spec.before == BeforeChange::Off => None,
        None if change.kind == EventKind::Insert => None,
        None => previous(backend, key, change.event.ts)?,
    };
    let doc_bytes = match (&change.doc, &earlier) {
        (Some(d), _) | (None, Some(d)) => d.clone(),
        (None, None) => {
            return Err(CommandError::new(
                error::INTERNAL_ERROR,
                "a deleted document's version is gone",
            ));
        }
    };
    let id = RawDoc::from_bytes(&doc_bytes)?.get("_id").map(Value::from).unwrap_or(Value::Null);
    let document_key = Document::new().with("_id", id);
    let event_ns = EventNs { db: ns.db.clone(), coll: Some(ns.coll.clone()) };
    let mut event = ChangeEvent::of(change, event_ns, Some(document_key), stream.spec.expanded)?;
    event.collection_uuid = Some(uuid);
    let as_value = |d: Option<&Vec<u8>>| -> Result<Value, CommandError> {
        match d {
            Some(b) => Ok(Value::Document(Document::from(RawDoc::from_bytes(b)?))),
            None => Ok(Value::Null),
        }
    };
    if change.kind == EventKind::Update {
        event.full_document = match stream.spec.full_document {
            FullDocument::Default => None,
            FullDocument::UpdateLookup => Some(current(backend, key)?),
            FullDocument::WhenAvailable | FullDocument::Required if images.on => {
                Some(as_value(change.doc.as_ref())?)
            }
            FullDocument::WhenAvailable => Some(Value::Null),
            FullDocument::Required => return Err(no_image(&event, "post", "update")),
        };
    }
    if change.kind != EventKind::Insert {
        event.before = match stream.spec.before {
            BeforeChange::Off => None,
            BeforeChange::WhenAvailable | BeforeChange::Required
                if images.on && earlier.is_some() =>
            {
                Some(as_value(earlier.as_ref())?)
            }
            BeforeChange::WhenAvailable => Some(Value::Null),
            BeforeChange::Required => {
                return Err(no_image(&event, "pre", "update, delete and replace"));
            }
        };
    }
    Ok(event)
}

/// The error of a stream that requires an image its event has not (measured on 01-11).
fn no_image(event: &ChangeEvent, which: &str, kinds: &str) -> CommandError {
    let (time, increment) = teseladb_hlc::to_timestamp(event.cluster_time);
    let coll = event.ns.coll.as_deref().unwrap_or_default();
    CommandError::new(
        error::NO_MATCHING_DOCUMENT,
        format!(
            "Executor error during getMore :: caused by :: Change stream was configured to require a {which}-image for all {kinds} events, but the {which}-image was not found for event: {{operationType: \"{}\", ns: {{db: \"{}\", coll: \"{coll}\"}}, clusterTime: Timestamp({time}, {increment})}}",
            teseladb_changestream::event::operation_type(event.kind()),
            event.ns.db,
        ),
    )
}

/// The version of `key` just below mark `ts`: the document a change at `ts` replaced, if any.
fn previous<B: Backend + ?Sized>(
    backend: &mut B,
    key: &[u8],
    ts: Hlc,
) -> Result<Option<Vec<u8>>, CommandError> {
    let Some(at) = below(ts) else { return Ok(None) };
    let view = teseladb_range::push::TxnView::default();
    match backend.txn_get(key, at, &view, None)? {
        Some((teseladb_changestream::DOCUMENT_VALUE, bytes)) => Ok(Some(bytes)),
        _ => Ok(None),
    }
}

/// The document at `key` now (`fullDocument: "updateLookup"`), or `null`.
fn current<B: Backend + ?Sized>(backend: &mut B, key: &[u8]) -> Result<Value, CommandError> {
    let mut end = key.to_vec();
    end.push(0);
    let rows = backend.scan(key, &end, 1, ReadConcern::Majority)?;
    match rows.into_iter().find(|(k, _)| k == key) {
        Some((_, bytes)) => Ok(Value::Document(Document::from(RawDoc::from_bytes(&bytes)?))),
        None => Ok(Value::Null),
    }
}

/// Runs the stages after `$changeStream` on event `doc`; an output whose `_id` changed is
/// `ChangeStreamFatalError`.
fn run_stages(
    stream: &ChangeStream,
    doc: Document,
    seed: u64,
) -> Result<Vec<Document>, CommandError> {
    if stream.stages.is_empty() {
        return Ok(vec![doc]);
    }
    let pipeline = parse_rest(&stream.stages)?;
    let env = EventEnv { seed: std::cell::Cell::new(seed) };
    let id = doc.get("_id").cloned();
    let out = pipeline.run_in(vec![doc], &env, true).map_err(|e| {
        let mut e = super::mutation::from_mql(e);
        e.errmsg = format!("Executor error during getMore :: caused by :: {}", e.errmsg);
        e
    })?;
    for d in &out {
        if d.get("_id") != id.as_ref() {
            return Err(fatal_id(id.as_ref(), d.get("_id")));
        }
    }
    Ok(out)
}

/// What the stages after `$changeStream` read besides the event: nothing but `$rand`'s numbers
/// (the stages that read a collection cannot follow `$changeStream`).
struct EventEnv {
    seed: std::cell::Cell<u64>,
}

impl teseladb_exec::pipeline::Env for EventEnv {
    fn ext(&self) -> &dyn teseladb_mql::Extensions {
        &super::engines::NodeExtensions
    }

    fn vars(&self) -> Option<&Document> {
        None
    }

    fn collection(&self, name: &str) -> Result<Vec<Document>, teseladb_mql::MqlError> {
        Err(teseladb_mql::MqlError::new(
            teseladb_mql::ErrorCode { code: 20, name: "IllegalOperation" },
            format!("{name} cannot be read in a $changeStream pipeline"),
        ))
    }

    fn random(&self) -> f64 {
        let s = self.seed.get().wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        self.seed.set(s);
        (s >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// The error of an event whose `_id` the stages changed.
fn fatal_id(expected: Option<&Value>, found: Option<&Value>) -> CommandError {
    let show = |v: Option<&Value>| match v {
        None => String::from("{}"),
        Some(v) => format!("{{ _id: {} }}", shown(v)),
    };
    CommandError::new(
        error::CHANGE_STREAM_FATAL,
        format!(
            "Executor error during getMore :: caused by :: Encountered an event whose _id field, which contains the resume token, was modified by the pipeline. Modifying the _id field of an event makes it impossible to resume the stream from that point. Only transformations that retain the unmodified _id field are allowed. Expected: {} but found: {}",
            show(expected),
            show(found)
        ),
    )
}

/// `v` as the message writes it, with long strings cut.
fn shown(v: &Value) -> String {
    let cut = |v: &Value| match v {
        Value::String(s) if s.len() > SHOWN_STRING => {
            Value::String(format!("{}...", &s[..SHOWN_STRING]))
        }
        other => other.clone(),
    };
    let v = match v {
        Value::Document(d) => {
            Value::Document(d.iter().fold(Document::new(), |acc, (k, v)| acc.with(k, cut(v))))
        }
        other => cut(other),
    };
    let wrapped = Document::new().with("v", v);
    match wrapped.to_bytes() {
        Ok(bytes) => RawDoc::from_bytes(&bytes)
            .ok()
            .and_then(|d| d.get("v"))
            .map_or_else(String::new, teseladb_bson::shell::shell_value),
        Err(_) => String::new(),
    }
}
