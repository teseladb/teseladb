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
use teseladb_changestream::event::{ChangeEvent, Namespace as EventNs};
use teseladb_changestream::resolved::below;
use teseladb_changestream::stage::{
    CHANGE_STREAM, ChangeStreamSpec, FullDocument, SPLIT_LARGE_EVENT, Scope, StageError, check_rest,
};
use teseladb_changestream::token::{EventKind, ResumeToken};
use teseladb_hlc::Hlc;
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_meta::collection_types::CollectionKind;
use teseladb_range::read::ReadConcern;

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
const INTERNAL_DBS: [&str; 3] = [ADMIN, CONFIG, "local"];
/// The prefix of a system collection.
const SYSTEM_PREFIX: &str = "system.";
/// The read concern a change stream accepts.
const MAJORITY: &str = "majority";
/// Characters of a string a fatal error's message shows before `...` (measured).
const SHOWN_STRING: usize = 150;
/// How far behind the wall clock a write to a collection created after a read of the catalog
/// may be: the clock offset two nodes may have between them, twice.
const NEW_COLLECTION_LAG_MS: u64 = 2 * teseladb_hlc::MAX_CLOCK_OFFSET_MS.unsigned_abs();

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

/// The stages after `$changeStream`, checked; `$changeStreamSplitLargeEvent` left out.
fn rest_stages(stages: &[RawValue<'_>]) -> Result<Vec<Document>, CommandError> {
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
    Ok(out)
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
    let rest = rest_stages(stages)?;
    let position = StreamPosition::new(&spec.start, ctx.backend.cluster_mark());
    check_history(&mut *ctx.backend, position.from())?;
    let token = position.token().encode();
    let stream = ChangeStream {
        scope,
        db: if scope == Scope::Cluster { String::new() } else { ns.db.clone() },
        coll: on_collection.then(|| ns.coll.clone()),
        spec,
        position,
        pending: VecDeque::new(),
        stages: rest,
    };
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

/// The next batch of a change stream cursor and its `postBatchResumeToken`: at most `batch`
/// events (`None`: no limit) and 16 MiB, waiting for some up to `max_time` (1 s by default).
pub fn more<B: Backend + ?Sized>(
    backend: &mut B,
    stream: &mut ChangeStream,
    batch: Option<usize>,
    max_time: Option<u64>,
) -> Result<(Vec<Document>, Document), CommandError> {
    let deadline = backend.now_ms().saturating_add(max_time.unwrap_or(DEFAULT_AWAIT_MS));
    let mut out = Vec::new();
    let mut last = None;
    loop {
        if stream.pending.is_empty() {
            poll(backend, stream).map_err(resumable)?;
        }
        let mut bytes = 0usize;
        while batch.is_none_or(|b| out.len() < b) {
            let Some((token, doc)) = stream.pending.pop_front() else { break };
            let n = doc.to_bytes().map_or(0, |b| b.len());
            if bytes + n > super::query::MAX_BATCH_BYTES && !out.is_empty() {
                stream.pending.push_front((token, doc));
                break;
            }
            bytes += n;
            out.push(doc);
            last = Some(token);
        }
        if !out.is_empty() || backend.now_ms() >= deadline || !backend.pause() {
            break;
        }
    }
    stream.position.batch_ended(last.as_ref(), stream.pending.is_empty());
    let pbrt = Document::new().with("_data", stream.position.token().encode());
    Ok((out, pbrt))
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
fn check_history<B: Backend + ?Sized>(backend: &mut B, from: Hlc) -> Result<(), CommandError> {
    let params = backend.cluster_parameters()?;
    let stored = params.get(super::parameters::HISTORY_RETENTION).map(Vec::as_slice);
    let retention = super::parameters::history_retention_ms(stored);
    let floor = backend.wall_ms().saturating_sub(retention);
    if from.physical < floor {
        return Err(CommandError::new(
            error::CHANGE_STREAM_HISTORY_LOST,
            "Resume of change stream was not possible, as the resume point may no longer be in the oplog.",
        ));
    }
    Ok(())
}

/// Whether collection `ns` (`c`) is read by `stream`.
fn in_scope(stream: &ChangeStream, ns: &Namespace, c: &CollectionEntry) -> bool {
    let wanted = match stream.scope {
        Scope::Collection => ns.db == stream.db && Some(&ns.coll) == stream.coll.as_ref(),
        Scope::Database => ns.db == stream.db && !ns.coll.starts_with(SYSTEM_PREFIX),
        Scope::Cluster => {
            !INTERNAL_DBS.contains(&ns.db.as_str()) && !ns.coll.starts_with(SYSTEM_PREFIX)
        }
    };
    wanted
        && matches!(kinds::kind_of(c), Ok(CollectionKind::Normal | CollectionKind::Capped { .. }))
}

/// Reads the changes after the stream's mark into its pending events.
fn poll<B: Backend + ?Sized>(
    backend: &mut B,
    stream: &mut ChangeStream,
) -> Result<(), CommandError> {
    let from = stream.position.from();
    check_history(backend, from)?;
    // A collection created after this read of the catalog gets its writes above this mark.
    let wall =
        Hlc { physical: backend.wall_ms().saturating_sub(NEW_COLLECTION_LAG_MS), logical: 0 };
    let tenant = backend.tenant();
    let colls: Vec<(Namespace, CollectionEntry)> = backend
        .collections()?
        .into_iter()
        .filter(|(ns, c)| c.tenant == tenant && in_scope(stream, ns, c))
        .collect();
    let mut resolved = match stream.scope {
        Scope::Collection if !colls.is_empty() => None,
        _ => Some(wall),
    };
    let mut rows = Vec::new();
    for (_, c) in &colls {
        let (start, end) = documents_of(c);
        let (more, mark) = backend.changes(&start, &end, from)?;
        resolved = Some(resolved.map_or(mark, |r: Hlc| r.min(mark)));
        rows.extend(more);
    }
    let Some(resolved) = resolved.filter(|r| *r > from) else { return Ok(()) };
    let by_id: BTreeMap<(u32, u32), (Namespace, [u8; 16])> =
        colls.into_iter().map(|(ns, c)| ((c.tenant, c.id), (ns, c.uuid))).collect();
    let versions = rows.into_iter().filter(|r| r.ts <= resolved).map(|r| Versioned {
        key: r.key,
        ts: r.ts,
        value: r.value,
    });
    let found = changes(versions).map_err(|e| {
        CommandError::new(error::INTERNAL_ERROR, format!("a change stream read a bad meta: {e:?}"))
    })?;
    for change in found.iter().filter(|c| stream.position.wants(&c.event)) {
        let Some((ns, uuid)) = collection_of(&change.event.key).and_then(|id| by_id.get(&id))
        else {
            continue;
        };
        let event = event_of(backend, stream, change, ns, *uuid)?;
        let token = event.token.clone();
        let doc = event.to_document(stream.spec.expanded);
        for out in run_stages(stream, doc, backend.random())? {
            stream.pending.push_back((token.clone(), out));
        }
    }
    stream.position.read_to(resolved);
    Ok(())
}

/// The event of `change` of collection `ns`.
fn event_of<B: Backend + ?Sized>(
    backend: &mut B,
    stream: &ChangeStream,
    change: &Change,
    ns: &Namespace,
    uuid: [u8; 16],
) -> Result<ChangeEvent, CommandError> {
    let key = &change.event.key;
    let doc_bytes = match &change.doc {
        Some(d) => d.clone(),
        None => deleted(backend, key, change.event.ts)?,
    };
    let id = RawDoc::from_bytes(&doc_bytes)?.get("_id").map(Value::from).unwrap_or(Value::Null);
    let document_key = Document::new().with("_id", id);
    let event_ns = EventNs { db: ns.db.clone(), coll: Some(ns.coll.clone()) };
    let mut event = ChangeEvent::of(change, event_ns, Some(document_key), stream.spec.expanded)?;
    event.collection_uuid = Some(uuid);
    if change.kind == EventKind::Update && stream.spec.full_document == FullDocument::UpdateLookup {
        event.full_document = Some(current(backend, key)?);
    }
    Ok(event)
}

/// The document a deletion at `ts` removed: its version just below the deletion.
fn deleted<B: Backend + ?Sized>(
    backend: &mut B,
    key: &[u8],
    ts: Hlc,
) -> Result<Vec<u8>, CommandError> {
    let gone = || CommandError::new(error::INTERNAL_ERROR, "a deleted document's version is gone");
    let at = below(ts).ok_or_else(gone)?;
    let view = teseladb_range::push::TxnView::default();
    match backend.txn_get(key, at, &view, None)? {
        Some((teseladb_changestream::DOCUMENT_VALUE, bytes)) => Ok(bytes),
        _ => Err(gone()),
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
