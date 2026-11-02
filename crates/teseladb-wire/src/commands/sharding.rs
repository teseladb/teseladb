//! `enableSharding` and `shardCollection` (018 FR-001): the shard key of a collection, checked
//! with the messages of a `refrouter` 9.0.4 (measured on 01-11-2026), kept with the collection's
//! options under [`SHARD_KEY`] (not a collection option: `listCollections` does not show it) and
//! listed in `config.collections`.

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_meta::catalog::{CatalogCommand, CollectionEntry, Namespace};
use teseladb_meta::collection_types::CollectionKind;

use super::names::{full, parse_full};
use super::{Backend, Ctx, display, kinds};
use crate::error::{
    ALREADY_INITIALIZED, BAD_VALUE, COMMAND_NOT_SUPPORTED_ON_VIEW, CommandError, ILLEGAL_OPERATION,
    INVALID_NAMESPACE, INVALID_OPTIONS, SHARD_KEY_NOT_FOUND,
};
use crate::types::wrong_type;

/// `enableSharding`.
pub const ENABLE_SHARDING: &str = "enableSharding";
/// `shardCollection`.
pub const SHARD_COLLECTION: &str = "shardCollection";
/// Where the catalog keeps a collection's shard key among its options: `{key, unique}`.
pub const SHARD_KEY: &str = "$shardKey";
/// The prefix of the system collections, which a shard key cannot be given.
const SYSTEM_PREFIX: &str = "system.";
/// The value of a hashed field of a shard key.
const HASHED: &str = "hashed";

/// A collection's shard key.
#[derive(Clone, Debug, PartialEq)]
pub struct ShardKey {
    /// Its fields and their values (`1` or `"hashed"`), in order.
    pub key: Document,
    /// `unique`.
    pub unique: bool,
}

/// The shard key of `coll`, if it has one.
pub fn shard_key_of(coll: &CollectionEntry) -> Result<Option<ShardKey>, CommandError> {
    let options = RawDoc::from_bytes(&coll.options)?;
    let Some(teseladb_bson::RawValue::Document(d)) = options.get(SHARD_KEY) else {
        return Ok(None);
    };
    let d = Document::from(d);
    let key = match d.get("key") {
        Some(Value::Document(k)) => k.clone(),
        _ => Document::new(),
    };
    Ok(Some(ShardKey { key, unique: matches!(d.get("unique"), Some(Value::Boolean(true))) }))
}

/// Whether `coll` has a shard key: its documents are keyed by it ([`document_key`]).
pub fn is_sharded(coll: &CollectionEntry) -> bool {
    RawDoc::from_bytes(&coll.options).is_ok_and(|o| o.get(SHARD_KEY).is_some())
}

/// The value at dotted `path` of `doc`: `None` when it is missing; an array on the way is
/// refused, as no shard key holds one (measured on a `refrouter` 9.0.4 on 02-11-2026).
fn shard_value<'a>(doc: &'a Document, path: &str) -> Result<Option<&'a Value>, CommandError> {
    let mut at = doc;
    let mut parts = path.split('.').peekable();
    while let Some(part) = parts.next() {
        let v = match at.get(part) {
            None => return Ok(None),
            Some(Value::Array(_)) => {
                let msg = "Shard key cannot contain array values or array descendants.";
                return Err(CommandError::new(SHARD_KEY_NOT_FOUND, msg));
            }
            Some(v) => v,
        };
        match (parts.peek(), v) {
            (None, _) => return Ok(Some(v)),
            (Some(_), Value::Document(d)) => at = d,
            (Some(_), _) => return Ok(None),
        }
    }
    Ok(None)
}

/// The values of shard key `key` in `doc`, in order, as its document's key holds them: a
/// missing field is `null` and a hashed field its hash.
pub fn shard_values(key: &Document, doc: &Document) -> Result<Vec<Value>, CommandError> {
    let mut out = Vec::new();
    for (path, kind) in key.iter() {
        let v = shard_value(doc, path)?.cloned().unwrap_or(Value::Null);
        out.push(match kind {
            Value::String(s) if s == HASHED => {
                let holder = Document::new().with("v", v).to_bytes()?;
                let raw =
                    RawDoc::from_bytes(&holder)?.get("v").unwrap_or(teseladb_bson::RawValue::Null);
                Value::Int64(teseladb_index::keygen::hashed_key(raw))
            }
            _ => v,
        });
    }
    Ok(out)
}

/// The key of document `doc` of `coll`, whose shard key is `key`: `enc(shard key) · enc(_id)`.
pub fn document_key(
    coll: &CollectionEntry,
    key: &Document,
    doc: &Document,
) -> Result<Vec<u8>, CommandError> {
    document_key_in((coll.tenant, coll.id), key, doc)
}

/// [`document_key`] in the collection `collection` of tenant `tenant`.
pub fn document_key_in(
    (tenant, collection): (u32, u32),
    key: &Document,
    doc: &Document,
) -> Result<Vec<u8>, CommandError> {
    let values = shard_values(key, doc)?;
    let id = doc.get("_id").cloned().unwrap_or(Value::Null);
    let holder = values
        .into_iter()
        .enumerate()
        .fold(Document::new(), |d, (i, v)| d.with(i.to_string(), v))
        .with("_id", id)
        .to_bytes()?;
    let raw = RawDoc::from_bytes(&holder)?;
    let parts: Vec<teseladb_bson::RawValue<'_>> = raw.iter().map(|(_, v)| v).collect();
    let (id, shard) = parts.split_last().ok_or_else(|| CommandError::new(BAD_VALUE, "bad _id"))?;
    let k = teseladb_keys::KeyBuilder::document_sharded(tenant, collection, shard, *id);
    Ok(k.as_bytes().to_vec())
}

/// The key a range of `coll` is split at for `at` (`teseladb admin split --at`): the key of a
/// document with that `_id`, and in a collection with a shard key the one of its values in `at`
/// (a missing field `null`) before any `_id` when `at` names none.
pub fn split_point(coll: &CollectionEntry, at: &Document) -> Result<Vec<u8>, CommandError> {
    let Some(k) = shard_key_of(coll)? else {
        let id = at.get("_id").ok_or_else(|| CommandError::new(BAD_VALUE, "--at needs an _id"))?;
        return super::crud::doc_key(coll, id);
    };
    let mut doc = at.clone();
    if doc.get("_id").is_none() {
        doc.push("_id", Value::MinKey);
    }
    document_key(coll, &k.key, &doc)
}

/// The key a change event gives a document of `coll`: the fields of its shard key it has, by
/// their paths, then `_id` (measured on a `refrouter` 9.0.4 on 02-11-2026).
pub fn event_key(coll: &CollectionEntry, doc: &Document) -> Result<Document, CommandError> {
    let mut out = Document::new();
    if let Some(k) = shard_key_of(coll)? {
        for (path, _) in k.key.iter() {
            if let Ok(Some(v)) = shard_value(doc, path) {
                out.push(path, v.clone());
            }
        }
    }
    Ok(out.with("_id", doc.get("_id").cloned().unwrap_or(Value::Null)))
}

/// `enableSharding`: every database can hold sharded collections (TeselaDB keeps no primary
/// shard per database), so it only checks its argument.
pub fn enable_sharding<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    super::admin::admin_only(ENABLE_SHARDING, ctx.db)?;
    match ctx.first() {
        // The messages of a `refrouter` 9.0.4 (measured on 02-11-2026).
        Value::String(db) if db.contains('.') => Err(CommandError::new(
            INVALID_NAMESPACE,
            format!("'.' is an invalid character in a db name: {db}"),
        )),
        Value::String(db)
            if db.is_empty() || db.contains(super::names::DB_FORBIDDEN) || db.contains('$') =>
        {
            Err(CommandError::new(
                INVALID_NAMESPACE,
                format!(
                    "Database {db} could not be created :: caused by :: Invalid db name specified: {db}"
                ),
            ))
        }
        Value::String(_) => Ok(Document::new().with("ok", 1.0)),
        v => Err(wrong_type(ENABLE_SHARDING, v, "string")),
    }
}

/// `shardCollection`.
pub fn shard_collection<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    super::admin::admin_only(SHARD_COLLECTION, ctx.db)?;
    let name = match ctx.first() {
        Value::String(s) => s.clone(),
        v => return Err(wrong_type(SHARD_COLLECTION, v, "string")),
    };
    let ns =
        parse_full(&name).filter(|n| !n.db.is_empty() && !n.coll.is_empty()).ok_or_else(|| {
            CommandError::new(INVALID_NAMESPACE, format!("Invalid namespace specified '{name}'"))
        })?;
    let key = match ctx.cmd.get("key") {
        Some(Value::Document(k)) => k.clone(),
        Some(v) => return Err(wrong_type("shardCollection.key", v, "object")),
        None => return Err(super::args::missing(SHARD_COLLECTION, "key")),
    };
    let unique = matches!(ctx.cmd.get("unique"), Some(Value::Boolean(true)));
    let asked = ShardKey { key, unique };
    check_key(&asked)?;
    check_collation(ctx.cmd.get("collation"))?;
    let special = [super::admin::ADMIN_DB, super::config_db::CONFIG_DB, super::names::LOCAL_DB];
    if special.contains(&ns.db.as_str()) || ns.coll.starts_with(SYSTEM_PREFIX) {
        return Err(CommandError::new(
            ILLEGAL_OPERATION,
            format!(
                "Special collection '{}' can't be registered in the sharding catalog",
                full(&ns)
            ),
        ));
    }
    let coll = ctx.backend.collection(&ns)?;
    if let Some(c) = &coll {
        check_collection(&ns, c, &asked)?;
        if let Some(have) = shard_key_of(c)? {
            if have.key != asked.key {
                return Err(CommandError::new(
                    ALREADY_INITIALIZED,
                    format!(
                        "Collection '{}' already exists with a different shard key. Requested {} but found {}",
                        full(&ns),
                        display(&Value::Document(asked.key)),
                        display(&Value::Document(have.key)),
                    ),
                ));
            }
            return Ok(sharded_reply(&ns));
        }
    }
    let empty = match &coll {
        Some(c) => is_empty(ctx.backend, c)?,
        None => true,
    };
    if coll.is_none() {
        let create = Document::new().with("create", ns.coll.as_str());
        let mut sub =
            Ctx { backend: &mut *ctx.backend, shared: ctx.shared, db: &ns.db, cmd: &create };
        super::catalog::create(&mut sub)?;
    }
    supporting_index(ctx, &ns, &asked, empty)?;
    let c = ctx
        .backend
        .collection(&ns)?
        .ok_or_else(|| CommandError::new(crate::error::NAMESPACE_NOT_FOUND, "no collection"))?;
    let mut options = Document::from(RawDoc::from_bytes(&c.options)?);
    options.push(
        SHARD_KEY,
        Document::new().with("key", asked.key.clone()).with("unique", asked.unique),
    );
    super::write_block::ddl(ctx.backend, &ns.db)?;
    ctx.backend.catalog_write(CatalogCommand::Modify {
        ns: ns.clone(),
        options: options.to_bytes()?,
        // Not a `collMod`: its change event is a `shardCollection` (014, measured on 02-11).
        description: super::change_stream_ddl::shard_description(
            &asked.key,
            asked.unique,
            matches!(options.get("capped"), Some(Value::Boolean(true))),
        )?,
    })?;
    if !empty {
        let after = ctx
            .backend
            .collection(&ns)?
            .ok_or_else(|| CommandError::new(crate::error::NAMESPACE_NOT_FOUND, "no collection"))?;
        rekey(ctx.backend, &ns, &c, &after)?;
    }
    Ok(sharded_reply(&ns))
}

/// Keys again by the shard key the documents `before` kept by their `_id` (018 FR-001): each
/// one leaves its key and its index entries, written without the shard key, and goes in again
/// with them as `after` keeps them, its `_id` in the `_id_` index.
fn rekey<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    before: &CollectionEntry,
    after: &CollectionEntry,
) -> Result<(), CommandError> {
    use super::crud::{Writer, index_rules, rules_of};
    use super::mutation::{Bound, DocMutation, Image};
    // No change stream sees the move: the documents stay what they were.
    let old = super::mutation::Rules {
        unlogged: true,
        ..rules_of(ns, before, true, None, index_rules(backend, before)?)?
    };
    let new = super::mutation::Rules {
        unlogged: true,
        ..rules_of(ns, after, true, None, index_rules(backend, after)?)?
    };
    let (start, end) = super::query::documents_of(before);
    let mut rows = Vec::new();
    let mut next = start;
    loop {
        let chunk = backend.scan(
            &next,
            &end,
            super::query::SCAN_CHUNK,
            teseladb_range::read::ReadConcern::Local,
        )?;
        let short = chunk.len() < super::query::SCAN_CHUNK;
        if let Some((k, _)) = chunk.last() {
            next = [k.as_slice(), &[0]].concat();
        }
        rows.extend(chunk);
        if short {
            break;
        }
    }
    let mut w = Writer::internal();
    for (key, bytes) in rows {
        let doc = Document::from(RawDoc::from_bytes(&bytes)?);
        let id = doc.get("_id").cloned().unwrap_or(Value::Null);
        let filter = Document::new().with("_id", id);
        let gone =
            DocMutation::Delete { filter, multi: false, image: Image::None, rules: old.clone() };
        w.write(backend, &Bound { mutation: gone, keys: vec![key] }, 0)?;
        let at =
            document_key(after, &shard_key_of(after)?.map(|k| k.key).unwrap_or_default(), &doc)?;
        let back = DocMutation::Insert { docs: vec![doc], ordered: true, rules: new.clone() };
        let r = w.write(backend, &Bound { mutation: back, keys: vec![at] }, 0)?;
        if let Some(e) = r.errors.into_iter().next() {
            return Err(e.error);
        }
    }
    Ok(())
}

fn sharded_reply(ns: &Namespace) -> Document {
    Document::new().with("collectionsharded", full(ns)).with("ok", 1.0)
}

/// The shard key's own rules: fields, each `1` or the one `"hashed"`, no `$` part, and no
/// unique hashed key.
fn check_key(k: &ShardKey) -> Result<(), CommandError> {
    if k.key.is_empty() {
        return Err(CommandError::new(BAD_VALUE, "Shard key is empty"));
    }
    let shown = display(&Value::Document(k.key.clone()));
    let mut hashed = 0;
    for (field, v) in k.key.iter() {
        if field.split('.').any(|p| p.starts_with('$')) {
            return Err(CommandError::new(
                BAD_VALUE,
                format!("Field {field} contains parts that start with '$'"),
            ));
        }
        let one = matches!(v, Value::Int32(1) | Value::Int64(1))
            || matches!(v, Value::Double(x) if *x == 1.0);
        let is_hashed = matches!(v, Value::String(s) if s == HASHED);
        if !one && !is_hashed {
            return Err(CommandError::new(
                BAD_VALUE,
                format!(
                    "Shard key {shown} is invalid. Non-hashed fields must be set to a value of 1 (ascending). Failed to parse field {field}"
                ),
            ));
        }
        hashed += usize::from(is_hashed);
        if hashed > 1 {
            return Err(CommandError::new(
                BAD_VALUE,
                format!(
                    "Shard key {shown} can contain at most one 'hashed' field. Failed to parse field {field}"
                ),
            ));
        }
    }
    if k.unique && hashed > 0 {
        return Err(CommandError::new(
            INVALID_OPTIONS,
            "Hashed shard keys cannot be declared unique. It's possible to ensure uniqueness on the hashed field by declaring an additional (non-hashed) unique index on the field.",
        ));
    }
    Ok(())
}

/// Only the simple collation shards a collection.
fn check_collation(c: Option<&Value>) -> Result<(), CommandError> {
    match c {
        None => Ok(()),
        Some(Value::Document(d)) if d.get("locale") == Some(&Value::from("simple")) => Ok(()),
        Some(v) => Err(CommandError::new(
            BAD_VALUE,
            format!(
                "The collation for shardCollection must be {{locale: 'simple'}}, but found: {}",
                display(v)
            ),
        )),
    }
}

/// A view or a capped collection cannot be sharded.
fn check_collection(ns: &Namespace, c: &CollectionEntry, _: &ShardKey) -> Result<(), CommandError> {
    match kinds::kind_of(c)? {
        CollectionKind::View { .. } => Err(CommandError::new(
            COMMAND_NOT_SUPPORTED_ON_VIEW,
            format!(
                "Cannot shard collection '{}' because a view already exists with the same namespace",
                full(ns)
            ),
        )),
        CollectionKind::Capped { .. } => {
            Err(CommandError::new(INVALID_OPTIONS, "can't shard a capped collection"))
        }
        _ => Ok(()),
    }
}

fn is_empty<B: Backend + ?Sized>(
    backend: &mut B,
    c: &CollectionEntry,
) -> Result<bool, CommandError> {
    let (start, end) = super::query::documents_of(c);
    Ok(backend.scan(&start, &end, 1, teseladb_range::read::ReadConcern::Local)?.is_empty())
}

/// The index whose key starts with the shard key: one that exists (unique when the shard key
/// is), or the one `shardCollection` makes on an empty collection; the index of `_id` serves a
/// key `{_id: 1}`.
fn supporting_index<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    k: &ShardKey,
    empty: bool,
) -> Result<(), CommandError> {
    let fields: Vec<(&str, &Value)> = k.key.iter().collect();
    if fields.len() == 1 && fields[0].0 == "_id" && fields[0].1 != &Value::from(HASHED) {
        return Ok(());
    }
    let list = Document::new().with("listIndexes", ns.coll.as_str());
    let mut sub = Ctx { backend: &mut *ctx.backend, shared: ctx.shared, db: &ns.db, cmd: &list };
    let listed = super::catalog::list_indexes(&mut sub)?;
    let specs = match listed.get("cursor") {
        Some(Value::Document(c)) => match c.get("firstBatch") {
            Some(Value::Array(b)) => b.clone(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    let prefix = |spec: &Document| match spec.get("key") {
        Some(Value::Document(ik)) => {
            ik.len() >= fields.len()
                && ik.iter().zip(&fields).all(|((f, v), (sf, sv))| f == *sf && same_value(v, sv))
        }
        _ => false,
    };
    let found: Vec<Document> = specs
        .into_iter()
        .filter_map(|s| match s {
            Value::Document(d) if prefix(&d) => Some(d),
            _ => None,
        })
        .collect();
    if let Some(first) = found.first() {
        let is_unique = found.iter().any(|d| matches!(d.get("unique"), Some(Value::Boolean(true))));
        if k.unique && !is_unique {
            return Err(CommandError::new(
                INVALID_OPTIONS,
                format!(
                    "can't shard collection {}, {} index not unique, and unique index explicitly specified",
                    full(ns),
                    display(first.get("key").unwrap_or(&Value::Null)),
                ),
            ));
        }
        return Ok(());
    }
    if !empty {
        return Err(CommandError::new(
            INVALID_OPTIONS,
            "Please create an index that starts with the proposed shard key before sharding the collection. ",
        ));
    }
    let name: Vec<String> = fields
        .iter()
        .map(|(f, v)| match v {
            Value::String(s) => format!("{f}_{s}"),
            _ => format!("{f}_1"),
        })
        .collect();
    let mut spec = Document::new().with("key", k.key.clone()).with("name", name.join("_"));
    if k.unique {
        spec.push("unique", true);
    }
    let create = Document::new()
        .with("createIndexes", ns.coll.as_str())
        .with("indexes", Value::Array(vec![Value::Document(spec)]));
    let mut sub = Ctx { backend: &mut *ctx.backend, shared: ctx.shared, db: &ns.db, cmd: &create };
    super::indexes::create_indexes(&mut sub).map(drop)
}

/// Whether an index key value and a shard key value say the same: `1` in any numeric type, or
/// `"hashed"`.
fn same_value(index: &Value, shard: &Value) -> bool {
    let number = |v: &Value| match v {
        Value::Int32(n) => Some(f64::from(*n)),
        Value::Int64(n) => Some(*n as f64),
        Value::Double(x) => Some(*x),
        _ => None,
    };
    match (index, shard) {
        (Value::String(a), Value::String(b)) => a == b,
        (a, b) => number(a).is_some_and(|x| x > 0.0) && number(b) == Some(1.0),
    }
}

/// The `config.collections` document of sharded collection `ns`, as a `refrouter` 9.0.4 lists it.
pub fn config_entry(ns: &Namespace, c: &CollectionEntry, k: &ShardKey) -> Document {
    let mut epoch = [0u8; 12];
    epoch.copy_from_slice(&c.uuid[..12]);
    let at = u32::try_from(c.created_index).unwrap_or(u32::MAX);
    Document::new()
        .with("_id", full(ns))
        .with("lastmodEpoch", Value::ObjectId(epoch))
        .with("timestamp", Value::Timestamp { time: at, increment: 1 })
        .with("uuid", Value::Binary { subtype: 4, bytes: c.uuid.to_vec() })
        .with("key", k.key.clone())
        .with("unique", k.unique)
        .with("lastmod", Value::DateTime(0))
        .with("noBalance", false)
}
