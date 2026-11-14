//! `collMod` of an index (009 FR-004, FR-007, contract `index-commands.md`): `hidden`,
//! `expireAfterSeconds`, `prepareUnique` and the conversion to `unique`, with the replies and
//! errors of a `refrouter` 9.0.4 (measured). The conversion first looks for documents with the same
//! values in the index's entries (`CannotConvertIndexToUnique` with their `_id`s), then builds a
//! unique index under another name, which takes the old one's place in Group 0 before the old
//! entries go.

use std::collections::BTreeMap;

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_index::spec::index_def;
use teseladb_keys::{KeyBuilder, PrimaryKey, values_len};
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_meta::indexes::{IndexCommand, IndexEntry};
use teseladb_range::read::ReadConcern;

use super::crud::MODIFY_CHUNK_KEYS;
use super::indexes::{backfill, drop_one, in_stored_order};
use super::names::full;
use super::query::documents_of;
use super::{Backend, display};
use crate::error::{
    BAD_VALUE, CANNOT_CONVERT_INDEX_TO_UNIQUE, CANNOT_CREATE_INDEX, CommandError, DUPLICATE_KEY,
    HASHED_UNIQUE, IDL_UNKNOWN_FIELD, INDEX_NOT_FOUND, INTERNAL_ERROR, INVALID_OPTIONS,
    TYPE_MISMATCH,
};

/// Fields of the `index` of `collMod`.
const NAME: &str = "name";
const KEY_PATTERN: &str = "keyPattern";
const HIDDEN: &str = "hidden";
const EXPIRE_AFTER_SECONDS: &str = "expireAfterSeconds";
const PREPARE_UNIQUE: &str = "prepareUnique";
const UNIQUE: &str = "unique";
/// Fields of a stored specification.
const KEY: &str = "key";
/// Kinds of index that cannot be unique.
const HASHED: &str = "hashed";
const WILDCARD: &str = "$**";
const WILDCARD_SUFFIX: &str = ".$**";
/// The name a unique index has while it is built to replace another (no client name has a NUL).
const CONVERSION_SUFFIX: &str = "\u{0}unique";
/// The types a boolean field of `collMod` takes, as its errors name them.
const BOOLEAN_TYPES: &str = "[bool, double, decimal, int, long]";

/// How `collMod` changes an index.
#[derive(Clone, Copy, Debug, Default)]
pub struct Modify {
    /// `dryRun`: only checks a conversion to `unique`.
    pub dry_run: bool,
    /// The command also changes options of the collection.
    pub collection_changes: bool,
}

/// `dryRun` of `collMod` (`v`), in a command with an `index` or not (`index`).
pub fn dry_run(v: Option<&Value>, index: bool) -> Result<bool, CommandError> {
    let Some(v) = v else { return Ok(false) };
    let Some(on) = crate::types::truthy(v) else {
        let msg = format!(
            "BSON field 'collMod.dryRun' is the wrong type '{}', expected types '{BOOLEAN_TYPES}'",
            crate::types::type_name(v)
        );
        return Err(CommandError::new(TYPE_MISMATCH, msg));
    };
    if on && !index {
        let msg = "dry run mode requires an valid index modification.";
        return Err(CommandError::new(INVALID_OPTIONS, msg));
    }
    Ok(on)
}

/// What the `index` of `collMod` asks.
#[derive(Debug, Default)]
struct Request {
    name: Option<String>,
    key: Option<Value>,
    hidden: Option<bool>,
    expire: Option<Value>,
    prepare: Option<bool>,
    unique: Option<bool>,
}

fn wrong_type(field: &str, v: &Value, expected: &str) -> CommandError {
    let msg = format!(
        "BSON field 'collMod.index.{field}' is the wrong type '{}', expected {expected}",
        crate::types::type_name(v)
    );
    CommandError::new(TYPE_MISMATCH, msg)
}

/// A boolean field: `null` is no field.
fn boolean(field: &str, v: &Value) -> Result<Option<bool>, CommandError> {
    if matches!(v, Value::Null) {
        return Ok(None);
    }
    crate::types::truthy(v)
        .map(Some)
        .ok_or_else(|| wrong_type(field, v, &format!("types '{BOOLEAN_TYPES}'")))
}

fn parse(change: &Value) -> Result<Request, CommandError> {
    let Value::Document(change) = change else {
        let msg = format!(
            "BSON field 'collMod.index' is the wrong type '{}', expected type 'object'",
            crate::types::type_name(change)
        );
        return Err(CommandError::new(TYPE_MISMATCH, msg));
    };
    let mut r = Request::default();
    for (field, v) in change.iter() {
        match field {
            NAME => match v {
                Value::String(s) => r.name = Some(s.clone()),
                v => return Err(wrong_type(field, v, "type 'string'")),
            },
            KEY_PATTERN => match v {
                Value::Document(_) => r.key = Some(v.clone()),
                v => return Err(wrong_type(field, v, "type 'object'")),
            },
            HIDDEN => r.hidden = boolean(field, v)?,
            PREPARE_UNIQUE => r.prepare = boolean(field, v)?,
            UNIQUE => r.unique = boolean(field, v)?,
            EXPIRE_AFTER_SECONDS => r.expire = Some(v.clone()),
            _ => {
                let msg = format!("BSON field 'collMod.index.{field}' is an unknown field.");
                return Err(CommandError::new(IDL_UNKNOWN_FIELD, msg));
            }
        }
    }
    match (&r.name, &r.key) {
        (Some(_), Some(_)) => {
            let msg = "Cannot specify both key pattern and name.";
            return Err(CommandError::new(INVALID_OPTIONS, msg));
        }
        (None, None) => {
            let msg = "Must specify either index name or key pattern.";
            return Err(CommandError::new(INVALID_OPTIONS, msg));
        }
        _ => {}
    }
    if r.hidden.is_none() && r.expire.is_none() && r.prepare.is_none() && r.unique.is_none() {
        let msg = "no expireAfterSeconds, hidden, unique, or prepareUnique field";
        return Err(CommandError::new(INVALID_OPTIONS, msg));
    }
    Ok(r)
}

/// The index `r` names, with its stored specification.
fn find<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    coll: &CollectionEntry,
    r: &Request,
) -> Result<(IndexEntry, Document), CommandError> {
    for e in backend.indexes(coll.id)? {
        let spec = Document::from(RawDoc::from_bytes(&e.spec)?);
        let named = match (&r.name, &r.key) {
            (Some(n), _) => spec.get(NAME) == Some(&Value::from(n.as_str())),
            (None, Some(k)) => spec.get(KEY) == Some(k),
            (None, None) => false,
        };
        if named {
            return Ok((e, spec));
        }
    }
    let which = match (&r.name, &r.key) {
        (Some(n), _) => n.clone(),
        (None, Some(k)) => display(k),
        (None, None) => String::new(),
    };
    let msg = format!("cannot find index {which} for ns {}", full(ns));
    Err(CommandError::new(INDEX_NOT_FOUND, msg))
}

fn set(spec: &Document, field: &str) -> bool {
    spec.get(field).and_then(crate::types::truthy) == Some(true)
}

/// `collMod` of an index: the index's specification changes in Group 0 and the reply gives each
/// old and new value that changed, as a `refrouter` 9.0.4 does (measured; a field set to what it
/// already is gives nothing, `hidden: false` and `prepareUnique: false` drop the field).
pub fn modify_index<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    coll: &CollectionEntry,
    change: &Value,
    how: Modify,
) -> Result<Document, CommandError> {
    let r = parse(change)?;
    // The index of `_id` cannot be hidden (measured on a `refrouter` 9.0.4 on 05-11-2026).
    let id_key = Value::Document(Document::new().with("_id", 1));
    let names_id = r.name.as_deref() == Some("_id_") || r.key.as_ref() == Some(&id_key);
    if names_id && r.hidden.is_some() {
        return Err(CommandError::new(BAD_VALUE, "can't hide _id index"));
    }
    let (entry, mut spec) = find(backend, ns, coll, &r)?;
    let (unique, prepared) = (set(&spec, UNIQUE), set(&spec, PREPARE_UNIQUE));
    if r.unique == Some(false) {
        return Err(CommandError::new(BAD_VALUE, "Cannot make index non-unique"));
    }
    let converting = r.unique == Some(true) && !unique;
    if converting && !prepared {
        let msg =
            "Cannot make index unique with 'prepareUnique=false'. Run collMod to set it first.";
        return Err(CommandError::new(INVALID_OPTIONS, msg));
    }
    // A unique index has nothing to prepare: `prepareUnique` changes nothing there.
    let prepare = r.prepare.filter(|p| !unique && *p != prepared);
    let others = r.hidden.is_some() || r.expire.is_some() || r.unique.is_some();
    if prepare.is_some() && (others || how.collection_changes) {
        let msg = "prepareUnique cannot be combined with any other modification.";
        return Err(CommandError::new(INVALID_OPTIONS, msg));
    }
    if how.dry_run {
        if r.unique != Some(true) {
            let msg = "dry run mode requires an index modification with unique: true.";
            return Err(CommandError::new(INVALID_OPTIONS, msg));
        }
        if r.hidden.is_some() || r.expire.is_some() || r.prepare.is_some() || how.collection_changes
        {
            let msg =
                "unique: true cannot be combined with any other modification in dry run mode.";
            return Err(CommandError::new(INVALID_OPTIONS, msg));
        }
    }
    let mut reply = Document::new();
    if let Some(new) = r.hidden
        && new != set(&spec, HIDDEN)
    {
        reply.push("hidden_old", !new);
        reply.push("hidden_new", new);
        spec.remove(HIDDEN);
        if new {
            spec.push(HIDDEN, true);
        }
    }
    if let Some(v) = &r.expire {
        let Some(old) = spec.get(EXPIRE_AFTER_SECONDS).and_then(crate::types::number) else {
            let msg = "no expireAfterSeconds field to update";
            return Err(CommandError::new(INVALID_OPTIONS, msg));
        };
        let Some(new) = crate::types::number(v) else {
            let msg = "expireAfterSeconds must be a number";
            return Err(CommandError::new(TYPE_MISMATCH, msg));
        };
        if old as i64 != new as i64 {
            reply.push("expireAfterSeconds_old", old as i64);
            reply.push("expireAfterSeconds_new", new as i64);
        }
        spec.insert(EXPIRE_AFTER_SECONDS, new as i32);
    }
    if let Some(new) = prepare {
        reply.push("prepareUnique_old", !new);
        reply.push("prepareUnique_new", new);
        spec.remove(PREPARE_UNIQUE);
        if new {
            spec.push(PREPARE_UNIQUE, true);
        }
    }
    if converting {
        // The duplicates come before the type of the index (a `refrouter` 9.0.4, measured).
        let groups = violations(backend, coll, &entry)?;
        if !groups.is_empty() {
            return Err(cannot_convert(groups));
        }
        convertible(&spec)?;
        if how.dry_run {
            return Ok(Document::new());
        }
        spec.remove(PREPARE_UNIQUE);
        spec.insert(UNIQUE, true);
        convert(backend, ns, coll, &entry, &in_stored_order(&spec))?;
        reply.push("unique_new", true);
        return Ok(reply);
    }
    if how.dry_run {
        return Ok(Document::new());
    }
    let spec = in_stored_order(&spec);
    backend.index_write(IndexCommand::SetSpec { id: entry.id, spec: spec.to_bytes()? })?;
    Ok(reply)
}

/// Whether an index of specification `spec` can be unique: a hashed one cannot, with the error
/// of a `refrouter` 9.0.4 (measured). Nor can a wildcard one, with the error `createIndexes` gives
/// (DV: a `refnode` 9.0.4 accepts the conversion and aborts on the invalid index it wrote).
fn convertible(spec: &Document) -> Result<(), CommandError> {
    // The node stored the specification with its key.
    let key = match spec.get(KEY) {
        Some(Value::Document(key)) => key.clone(),
        _ => Document::new(),
    };
    if key.iter().any(|(_, v)| v == &Value::from(HASHED)) {
        let msg = "Currently hashed indexes cannot guarantee uniqueness. Use a regular index.";
        return Err(CommandError::new(HASHED_UNIQUE, msg));
    }
    if key.iter().any(|(k, _)| k == WILDCARD || k.ends_with(WILDCARD_SUFFIX)) {
        let msg = "Index type 'wildcard' does not support the 'unique' option";
        return Err(CommandError::new(CANNOT_CREATE_INDEX, msg));
    }
    Ok(())
}

/// `CannotConvertIndexToUnique` with the `_id`s of each group of documents that share values.
fn cannot_convert(groups: Vec<Vec<Value>>) -> CommandError {
    let msg = "Cannot convert the index to unique. Please resolve conflicting documents before \
               running collMod again.";
    let mut e = CommandError::new(CANNOT_CONVERT_INDEX_TO_UNIQUE, msg);
    let items: Vec<Value> = groups
        .into_iter()
        .map(|ids| Value::Document(Document::new().with("ids", Value::Array(ids))))
        .collect();
    e.extra.push("violations", Value::Array(items));
    e
}

/// The groups of documents whose entries in index `entry` have the same values, in the order of
/// the values (past the bucket, D-008), each with its `_id`s in their order (a `refrouter` 9.0.4 lists them in the order the
/// documents were inserted; DV). The entries are read in order: the values end where the
/// primary key starts. Then the documents give the `_id`s of the primary keys found.
pub(super) fn violations<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    entry: &IndexEntry,
) -> Result<Vec<Vec<Value>>, CommandError> {
    let spec = RawDoc::from_bytes(&entry.spec)?;
    let def =
        index_def(spec, entry.buckets).map_err(|e| CommandError::new(INTERNAL_ERROR, e.reason))?;
    // A text index keeps no entries here (011).
    let Some(def) = def else { return Ok(Vec::new()) };
    let dirs: Vec<_> = def.key_spec().fields.iter().map(|f| f.direction).collect();
    let start = KeyBuilder::index_prefix(coll.tenant, coll.id, entry.id).as_bytes().to_vec();
    let end = KeyBuilder::index_prefix(coll.tenant, coll.id, entry.id + 1).as_bytes().to_vec();
    let head = start.len() + usize::from(def.buckets > 0);
    let malformed = || CommandError::new(INTERNAL_ERROR, "an index entry that does not decode");
    let mut groups: Vec<(Vec<u8>, Vec<Vec<u8>>)> = Vec::new();
    let mut from = start;
    loop {
        let rows = backend.scan(&from, &end, MODIFY_CHUNK_KEYS, ReadConcern::Local)?;
        let Some((last, _)) = rows.last() else { break };
        from = [last.as_slice(), &[0]].concat();
        for (key, _) in rows {
            let rest = key.get(head..).ok_or_else(malformed)?;
            let cut = head + values_len(rest, &dirs).map_err(|_| malformed())?;
            let (values, pk) = key.split_at(cut);
            // Keys are distinct: under the same values each one is another document.
            match groups.last_mut() {
                Some((v, pks)) if v.as_slice() == values => pks.push(pk.to_vec()),
                _ => groups.push((values.to_vec(), vec![pk.to_vec()])),
            }
        }
    }
    groups.retain(|(_, pks)| pks.len() > 1);
    // In the values' order, as an index without buckets would read them.
    groups.sort_by(|(a, _), (b, _)| a.get(head..).cmp(&b.get(head..)));
    if groups.is_empty() {
        return Ok(Vec::new());
    }
    let mut ids: BTreeMap<Vec<u8>, Value> =
        groups.iter().flat_map(|(_, pks)| pks.iter().map(|p| (p.clone(), Value::Null))).collect();
    let (mut from, end) = documents_of(coll);
    loop {
        let rows = backend.scan(&from, &end, MODIFY_CHUNK_KEYS, ReadConcern::Local)?;
        let Some((last, _)) = rows.last() else { break };
        from = [last.as_slice(), &[0]].concat();
        for (_, bytes) in rows {
            let doc = RawDoc::from_bytes(&bytes)?;
            let Some(id) = doc.get("_id") else { continue };
            let pk = PrimaryKey::id(id);
            if let Some(slot) = ids.get_mut(pk.as_bytes()) {
                *slot = Value::from(id);
            }
        }
    }
    Ok(groups
        .into_iter()
        .map(|(_, pks)| pks.iter().filter_map(|p| ids.get(p).cloned()).collect())
        .collect())
}

/// Builds the unique index of specification `spec` under another name and makes it take the
/// place of `old`, whose entries then go. A duplicate a write slipped in meanwhile fails the
/// conversion as the check before it would have.
fn convert<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    coll: &CollectionEntry,
    old: &IndexEntry,
    spec: &Document,
) -> Result<(), CommandError> {
    let name = format!("{}{CONVERSION_SUFFIX}", old.name);
    let cmd = IndexCommand::Create {
        collection: coll.id,
        name,
        spec: spec.to_bytes()?,
        buckets: old.buckets,
    };
    let new = super::index_build::start(backend, coll.id, cmd)?;
    if let Err(e) = backfill(backend, ns, coll, std::slice::from_ref(&new)) {
        drop_one(backend, ns, coll, &new)?;
        if e.code.code != DUPLICATE_KEY.code {
            return Err(e);
        }
        let groups = violations(backend, coll, old)?;
        return Err(if groups.is_empty() { e } else { cannot_convert(groups) });
    }
    backend.index_write(IndexCommand::Swap { old: old.id, new: new.id })?;
    let dropped = backend
        .indexes(coll.id)?
        .into_iter()
        .find(|e| e.id == old.id)
        .ok_or_else(|| CommandError::new(INTERNAL_ERROR, "the old index went missing"))?;
    drop_one(backend, ns, coll, &dropped)
}
