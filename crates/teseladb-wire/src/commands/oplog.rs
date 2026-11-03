//! `local.oplog.rs` (018 FR-009, contract `oplog-format.md`): a read-only collection whose
//! entries come from the versions of the documents and their HISTORY metas (D-114, D-118), the
//! catalog changes Group 0 recorded (014 FR-014) and the notes of `appendOplogNote`, in commit
//! order, over the history window of 014.
//!
//! - A document's change is an `i`, `u` or `d` entry; an update by operators carries
//!   `{$v: 2, diff}` (`teseladb_topology::diff`), built from the version before it.
//! - The writes of a client transaction go in `applyOps` entries on `admin.$cmd` with the
//!   transaction's `lsid` and `txnNumber` (`teseladb_topology::oplog::transaction_entries`).
//! - A document that moved with its shard key (D-118) is one `u` entry keyed by the version it
//!   left; its deletion at the old key gives none, as for change streams.
//! - The catalog changes are `c` entries on `<db>.$cmd`; the ones of resharding, balancing and
//!   shard keys, which a replica set does not have, give none.
//! - `ts` is the commit mark (005 FR-024) and `t` the presentation's term
//!   ([`super::presentation`]); `wall` is the mark's milliseconds.

use std::collections::BTreeMap;

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_changestream::changes::{Change, Versioned, changes, collection_of, gives_event};
use teseladb_changestream::meta::{Moved, RetryInfo, lsid_document};
use teseladb_changestream::token::EventKind;
use teseladb_hlc::Hlc;
use teseladb_hlc::{from_timestamp, to_timestamp};
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_meta::command::SettingChange;
use teseladb_meta::ddl_log::{DdlChange, DdlRecord};
use teseladb_topology::oplog::{
    APPLY_OPS_NS, DROP_REFUSED, DROP_REFUSED_CODE, ENTRY_VERSION, NULL_OP_TIME, OpType, Operation,
    OplogEntry, TxnInfo, WRITE_REFUSED, transaction_entries,
};
use teseladb_topology::replset::{OpTime, Timestamp};

use super::Backend;
use super::change_stream::{Watched, history_floor, previous, watched_all};
use super::query::documents_of;
use super::txn_ddl::is_scratch;
use crate::error::{Code, CommandError, INTERNAL_ERROR, INVALID_NAMESPACE};

/// The mark before every entry.
const START: Hlc = Hlc { physical: 0, logical: 0 };

/// The room between the positions of two catalog changes of one Group 0 entry, for the
/// entries one change gives ([`scratch_creation`]).
const SEQ_STRIDE: u32 = 1 << 10;

/// The longest a read that is not tailable waits for the ranges to resolve the node's clock.
pub const CATCH_UP_MS: u64 = 5_000;

/// How far back each step of a read of the newest entries goes: what a range answers at a time
/// (`teseladb_range::host::feed::FEED_WINDOW_MS`).
const BACK_WINDOW_MS: u64 = 1_000;

/// The oplog's collection.
pub const OPLOG_COLL: &str = "oplog.rs";
/// The prefix of the Group 0 settings that keep the notes of `appendOplogNote`, followed by the
/// note's mark (physical and logical parts, big-endian).
pub const NOTE_PREFIX: &[u8] = b"oplogNote\0";
/// `drop` of the oplog (RefDb 9.0.4).
const CANNOT_DROP_LIVE_OPLOG: Code = Code { code: DROP_REFUSED_CODE, name: "Location5255000" };
/// The `_id` index of a collection, as `create` entries carry it.
fn id_index() -> Document {
    Document::new().with("v", 2).with("key", Document::new().with("_id", 1)).with("name", "_id_")
}

/// Whether `ns` is the oplog.
pub fn is_oplog(ns: &Namespace) -> bool {
    ns.db == super::names::LOCAL_DB && ns.coll == OPLOG_COLL
}

/// The error of an insert, update, delete or index build on the oplog.
pub fn write_refused() -> CommandError {
    CommandError::new(INVALID_NAMESPACE, WRITE_REFUSED)
}

/// The error of `drop` on the oplog.
pub fn drop_refused() -> CommandError {
    CommandError::new(CANNOT_DROP_LIVE_OPLOG, DROP_REFUSED)
}

/// An entry with its place in commit order.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// The commit mark.
    pub mark: Hlc,
    /// Its position among the entries of that mark.
    pub seq: u32,
    /// The entry.
    pub doc: Document,
}

impl Item {
    /// Its place as bytes that sort in commit order (a tailable cursor's position).
    pub fn key(&self) -> Vec<u8> {
        let mut k = Vec::with_capacity(16);
        k.extend_from_slice(&self.mark.physical.to_be_bytes());
        k.extend_from_slice(&self.mark.logical.to_be_bytes());
        k.extend_from_slice(&self.seq.to_be_bytes());
        k
    }
}

/// What a read of the oplog found after a mark.
#[derive(Clone, Debug, PartialEq)]
pub struct OplogRead {
    /// The entries, in commit order.
    pub items: Vec<Item>,
    /// No entry at or below this mark appears later.
    pub resolved: Hlc,
}

/// The note setting's key at `mark`.
fn note_key(mark: Hlc) -> Vec<u8> {
    let mut k = NOTE_PREFIX.to_vec();
    k.extend_from_slice(&mark.physical.to_be_bytes());
    k.extend_from_slice(&mark.logical.to_be_bytes());
    k
}

/// The mark of note setting `key`.
fn note_mark(key: &[u8]) -> Option<Hlc> {
    let rest = key.strip_prefix(NOTE_PREFIX)?;
    let physical = u64::from_be_bytes(rest.get(..8)?.try_into().ok()?);
    let logical = u32::from_be_bytes(rest.get(8..12)?.try_into().ok()?);
    Some(Hlc { physical, logical })
}

/// Bounded retries of a note whose mark another note took.
const NOTE_ATTEMPTS: u32 = 8;

/// Records note `data` (`appendOplogNote`) at the node's mark and forgets the notes below the
/// history window.
pub fn append_note<B: Backend + ?Sized>(
    backend: &mut B,
    data: &Document,
) -> Result<(), CommandError> {
    let value = data.to_bytes()?;
    let floor = history_floor(backend)?;
    let old: Vec<SettingChange> = backend
        .group0_settings(NOTE_PREFIX)?
        .into_iter()
        .filter(|(k, _)| note_mark(k).is_some_and(|m| m.physical < floor))
        .map(|(key, v)| SettingChange { key, expect: Some(v), value: None })
        .collect();
    let mut mark = backend.cluster_mark();
    for _ in 0..NOTE_ATTEMPTS {
        let mut writes = old.clone();
        writes.push(SettingChange {
            key: note_key(mark),
            expect: None,
            value: Some(value.clone()),
        });
        if backend.group0_settings_write(writes)? {
            return Ok(());
        }
        mark = Hlc { physical: mark.physical, logical: mark.logical.saturating_add(1) };
    }
    Err(CommandError::new(INTERNAL_ERROR, "appendOplogNote found no free mark"))
}

/// The optime of mark `mark` in term `term`.
fn op_time(mark: Hlc, term: i64) -> OpTime {
    let (time, increment) = to_timestamp(mark);
    OpTime { ts: Timestamp { time, increment }, term }
}

/// A plain entry (not of a transaction) at `mark`.
fn plain(operation: Operation, extra: Document, mark: Hlc, term: i64) -> Document {
    OplogEntry {
        txn: None,
        operation,
        retry_image: None,
        stmt_id: None,
        extra,
        op_time: Some(op_time(mark, term)),
        version: ENTRY_VERSION,
        wall_ms: Some(wall(mark)),
        prev_op_time: None,
        multi_op_type: None,
    }
    .to_document()
}

/// The entries of retryable write `r` at `mark` (018 FR-009), as a replica set member of RefDb
/// 9.0.4 writes them (measured on 02-11-2026): one document is a plain entry with the session,
/// several are one `applyOps` entry on `admin.$cmd` with `multiOpType: 1`, each operation with
/// its `stmtId`; `prevOpTime` is the command's previous statement, or null for the first.
fn retryable(r: &RetryInfo, ops: Vec<(u32, Operation)>, mark: Hlc, term: i64) -> Document {
    let stmt = |op: u32| i32::try_from(r.stmt_id.saturating_add(op)).unwrap_or(i32::MAX);
    let prev = r.prev.map_or(NULL_OP_TIME, |p| op_time(p, term));
    let txn = Some(TxnInfo { lsid: lsid_document(r.lsid, &r.uid), txn_number: r.txn_number });
    let entry = |operation, stmt_id: Option<i32>, multi_op_type| OplogEntry {
        txn: txn.clone(),
        operation,
        retry_image: r.image_name().map(str::to_owned).filter(|_| stmt_id.is_some()),
        stmt_id,
        extra: Document::new(),
        op_time: Some(op_time(mark, term)),
        version: ENTRY_VERSION,
        wall_ms: Some(wall(mark)),
        prev_op_time: Some(prev),
        multi_op_type,
    };
    if let [(op, one)] = ops.as_slice() {
        return entry(one.clone(), Some(stmt(*op)), None).to_document();
    }
    let applied: Vec<Value> = ops
        .into_iter()
        .map(|(op, o)| Value::Document(o.to_document().with("stmtId", stmt(op))))
        .collect();
    let o = Document::new().with("applyOps", applied);
    let batch = Operation::new(OpType::Command, APPLY_OPS_NS, None, o, None);
    entry(batch, None, Some(1)).to_document()
}

/// The entries of the operations of one write at `mark` that is neither a transaction nor a
/// retryable write: the inserts of several documents in a row are one `applyOps` entry on
/// `admin.$cmd`, without session, `stmtId` or `multiOpType`; every other operation is an entry
/// of its own (measured on a replica set of `ref:9.0.4`, 03-11-2026).
fn plain_entries(ops: Vec<Operation>, mark: Hlc, term: i64) -> Vec<Document> {
    let mut out = Vec::new();
    let mut inserts: Vec<Operation> = Vec::new();
    let flush = |inserts: &mut Vec<Operation>, out: &mut Vec<Document>| match inserts.len() {
        0 => {}
        1 => out.push(plain(inserts.remove(0), Document::new(), mark, term)),
        _ => {
            let applied: Vec<Value> =
                inserts.drain(..).map(|o| Value::Document(o.to_document())).collect();
            let o = Document::new().with("applyOps", applied);
            let batch = Operation::new(OpType::Command, APPLY_OPS_NS, None, o, None);
            out.push(plain(batch, Document::new(), mark, term));
        }
    };
    for op in ops {
        if op.op == OpType::Insert {
            inserts.push(op);
            continue;
        }
        flush(&mut inserts, &mut out);
        out.push(plain(op, Document::new(), mark, term));
    }
    flush(&mut inserts, &mut out);
    out
}

fn wall(mark: Hlc) -> i64 {
    i64::try_from(mark.physical).unwrap_or(i64::MAX)
}

fn doc_of(bytes: &[u8]) -> Result<Document, CommandError> {
    Ok(Document::from(RawDoc::from_bytes(bytes)?))
}

/// `db.$cmd`.
fn cmd_ns(db: &str) -> String {
    format!("{db}.$cmd")
}

fn full(ns: &Namespace) -> String {
    format!("{}.{}", ns.db, ns.coll)
}

/// The options of a collection as `create` gives them: without TeselaDB's own (`$…`).
fn create_options(entry: &CollectionEntry) -> Result<Document, CommandError> {
    if entry.options.is_empty() {
        return Ok(Document::new());
    }
    let options = doc_of(&entry.options)?;
    Ok(options
        .iter()
        .filter(|(k, _)| !k.starts_with('$'))
        .fold(Document::new(), |d, (k, v)| d.with(k, v.clone())))
}

/// An identifier in the form of RefDb's storage idents (a UUID's text): TeselaDB keeps no
/// files per collection or index, so it is derived from the collection's UUID and `what` (the
/// collection, its `_id` index or an index by name), the same on every read.
fn ident(collection: &CollectionEntry, what: &str) -> Value {
    let mut seed = collection.uuid.to_vec();
    seed.extend_from_slice(what.as_bytes());
    let mut b = teseladb_hash::sha256(&seed);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b[..16].iter().map(|x| format!("{x:02x}")).collect();
    Value::from(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

/// `versionContext` of the entries a RefDb 9.0.4 member writes under its feature
/// compatibility version (`create`, `createIndexes`, `collMod`).
fn version_context() -> Document {
    Document::new().with("OFCV", "9.0")
}

/// Whether `collection` is a time series.
fn is_timeseries(collection: &CollectionEntry) -> Result<bool, CommandError> {
    Ok(!collection.options.is_empty()
        && doc_of(&collection.options)?.get(super::kinds::TIMESERIES).is_some())
}

/// The fields after `o2` of a catalog entry: `isTimeseries` for a time series, then
/// `versionContext` when `versioned`.
fn catalog_extra(collection: &CollectionEntry, versioned: bool) -> Result<Document, CommandError> {
    let mut extra = Document::new();
    if is_timeseries(collection)? {
        extra.push("isTimeseries", true);
    }
    if versioned {
        extra.push("versionContext", version_context());
    }
    Ok(extra)
}

/// `create` as a member writes it: a time series as a clustered collection with its options
/// (`clusteredIndex`, `expireAfterSeconds`, `timeseries`) and no `_id` index; a view without
/// one either.
fn create_entry(ns: &Namespace, collection: &CollectionEntry) -> Result<Document, CommandError> {
    let mut o = Document::new().with("create", ns.coll.as_str());
    let options = create_options(collection)?;
    let view = options.get("viewOn").is_some();
    let series = options.get(super::kinds::TIMESERIES).is_some();
    if series {
        o.push("clusteredIndex", true);
        if let Some(e) = options.get(super::kinds::EXPIRE) {
            o.push(super::kinds::EXPIRE, e.clone());
        }
    }
    for (k, v) in options.iter().filter(|(k, _)| !series || *k != super::kinds::EXPIRE) {
        o.push(k, v.clone());
    }
    if !view && !series && options.get("clusteredIndex").is_none() {
        o.push("idIndex", id_index());
    }
    Ok(o)
}

/// `o2` of `create`: the collection's catalog number and its idents.
fn create_o2(collection: &CollectionEntry) -> Document {
    Document::new()
        .with("catalogId", i64::from(collection.id))
        .with("ident", ident(collection, ""))
        .with("idIndexIdent", ident(collection, "_id_"))
}

/// `collectionOptions_old` of a `collMod`: the collection's UUID and the options it had.
fn options_before(collection: &CollectionEntry) -> Result<Document, CommandError> {
    let uuid = Value::Binary { subtype: 4, bytes: collection.uuid.to_vec() };
    let mut d = Document::new().with("uuid", uuid);
    let options = create_options(collection)?;
    if options.get(super::kinds::TIMESERIES).is_some() {
        d.push("clusteredIndex", true);
    }
    for (k, v) in options.iter() {
        d.push(k, v.clone());
    }
    Ok(d)
}

/// The default of an index option a spec leaves out, for `indexOptions_old`.
fn index_default(option: &str) -> Value {
    match option {
        "expireAfterSeconds" => Value::Null,
        _ => Value::Boolean(false),
    }
}

/// `o.index` and `indexOptions_old` of the `collMod` of an index from its specification before
/// and after: its name and each option that changed, with the new and the old value.
fn index_change(before: &Document, after: &Document) -> (Document, Document) {
    let name = after.get("name").cloned().unwrap_or(Value::Null);
    let mut index = Document::new().with("name", name);
    let mut old = Document::new();
    let fields = after.iter().map(|(k, _)| k).chain(before.iter().map(|(k, _)| k));
    let mut seen: Vec<&str> = Vec::new();
    for k in fields {
        if seen.contains(&k) || matches!(k, "v" | "key" | "name") {
            continue;
        }
        seen.push(k);
        let (b, a) = (before.get(k), after.get(k));
        if b == a {
            continue;
        }
        index.push(k, a.cloned().unwrap_or_else(|| index_default(k)));
        old.push(k, b.cloned().unwrap_or_else(|| index_default(k)));
    }
    (index, old)
}

/// The entries of a transaction's new collection, at the rename that gives it its name when the
/// transaction commits: its `create` and the `createIndexes` of the indexes the transaction
/// built on it, under that name and as those changes give them (without the scratch name or the
/// rename, DV-091). `all` are the catalog changes of the history window.
fn scratch_creation(
    r: &DdlRecord,
    all: &[DdlRecord],
) -> Result<Vec<(Operation, Document)>, CommandError> {
    let DdlChange::Rename { from, to, collection, .. } = &r.change else {
        return Ok(Vec::new());
    };
    if !is_scratch(from) {
        return Ok(Vec::new());
    }
    let named = |change: DdlChange| DdlRecord { mark: r.mark, op: r.op, change };
    let create = named(DdlChange::Create { ns: to.clone(), collection: collection.clone() });
    let mut ops: Vec<(Operation, Document)> = ddl_operation(&create)?.into_iter().collect();
    for earlier in all.iter().filter(|e| (e.mark, e.op) < (r.mark, r.op)) {
        if let DdlChange::CreateIndex { collection: c, spec, .. } = &earlier.change
            && (c.tenant, c.id) == (collection.tenant, collection.id)
        {
            let index = named(DdlChange::CreateIndex {
                ns: to.clone(),
                collection: c.clone(),
                spec: spec.clone(),
            });
            ops.extend(ddl_operation(&index)?);
        }
    }
    Ok(ops)
}

/// The `c` entry of catalog change `r`, if a replica set has one.
fn ddl_operation(r: &DdlRecord) -> Result<Option<(Operation, Document)>, CommandError> {
    let ui = |c: &CollectionEntry| Some(c.uuid);
    let none = Document::new();
    // A transaction's new collection under its scratch name (`super::txn_ddl`): its creation,
    // indexes and drop show at the commit's rename, or never ([`scratch_creation`]).
    let scratch = match &r.change {
        DdlChange::Create { ns, .. }
        | DdlChange::Drop { ns, .. }
        | DdlChange::CreateIndex { ns, .. }
        | DdlChange::DropIndex { ns, .. }
        | DdlChange::ModifyIndex { ns, .. }
        | DdlChange::Modify { ns, .. } => is_scratch(ns),
        DdlChange::Rename { from, .. } => is_scratch(from),
        DdlChange::DropDatabase { .. } => false,
    };
    if scratch {
        return Ok(None);
    }
    Ok(Some(match &r.change {
        DdlChange::Create { ns, collection } => {
            let o = create_entry(ns, collection)?;
            let o2 = Some(create_o2(collection));
            let extra = catalog_extra(collection, true)?;
            (Operation::new(OpType::Command, cmd_ns(&ns.db), ui(collection), o, o2), extra)
        }
        DdlChange::Drop { ns, collection } => {
            let o = Document::new().with("drop", ns.coll.as_str());
            let extra = catalog_extra(collection, false)?;
            (Operation::new(OpType::Command, cmd_ns(&ns.db), ui(collection), o, None), extra)
        }
        DdlChange::Rename { from, to, collection, target } => {
            if super::reshard::cutover_event(from, target.as_ref())?.is_some() {
                return Ok(None);
            }
            let mut o = Document::new()
                .with("renameCollection", full(from).as_str())
                .with("to", full(to).as_str());
            if let Some(t) = target {
                o.push("dropTarget", Value::Binary { subtype: 4, bytes: t.uuid.to_vec() });
            }
            o.push("stayTemp", false);
            (Operation::new(OpType::Command, cmd_ns(&from.db), ui(collection), o, None), none)
        }
        DdlChange::Modify { ns, collection, description } => {
            if super::reshard::is_cutover_change(description)
                || super::balancing::is_balancing_change(description)
                || super::change_stream_ddl::is_shard(description)
            {
                return Ok(None);
            }
            let mut o = Document::new().with("collMod", ns.coll.as_str());
            for (k, v) in doc_of(description)?.iter().filter(|(k, _)| !k.starts_with('$')) {
                o.push(k, v.clone());
            }
            let o2 = Document::new().with("collectionOptions_old", options_before(collection)?);
            let extra = catalog_extra(collection, true)?;
            (Operation::new(OpType::Command, cmd_ns(&ns.db), ui(collection), o, Some(o2)), extra)
        }
        DdlChange::DropDatabase { db } => {
            let o = Document::new().with("dropDatabase", 1);
            let extra = Document::new().with("fromMigrate", false);
            (Operation::new(OpType::Command, cmd_ns(db), None, o, None), extra)
        }
        DdlChange::CreateIndex { ns, collection, spec } => {
            let mut o = Document::new().with("createIndexes", ns.coll.as_str());
            let spec = doc_of(spec)?;
            for (k, v) in spec.iter() {
                o.push(k, v.clone());
            }
            let name = match spec.get("name") {
                Some(Value::String(n)) => n.clone(),
                _ => String::new(),
            };
            let o2 = Some(Document::new().with("indexIdent", ident(collection, &name)));
            let extra = catalog_extra(collection, true)?;
            (Operation::new(OpType::Command, cmd_ns(&ns.db), ui(collection), o, o2), extra)
        }
        DdlChange::DropIndex { ns, collection, spec } => {
            let spec = doc_of(spec)?;
            let name = spec.get("name").cloned().unwrap_or(Value::Null);
            let o = Document::new().with("dropIndexes", ns.coll.as_str()).with("index", name);
            (Operation::new(OpType::Command, cmd_ns(&ns.db), ui(collection), o, Some(spec)), none)
        }
        DdlChange::ModifyIndex { ns, collection, before, after } => {
            let (index, old) = index_change(&doc_of(before)?, &doc_of(after)?);
            let o = Document::new().with("collMod", ns.coll.as_str()).with("index", index);
            let o2 = Document::new()
                .with("collectionOptions_old", options_before(collection)?)
                .with("indexOptions_old", old);
            let extra = catalog_extra(collection, true)?;
            (Operation::new(OpType::Command, cmd_ns(&ns.db), ui(collection), o, Some(o2)), extra)
        }
    }))
}

/// The operation of document change `change` of collection `w`; `prior` is the document the
/// transaction's earlier write of the same key left.
fn document_operation<B: Backend + ?Sized>(
    backend: &mut B,
    w: &Watched,
    change: &Change,
    prior: Option<Option<&Vec<u8>>>,
) -> Result<Operation, CommandError> {
    let ns = full(w.ns_at(&change.event));
    let key = &change.event.key;
    let from = match change.meta.as_ref().and_then(|m| m.moved.as_ref()) {
        Some(Moved::From(k)) => Some(k),
        _ => None,
    };
    let earlier = match (prior, from) {
        (Some(doc), _) => doc.cloned(),
        (None, _) if change.kind == EventKind::Insert && from.is_none() => None,
        (None, Some(k)) => previous(backend, k, change.event.ts)?,
        (None, None) => previous(backend, key, change.event.ts)?,
    };
    let keyed = |bytes: &[u8]| -> Result<Document, CommandError> {
        super::sharding::event_key(&w.entry, &doc_of(bytes)?)
    };
    let ui = Some(w.entry.uuid);
    let missing = || CommandError::new(INTERNAL_ERROR, "a changed document's version is gone");
    let op = match change.kind {
        EventKind::Insert => {
            let doc = change.doc.as_ref().ok_or_else(missing)?;
            Operation::new(OpType::Insert, ns, ui, doc_of(doc)?, Some(keyed(doc)?))
        }
        EventKind::Delete => {
            let before = earlier.as_ref().ok_or_else(missing)?;
            Operation::new(OpType::Delete, ns, ui, keyed(before)?, None)
        }
        EventKind::Replace => {
            let doc = change.doc.as_ref().ok_or_else(missing)?;
            let o2 = keyed(earlier.as_deref().unwrap_or(doc))?;
            Operation::new(OpType::Update, ns, ui, doc_of(doc)?, Some(o2))
        }
        _ => {
            let desc = change.meta.as_ref().and_then(|m| m.update.as_ref()).ok_or_else(missing)?;
            let before = earlier.as_ref().ok_or_else(missing)?;
            let o = teseladb_topology::diff::update_entry(&doc_of(before)?, desc);
            Operation::new(OpType::Update, ns, ui, o, Some(keyed(before)?))
        }
    };
    Ok(op)
}

/// The entries of the document changes `found`, of collections `by_id`.
fn document_items<B: Backend + ?Sized>(
    backend: &mut B,
    found: &[Change],
    by_id: &BTreeMap<(u32, u32), &Watched>,
    term: i64,
) -> Result<Vec<Item>, CommandError> {
    let mut items = Vec::new();
    let mut i = 0;
    while i < found.len() {
        let first = &found[i];
        let mut end = i + 1;
        let session = first.meta.as_ref().and_then(|m| m.retry);
        if let Some(r) = session {
            // The documents of one retryable write share its mark and session.
            while found.get(end).is_some_and(|c| {
                c.event.ts == first.event.ts && c.meta.as_ref().and_then(|m| m.retry) == Some(r)
            }) {
                end += 1;
            }
        } else {
            // A transaction's writes share its mark and number; the other writes of one mark are
            // one command's (an insert of several documents is one entry, [`plain_entries`]).
            while found.get(end).is_some_and(|c| {
                (c.event.ts, c.event.txn) == (first.event.ts, first.event.txn)
                    && c.meta.as_ref().and_then(|m| m.retry).is_none()
            }) {
                end += 1;
            }
        }
        let group = &found[i..end];
        i = end;
        // The document each key's last write in the transaction left.
        let mut left: BTreeMap<&[u8], Option<&Vec<u8>>> = BTreeMap::new();
        let mut ops = Vec::new();
        let mut positions = Vec::new();
        for change in group {
            let prior = left.insert(change.event.key.as_slice(), change.doc.as_ref());
            // Every change is of a document of a watched collection: its writes up to its drop.
            let live = collection_of(&change.event.key)
                .and_then(|id| by_id.get(&id))
                .filter(|w| !w.dropped.as_ref().is_some_and(|d| change.event > *d));
            let Some(w) = live else { continue };
            ops.push(document_operation(backend, w, change, prior)?);
            positions.push(change.meta.as_ref().map_or(0, |m| m.op));
        }
        if ops.is_empty() {
            continue;
        }
        let mark = first.event.ts;
        match (first.meta.as_ref().and_then(|m| m.txn.as_ref()), session) {
            (Some(t), _) => {
                let info =
                    TxnInfo { lsid: lsid_document(t.lsid, &t.uid), txn_number: t.txn_number };
                let mut next = mark;
                let mut at = || {
                    let now = next;
                    next = Hlc { physical: now.physical, logical: now.logical.saturating_add(1) };
                    op_time(now, term)
                };
                let entries = transaction_entries(info, ops, wall(mark), &mut at);
                for (seq, e) in (0u32..).zip(entries) {
                    let at =
                        e.op_time.map_or(mark, |o| from_timestamp((o.ts.time, o.ts.increment)));
                    items.push(Item { mark: at, seq, doc: e.to_document() });
                }
            }
            (None, Some(r)) => {
                let doc = retryable(&r, positions.into_iter().zip(ops).collect(), mark, term);
                items.push(Item { mark, seq: 0, doc });
            }
            (None, None) => {
                for (seq, doc) in (0u32..).zip(plain_entries(ops, mark, term)) {
                    items.push(Item { mark, seq, doc });
                }
            }
        }
    }
    Ok(items)
}

/// The entries above mark `from` (raised to the history window), up to the lowest mark every
/// source has resolved.
///
/// A read from the start of the window asks the ranges for their whole history at once: a range
/// answers a later start one second at a time (`teseladb_range::host::feed::FEED_WINDOW_MS`).
pub fn read<B: Backend + ?Sized>(backend: &mut B, from: Hlc) -> Result<OplogRead, CommandError> {
    let floor = Hlc { physical: history_floor(backend)?, logical: 0 };
    let ask = if from <= floor { START } else { from };
    let from = from.max(floor);
    let term = super::presentation::term(backend)?;
    let ddl = backend.catalog_changes(from)?;
    let tenant = backend.tenant();
    let now: Vec<(Namespace, CollectionEntry)> =
        backend.collections()?.into_iter().filter(|(_, c)| c.tenant == tenant).collect();
    let records: Vec<DdlRecord> = ddl.records.into_iter().filter(|r| r.mark > from).collect();
    let colls = watched_all(now, &records);
    let mut resolved = ddl.resolved;
    let mut rows = Vec::new();
    for w in &colls {
        let (start, end) = documents_of(&w.entry);
        let (more, mark) = backend.changes(&start, &end, ask)?;
        resolved = resolved.min(mark);
        rows.extend(more);
    }
    let by_id: BTreeMap<(u32, u32), &Watched> =
        colls.iter().map(|w| ((w.entry.tenant, w.entry.id), w)).collect();
    let versions = rows
        .into_iter()
        .filter(|r| r.ts > from && r.ts <= resolved)
        .map(|r| Versioned { key: r.key, ts: r.ts, value: r.value });
    let found = changes(versions).map_err(|e| {
        CommandError::new(INTERNAL_ERROR, format!("the oplog read a bad meta: {e:?}"))
    })?;
    // A deletion at the old key of a document that moved, and the moves of `shardCollection`,
    // are no operation (018 FR-001).
    let found: Vec<Change> = found.into_iter().filter(gives_event).collect();
    let mut items = document_items(backend, &found, &by_id, term)?;
    let renames_scratch =
        |r: &DdlRecord| matches!(&r.change, DdlChange::Rename { from, .. } if is_scratch(from));
    let window = match records.iter().any(renames_scratch) {
        true => backend.catalog_changes(floor)?.records,
        false => Vec::new(),
    };
    for r in records.iter().filter(|r| r.mark <= resolved) {
        let seq = r.op.saturating_mul(SEQ_STRIDE);
        if let Some((op, extra)) = ddl_operation(r)? {
            items.push(Item { mark: r.mark, seq, doc: plain(op, extra, r.mark, term) });
        }
        for (k, (op, extra)) in scratch_creation(r, &window)?.into_iter().enumerate() {
            let seq = seq.saturating_add(u32::try_from(k).unwrap_or(u32::MAX));
            items.push(Item { mark: r.mark, seq, doc: plain(op, extra, r.mark, term) });
        }
    }
    for (key, value) in backend.group0_settings(NOTE_PREFIX)? {
        let Some(mark) = note_mark(&key).filter(|m| *m > from && *m <= resolved) else {
            continue;
        };
        let op = Operation::new(OpType::Noop, "", None, doc_of(&value)?, None);
        items.push(Item { mark, seq: 0, doc: plain(op, Document::new(), mark, term) });
    }
    items.sort_by_key(|a| (a.mark, a.seq));
    Ok(OplogRead { items, resolved: resolved.max(from) })
}

/// The entries of the history window `filter` can match, for a read that is not tailable, in
/// commit order: up to the node's clock when the read starts, so that the newest entry is at
/// least the last write the node acknowledged (what `refdump --oplog` takes as the end of its
/// copy). With `newest` (a `$natural: -1` read with that limit and no lower bound on `ts`), only
/// the newest entries, at least that many when the window has them: the read goes back from the
/// clock one second at a time instead of reading the whole window.
pub fn documents<B: Backend + ?Sized>(
    backend: &mut B,
    filter: &Document,
    newest: Option<usize>,
) -> Result<Vec<Document>, CommandError> {
    let target = backend.cluster_mark();
    let lower = lower_bound(filter);
    let items = match (newest, lower) {
        (Some(n), None) => newest_items(backend, target, n)?,
        _ => {
            let mut out = read(backend, lower.unwrap_or(START))?;
            catch_up(backend, &mut out, target)?;
            out.items
        }
    };
    Ok(items.into_iter().map(|i| i.doc).collect())
}

/// Reads on from `out` until mark `until` is resolved. The ranges close their marks a moment
/// behind their clocks: a read that stops advancing pauses ([`Backend::pause`]) before it asks
/// again, for at most [`CATCH_UP_MS`], and then keeps what is resolved.
fn catch_up<B: Backend + ?Sized>(
    backend: &mut B,
    out: &mut OplogRead,
    until: Hlc,
) -> Result<(), CommandError> {
    let mut deadline = None;
    while out.resolved < until {
        let more = read(backend, out.resolved)?;
        let advanced = more.resolved > out.resolved;
        out.items.extend(more.items);
        out.resolved = out.resolved.max(more.resolved);
        if advanced {
            deadline = None;
            continue;
        }
        let now = backend.wall_ms();
        if now >= *deadline.get_or_insert(now.saturating_add(CATCH_UP_MS)) || !backend.pause() {
            break;
        }
    }
    out.items.retain(|i| i.mark <= until);
    Ok(())
}

/// The newest entries up to mark `target`, at least `n` of them when the history window has
/// them, read back one window of [`BACK_WINDOW_MS`] at a time, in commit order.
fn newest_items<B: Backend + ?Sized>(
    backend: &mut B,
    target: Hlc,
    n: usize,
) -> Result<Vec<Item>, CommandError> {
    let floor = history_floor(backend)?;
    let mut items: Vec<Item> = Vec::new();
    let mut upper = target;
    while items.len() < n {
        let start = upper.physical.saturating_sub(BACK_WINDOW_MS);
        // The rest of the history window at once.
        let from = match start <= floor {
            true => START,
            false => Hlc { physical: start, logical: 0 },
        };
        let mut out = read(backend, from)?;
        catch_up(backend, &mut out, upper)?;
        out.items.extend(items);
        items = out.items;
        if from == START {
            break;
        }
        upper = from;
    }
    Ok(items)
}

/// The mark below which `filter` matches no entry: from a top-level `ts` equality, `$gt` or
/// `$gte` on a timestamp, the mark just below it.
pub fn lower_bound(filter: &Document) -> Option<Hlc> {
    let below = |time: u32, increment: u32| {
        let m = from_timestamp((time, increment));
        teseladb_changestream::resolved::below(m)
    };
    match filter.get("ts")? {
        Value::Timestamp { time, increment } => below(*time, *increment),
        Value::Document(d) => d
            .iter()
            .filter_map(|(k, v)| match (k, v) {
                ("$gt" | "$gte", Value::Timestamp { time, increment }) => below(*time, *increment),
                _ => None,
            })
            .max(),
        _ => None,
    }
}
