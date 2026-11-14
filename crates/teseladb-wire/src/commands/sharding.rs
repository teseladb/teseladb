//! `enableSharding` and `shardCollection` (018 FR-001): the shard key of a collection, checked
//! with the messages of a `refrouter` 9.0.4 (measured on 01-11-2026), kept with the collection's
//! options under [`SHARD_KEY`] (not a collection option: `listCollections` does not show it) and
//! listed in `config.collections`.

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_meta::catalog::{CatalogCommand, CollectionEntry, Namespace};
use teseladb_meta::collection_types::CollectionKind;
use teseladb_meta::indexes::{IndexEntry, IndexState};

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

/// Whether `kind`, the value of a field of a shard key, makes it hashed.
pub(super) fn is_hashed(kind: &Value) -> bool {
    *kind == Value::from(HASHED)
}

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
    let key = super::args::doc(&d, SHARD_KEY, "key")?.cloned().unwrap_or_default();
    Ok(Some(ShardKey { key, unique: matches!(d.get("unique"), Some(Value::Boolean(true))) }))
}

/// The size of a chunk of `config.system.sessions` in a `refrouter` 9.0.4's `config.collections`.
const SESSIONS_CHUNK_BYTES: i64 = 200_000;

/// Whether `ns` is the cluster's record of sessions, `config.system.sessions`, which a cluster of
/// RefDb shards by `{_id: 1}` (006 FR-038). Its documents stay keyed as in any collection
/// without a shard key; it is presented as sharded, in one chunk.
pub fn is_sessions_record(ns: &Namespace) -> bool {
    ns.db == super::registry::SESSIONS_DB && ns.coll == super::registry::SESSIONS_COLL
}

/// The shard key the record of sessions is presented with.
pub fn sessions_key() -> ShardKey {
    ShardKey { key: Document::new().with("_id", 1), unique: false }
}

/// The shard key collection `ns` is presented with: its own, or that of the record of sessions.
pub fn presented_key(
    ns: &Namespace,
    c: &CollectionEntry,
) -> Result<Option<ShardKey>, CommandError> {
    Ok(shard_key_of(c)?.or_else(|| is_sessions_record(ns).then(sessions_key)))
}

/// Whether `coll` has a shard key: its documents are keyed by it ([`document_key`]).
pub fn is_sharded(coll: &CollectionEntry) -> bool {
    RawDoc::from_bytes(&coll.options).is_ok_and(|o| o.get(SHARD_KEY).is_some())
}

/// The value at dotted `path` of `doc`: `None` when it is missing; an array on the way is
/// refused, as no shard key holds one (measured on a `refrouter` 9.0.4 on 02-11-2026).
fn shard_value<'a>(doc: &'a Document, path: &str) -> Result<Option<&'a Value>, CommandError> {
    let array = || {
        let msg = "Shard key cannot contain array values or array descendants.";
        CommandError::new(SHARD_KEY_NOT_FOUND, msg)
    };
    let mut parts = path.split('.');
    // `split` gives at least one part.
    let last = parts.next_back().unwrap_or_default();
    let mut at = doc;
    for part in parts {
        match at.get(part) {
            Some(Value::Array(_)) => return Err(array()),
            Some(Value::Document(d)) => at = d,
            _ => return Ok(None),
        }
    }
    match at.get(last) {
        Some(Value::Array(_)) => Err(array()),
        v => Ok(v),
    }
}

/// The values of shard key `key` in `doc`, in order, as its document's key holds them: a
/// missing field is `null` and a hashed field its hash.
pub fn shard_values(key: &Document, doc: &Document) -> Result<Vec<Value>, CommandError> {
    let mut out = Vec::new();
    for (path, kind) in key.iter() {
        let v = shard_value(doc, path)?.cloned().unwrap_or(Value::Null);
        if !is_hashed(kind) {
            out.push(v);
            continue;
        }
        let holder = Document::new().with("v", v).to_bytes()?;
        let raw = RawDoc::from_bytes(&holder)?.get("v").unwrap_or(teseladb_bson::RawValue::Null);
        out.push(Value::Int64(teseladb_index::keygen::hashed_key(raw)));
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
    // The holder ends with `_id`, so it has a last value.
    let (id, shard) = parts.split_last().unwrap_or((&teseladb_bson::RawValue::Null, &[]));
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

/// `enableSharding`: every database can hold sharded collections; it records the database and,
/// with `primaryShard`, its primary ([`super::primaries`]).
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
        Value::String(db) => {
            let db = db.clone();
            super::primaries::enable_with_primary(ctx, &db)?;
            Ok(Document::new().with("ok", 1.0))
        }
        v => Err(wrong_type(&format!("{ENABLE_SHARDING}.{ENABLE_SHARDING}"), v, "string")),
    }
}

/// `shardCollection`.
pub fn shard_collection<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    super::admin::admin_only(SHARD_COLLECTION, ctx.db)?;
    // The messages of a `refrouter` 9.0.4 (measured on 03-11-2026).
    let name = match ctx.first() {
        Value::String(s) => s.clone(),
        v => {
            let msg = format!("Collection name has invalid type {}", crate::types::type_name(v));
            return Err(CommandError::new(INVALID_NAMESPACE, msg));
        }
    };
    let ns =
        parse_full(&name).filter(|n| !n.db.is_empty() && !n.coll.is_empty()).ok_or_else(|| {
            CommandError::new(INVALID_NAMESPACE, format!("Invalid namespace specified '{name}'"))
        })?;
    let key = match ctx.cmd.get("key") {
        Some(Value::Document(k)) => k.clone(),
        Some(v) => return Err(wrong_type(".key", v, "object")),
        None => return Err(super::args::missing("", "key")),
    };
    let unique = matches!(ctx.cmd.get("unique"), Some(Value::Boolean(true)));
    let asked = ShardKey { key, unique };
    check_key(&asked)?;
    check_collation(SHARD_COLLECTION, ctx.cmd.get("collation"))?;
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
    let c = ctx.backend.collection(&ns)?.ok_or_else(no_collection)?;
    supporting_index(ctx, (&ns, &c), &asked, empty)?;
    let mut options = Document::from(RawDoc::from_bytes(&c.options)?);
    options.push(
        SHARD_KEY,
        Document::new().with("key", asked.key.clone()).with("unique", asked.unique),
    );
    super::write_block::ddl(ctx.backend, &ns.db)?;
    // Not a `collMod`: its change event is a `shardCollection` (014, measured on 02-11).
    let sharded = super::change_stream_ddl::shard_description(
        &asked.key,
        asked.unique,
        matches!(options.get("capped"), Some(Value::Boolean(true))),
    )?;
    let options = options.to_bytes()?;
    // A collection with documents takes its shard key quietly and gives its event once every
    // document has its new key: a call that fails meanwhile is no change for a stream.
    let first = if empty { sharded.clone() } else { super::balancing::quiet_description()? };
    ctx.backend.catalog_write(CatalogCommand::Modify {
        ns: ns.clone(),
        options: options.clone(),
        description: first,
    })?;
    let after = ctx.backend.collection(&ns)?.ok_or_else(no_collection)?;
    if !empty {
        if let Err(e) = rekey(ctx.backend, &ns, &c, &after) {
            // The collection goes back to its options without the shard key.
            ctx.backend.catalog_write(CatalogCommand::Modify {
                ns: ns.clone(),
                options: c.options.clone(),
                description: super::balancing::quiet_description()?,
            })?;
            return Err(e);
        }
        ctx.backend.catalog_write(CatalogCommand::Modify {
            ns: ns.clone(),
            options,
            description: sharded,
        })?;
    }
    // The zones' intervals given before take the shard key's keys (018 FR-003).
    super::zones::rekey(ctx.backend, &ns, &after)?;
    Ok(sharded_reply(&ns))
}

/// Keys again by the shard key the documents `before` kept by their `_id` (018 FR-001): each
/// one leaves its key and its index entries, written without the shard key, and goes in again
/// with them as `after` keeps them, its `_id` in the `_id_` index. A write that fails puts the
/// documents moved so far back where they were.
fn rekey<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    before: &CollectionEntry,
    after: &CollectionEntry,
) -> Result<(), CommandError> {
    use super::crud::{index_rules, rules_of};
    // No change stream or oplog sees the move, the search indexes do: the documents stay what
    // they were at new keys.
    let old = super::mutation::Rules {
        rekeys: true,
        ..rules_of(ns, before, true, None, index_rules(backend, before)?)?
    };
    let new = super::mutation::Rules {
        rekeys: true,
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
    // Every new key first: a document whose shard key cannot be keyed moves nothing.
    let key = shard_key_of(after)?.map(|k| k.key).unwrap_or_default();
    let mut moves = Vec::new();
    for (from, bytes) in rows {
        let doc = Document::from(RawDoc::from_bytes(&bytes)?);
        moves.push((from, document_key(after, &key, &doc)?, doc));
    }
    let mut moved: Vec<(Vec<u8>, Vec<u8>, Document)> = Vec::new();
    for (from, to, doc) in moves {
        if let Err(e) = move_document(backend, (&old, &from), (&new, &to), &doc) {
            // The document itself may have left its key: it goes back with the others.
            moved.push((from, to, doc));
            for (from, to, doc) in moved.into_iter().rev() {
                move_document(backend, (&new, &to), (&old, &from), &doc)?;
            }
            return Err(e);
        }
        moved.push((from, to, doc));
    }
    Ok(())
}

/// Moves `doc` from key `from` (written with rules `old`) to key `to` (with rules `new`); a
/// document already gone from `from`, or already at `to`, is left as it is.
fn move_document<B: Backend + ?Sized>(
    backend: &mut B,
    (old, from): (&super::mutation::Rules, &[u8]),
    (new, to): (&super::mutation::Rules, &[u8]),
    doc: &Document,
) -> Result<(), CommandError> {
    use super::crud::Writer;
    use super::mutation::{Bound, DocMutation, Image};
    let mut w = Writer::internal();
    let id = doc.get("_id").cloned().unwrap_or(Value::Null);
    let filter = Document::new().with("_id", id);
    let gone = DocMutation::Delete { filter, multi: false, image: Image::None, rules: old.clone() };
    w.write(backend, &Bound { mutation: gone, keys: vec![from.to_vec()] }, 0)?;
    let back = DocMutation::Insert { docs: vec![doc.clone()], ordered: true, rules: new.clone() };
    let r = w.write(backend, &Bound { mutation: back, keys: vec![to.to_vec()] }, 0)?;
    match r.errors.into_iter().next() {
        Some(e) if e.error.code != crate::error::DUPLICATE_KEY => Err(e.error),
        _ => Ok(()),
    }
}

/// The collection `shardCollection` works on was dropped meanwhile by another client.
fn no_collection() -> CommandError {
    CommandError::new(
        crate::error::NAMESPACE_NOT_FOUND,
        "the collection was dropped while it was being sharded",
    )
}

fn sharded_reply(ns: &Namespace) -> Document {
    Document::new().with("collectionsharded", full(ns)).with("ok", 1.0)
}

/// The shard key's own rules: fields, each `1` or the one `"hashed"`, no `$` part, and no
/// unique hashed key.
pub(super) fn check_key(k: &ShardKey) -> Result<(), CommandError> {
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
        if crate::types::number(v) != Some(1.0) && !is_hashed(v) {
            return Err(CommandError::new(
                BAD_VALUE,
                format!(
                    "Shard key {shown} is invalid. Non-hashed fields must be set to a value of 1 (ascending). Failed to parse field {field}"
                ),
            ));
        }
        hashed += usize::from(is_hashed(v));
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
pub(super) fn check_collation(name: &str, c: Option<&Value>) -> Result<(), CommandError> {
    match c {
        None => Ok(()),
        Some(Value::Document(d)) if d.get("locale") == Some(&Value::from("simple")) => Ok(()),
        Some(v) => Err(CommandError::new(
            BAD_VALUE,
            format!(
                "The collation for {name} must be {{locale: 'simple'}}, but found: {}",
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

pub(super) fn is_empty<B: Backend + ?Sized>(
    backend: &mut B,
    c: &CollectionEntry,
) -> Result<bool, CommandError> {
    let (start, end) = super::query::documents_of(c);
    Ok(backend.scan(&start, &end, 1, teseladb_range::read::ReadConcern::Local)?.is_empty())
}

/// The index whose key starts with the shard key: one that exists (unique when the shard key
/// is), or the one `shardCollection` makes on empty collection `c`; the index of `_id` serves a
/// key `{_id: 1}`. An index serves when it is not sparse, partial, hidden or multikey; without
/// one, an index on exactly the shard key that is not sparse refuses the command with each
/// index's reason, and a collection with documents asks for an index (measured on a `refrouter`
/// 9.0.4 on 03-11-2026, `sh3.js` to `sh6.js`).
fn supporting_index<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    (ns, c): (&Namespace, &CollectionEntry),
    k: &ShardKey,
    empty: bool,
) -> Result<(), CommandError> {
    let fields: Vec<(&str, &Value)> = k.key.iter().collect();
    if fields.len() == 1 && fields[0].0 == "_id" && fields[0].1 != &Value::from(HASHED) {
        return Ok(());
    }
    let flag = |spec: &Document, f: &str| matches!(spec.get(f), Some(Value::Boolean(true)));
    // The index of `_id` comes first and never starts with another shard key.
    let mut reasons = vec![
        "Index _id_ cannot be used for sharding because: Shard key is not a prefix of index key."
            .to_owned(),
    ];
    let mut on_key = false;
    let mut sparse = None;
    let mut serving: Vec<Document> = Vec::new();
    let ready = ctx.backend.indexes(c.id)?.into_iter().filter(|e| e.state == IndexState::Ready);
    for entry in ready.collect::<Vec<_>>() {
        let spec = Document::from(RawDoc::from_bytes(&entry.spec)?);
        let ik = super::args::doc(&spec, "index", "key")?.cloned().unwrap_or_default();
        let is_prefix = ik.len() >= fields.len()
            && ik.iter().zip(&fields).all(|((f, v), (sf, sv))| f == *sf && same_value(v, sv));
        let hidden = flag(&spec, "hidden");
        on_key |= is_prefix && ik.len() == fields.len() && !flag(&spec, "sparse");
        let why = match () {
            () if !is_prefix => Some("Shard key is not a prefix of index key."),
            () if flag(&spec, "sparse") => {
                sparse = Some(entry.name.clone());
                None
            }
            () if spec.get("partialFilterExpression").is_some() => Some("Index key is partial."),
            () if is_multikey(ctx.backend, c, &entry, &ik)? => Some("Index key is multikey."),
            () if hidden => None,
            () => {
                serving.push(spec.clone());
                None
            }
        };
        if let (Some(why), false) = (why, hidden) {
            let name = &entry.name;
            reasons.push(format!("Index {name} cannot be used for sharding because: {why}"));
        }
    }
    if let Some(first) = serving.first() {
        let is_unique = serving.iter().any(|d| flag(d, "unique"));
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
    if on_key {
        let msg = format!(
            "can't shard collection because an index already exists on the shard key and it's a non shard key valid index. :: caused by :: couldn't find valid index for shard key\n{}",
            reasons.join("\n")
        );
        return Err(CommandError::new(INVALID_OPTIONS, msg));
    }
    if !empty {
        let mut msg = "Please create an index that starts with the proposed shard key before sharding the collection. ".to_owned();
        if let Some(name) = sparse {
            msg.push_str(&format!(
                " Index {name} cannot be used for sharding because [ Index key is sparse. ]"
            ));
        }
        return Err(CommandError::new(INVALID_OPTIONS, msg));
    }
    create_supporting(ctx, ns, k)
}

/// Whether index `entry` of collection `c` holds an array in one of the fields of `key`.
fn is_multikey<B: Backend + ?Sized>(
    backend: &mut B,
    c: &CollectionEntry,
    entry: &IndexEntry,
    key: &Document,
) -> Result<bool, CommandError> {
    let paths = super::plan_exec::multikey_paths(backend, (c.tenant, c.id), entry, key)?;
    Ok(paths.iter().any(|(_, p)| !p.is_empty()))
}

/// Builds the index of shard key `k` on `ns`, named after its fields.
fn create_supporting<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    k: &ShardKey,
) -> Result<(), CommandError> {
    let name: Vec<String> = k
        .key
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
pub(super) fn same_value(index: &Value, shard: &Value) -> bool {
    match (index, shard) {
        (Value::String(a), Value::String(b)) => a == b,
        // Only an ascending field of 1, in any numeric type, serves (measured on 03-11-2026).
        (a, b) => crate::types::number(a) == Some(1.0) && crate::types::number(b) == Some(1.0),
    }
}

/// The `config.collections` document of sharded collection `ns`, as a `refrouter` 9.0.4 lists it.
pub fn config_entry(ns: &Namespace, c: &CollectionEntry, k: &ShardKey) -> Document {
    let mut epoch = [0u8; 12];
    epoch.copy_from_slice(&c.uuid[..12]);
    let at = u32::try_from(c.created_index).unwrap_or(u32::MAX);
    let mut d = Document::new()
        .with("_id", full(ns))
        .with("lastmodEpoch", Value::ObjectId(epoch))
        .with("timestamp", Value::Timestamp { time: at, increment: 1 })
        .with("uuid", Value::Binary { subtype: 4, bytes: c.uuid.to_vec() })
        .with("key", k.key.clone())
        .with("unique", k.unique)
        .with("lastmod", Value::DateTime(0))
        .with("noBalance", false);
    for (k, v) in super::balancing::config_fields(&c.options).iter() {
        d.push(k, v.clone());
    }
    d
}

/// The `config.collections` document of collection `ns`, if the cluster tracks it: with its
/// shard key, or with key `{_id: 1}` and `unsplittable: true` once `moveCollection` or
/// `unshardCollection` left it without one, the field where each command puts it (measured on a
/// `refrouter` 9.0.4 on 02-11-2026).
pub fn tracked_entry(
    ns: &Namespace,
    c: &CollectionEntry,
) -> Result<Option<Document>, CommandError> {
    if let Some(k) = shard_key_of(c)? {
        return Ok(Some(config_entry(ns, c, &k)));
    }
    if is_sessions_record(ns) {
        let entry = config_entry(ns, c, &sessions_key());
        return Ok(Some(entry.with("maxChunkSizeBytes", Value::Int64(SESSIONS_CHUNK_BYTES))));
    }
    let Some(by) = super::move_collection::unsplittable_by(c) else { return Ok(None) };
    let id = ShardKey { key: Document::new().with("_id", 1), unique: false };
    let entry = config_entry(ns, c, &id);
    if by != super::move_collection::MOVE_COLLECTION {
        return Ok(Some(entry.with("unsplittable", true)));
    }
    let mut out = Document::new();
    for (k, v) in entry.iter() {
        if k == "lastmod" {
            out.push("unsplittable", true);
        }
        out.push(k, v.clone());
    }
    Ok(Some(out))
}
