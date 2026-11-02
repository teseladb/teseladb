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
use teseladb_changestream::changes::{Change, Versioned, changes, collection_of};
use teseladb_changestream::meta::{Moved, lsid_document};
use teseladb_changestream::token::EventKind;
use teseladb_hlc::Hlc;
use teseladb_hlc::{from_timestamp, to_timestamp};
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_meta::command::SettingChange;
use teseladb_meta::ddl_log::{DdlChange, DdlRecord};
use teseladb_topology::oplog::{
    DROP_REFUSED, DROP_REFUSED_CODE, ENTRY_VERSION, OpType, Operation, OplogEntry, TxnInfo,
    WRITE_REFUSED, transaction_entries,
};
use teseladb_topology::replset::{OpTime, Timestamp};

use super::Backend;
use super::change_stream::{Watched, history_floor, previous, watched_all};
use super::query::documents_of;
use crate::error::{Code, CommandError, INTERNAL_ERROR, INVALID_NAMESPACE};

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
        stmt_id: None,
        extra,
        op_time: Some(op_time(mark, term)),
        version: ENTRY_VERSION,
        wall_ms: Some(wall(mark)),
        prev_op_time: None,
    }
    .to_document()
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

/// The `c` entry of catalog change `r`, if a replica set has one.
fn ddl_operation(r: &DdlRecord) -> Result<Option<(Operation, Document)>, CommandError> {
    let ui = |c: &CollectionEntry| Some(c.uuid);
    let none = Document::new();
    Ok(Some(match &r.change {
        DdlChange::Create { ns, collection } => {
            let mut o = Document::new().with("create", ns.coll.as_str());
            let options = create_options(collection)?;
            let view = options.get("viewOn").is_some();
            for (k, v) in options.iter() {
                o.push(k, v.clone());
            }
            if !view {
                o.push("idIndex", id_index());
            }
            (Operation::new(OpType::Command, cmd_ns(&ns.db), ui(collection), o, None), none)
        }
        DdlChange::Drop { ns, collection } => {
            let o = Document::new().with("drop", ns.coll.as_str());
            (Operation::new(OpType::Command, cmd_ns(&ns.db), ui(collection), o, None), none)
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
            let uuid = Value::Binary { subtype: 4, bytes: collection.uuid.to_vec() };
            let o2 =
                Document::new().with("collectionOptions_old", Document::new().with("uuid", uuid));
            (Operation::new(OpType::Command, cmd_ns(&ns.db), ui(collection), o, Some(o2)), none)
        }
        DdlChange::DropDatabase { db } => {
            let o = Document::new().with("dropDatabase", 1);
            let extra = Document::new().with("fromMigrate", false);
            (Operation::new(OpType::Command, cmd_ns(db), None, o, None), extra)
        }
        DdlChange::CreateIndex { ns, collection, spec } => {
            let mut o = Document::new().with("createIndexes", ns.coll.as_str());
            for (k, v) in doc_of(spec)?.iter() {
                o.push(k, v.clone());
            }
            (Operation::new(OpType::Command, cmd_ns(&ns.db), ui(collection), o, None), none)
        }
        DdlChange::DropIndex { ns, collection, spec } => {
            let spec = doc_of(spec)?;
            let name = spec.get("name").cloned().unwrap_or(Value::Null);
            let o = Document::new().with("dropIndexes", ns.coll.as_str()).with("index", name);
            (Operation::new(OpType::Command, cmd_ns(&ns.db), ui(collection), o, Some(spec)), none)
        }
        DdlChange::ModifyIndex { ns, collection, after, .. } => {
            let index = doc_of(after)?;
            let o = Document::new().with("collMod", ns.coll.as_str()).with("index", index);
            (Operation::new(OpType::Command, cmd_ns(&ns.db), ui(collection), o, None), none)
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
        if first.event.txn != 0 {
            while found
                .get(end)
                .is_some_and(|c| c.event.ts == first.event.ts && c.event.txn == first.event.txn)
            {
                end += 1;
            }
        }
        let group = &found[i..end];
        i = end;
        // The document each key's last write in the transaction left.
        let mut left: BTreeMap<&[u8], Option<&Vec<u8>>> = BTreeMap::new();
        let mut ops = Vec::new();
        for change in group {
            let prior = left.insert(change.event.key.as_slice(), change.doc.as_ref());
            let Some(w) = collection_of(&change.event.key).and_then(|id| by_id.get(&id)) else {
                continue;
            };
            if w.dropped.as_ref().is_some_and(|d| change.event > *d) {
                continue;
            }
            ops.push(document_operation(backend, w, change, prior)?);
        }
        if ops.is_empty() {
            continue;
        }
        let mark = first.event.ts;
        match first.meta.as_ref().and_then(|m| m.txn.as_ref()) {
            Some(t) => {
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
            None => {
                for (seq, op) in (0u32..).zip(ops) {
                    items.push(Item { mark, seq, doc: plain(op, Document::new(), mark, term) });
                }
            }
        }
    }
    Ok(items)
}

/// The entries above mark `from` (raised to the history window), up to the lowest mark every
/// source has resolved.
pub fn read<B: Backend + ?Sized>(backend: &mut B, from: Hlc) -> Result<OplogRead, CommandError> {
    let floor = Hlc { physical: history_floor(backend)?, logical: 0 };
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
        let (more, mark) = backend.changes(&start, &end, from)?;
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
    let mut items = document_items(backend, &found, &by_id, term)?;
    for r in records.iter().filter(|r| r.mark <= resolved) {
        if let Some((op, extra)) = ddl_operation(r)? {
            items.push(Item { mark: r.mark, seq: r.op, doc: plain(op, extra, r.mark, term) });
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

/// Every entry of the history window, for a read that is not tailable.
pub fn documents<B: Backend + ?Sized>(
    backend: &mut B,
    filter: &Document,
) -> Result<Vec<Document>, CommandError> {
    let from = lower_bound(filter).unwrap_or(Hlc { physical: 0, logical: 0 });
    Ok(read(backend, from)?.items.into_iter().map(|i| i.doc).collect())
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
