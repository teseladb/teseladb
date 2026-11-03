//! `listShards`, `balancerStart`, `balancerStop`, `balancerStatus`,
//! `configureCollectionBalancing` and `balancerCollectionStatus` (018 FR-004, FR-005, FR-018).
//! The shards are the nodes (`config.shards`); the balancer is Group 0's
//! (`teseladb_meta::balancer`), which stops while `config.settings`' `balancer` document says
//! `stopped` or mode `off`; a collection's balancing settings live with its shard key, where
//! `config.collections` shows them and the leaders of its ranges read the size at which they split
//! ([`max_chunk_bytes`]).
//!
//! The replies and messages are those of a `refrouter` 9.0.4 of two shards (measured on 02-11-2026,
//! `c1.js`…`c8.js` in the lane's notes): the commands run only on `admin` (13); `balancerStart`
//! and `balancerStop` take any field and write `{mode: "full", stopped: false}` and
//! `{mode: "off", stopped: true}`; `balancerStatus` answers `mode`, `inBalancerRound`,
//! `numBalancerRounds` and `term`; `configureCollectionBalancing` takes `chunkSize` in MiB,
//! truncated, from 0 to 1024 (0 drops it) as `maxChunkSizeBytes`, `enableAutoMerger` as
//! `enableAutoMerge` and `defragmentCollection`, which stays in `config.collections` until the
//! balancer runs (a defragmentation of TeselaDB's ranges is the merge of small neighbours that
//! the ranges already do, so a running balancer finishes it at once).

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_meta::catalog::{CatalogCommand, CollectionEntry, Namespace};

use super::config_settings::{BALANCER, CHUNK_SIZE, SETTINGS, SettingsWrite};
use super::names::parse_full;
use super::sharding::SHARD_KEY;
use super::{Backend, Ctx, admin};
use crate::error::{
    CommandError, INVALID_NAMESPACE, INVALID_OPTIONS, NAMESPACE_NOT_SHARDED, WRITE_CONFLICT,
};
use crate::types::{wrong_type, wrong_types};

/// `listShards`.
pub const LIST_SHARDS: &str = "listShards";
/// `balancerStart`.
pub const BALANCER_START: &str = "balancerStart";
/// `balancerStop`.
pub const BALANCER_STOP: &str = "balancerStop";
/// `balancerStatus`.
pub const BALANCER_STATUS: &str = "balancerStatus";
/// `configureCollectionBalancing`.
pub const CONFIGURE_COLLECTION_BALANCING: &str = "configureCollectionBalancing";
/// `setAllowMigrations`.
pub const SET_ALLOW_MIGRATIONS: &str = "setAllowMigrations";
/// The balancing field that blocks a collection's migrations (`setAllowMigrations: false`).
const PERMIT_MIGRATIONS: &str = "permitMigrations";
/// `balancerCollectionStatus`.
pub const BALANCER_COLLECTION_STATUS: &str = "balancerCollectionStatus";
/// The balancer's round, as `teseladb_meta::balancer::BALANCE_EVERY` (2 s).
const ROUND_MS: u64 = 2_000;
/// Largest `chunkSize`, in MiB (1 GiB).
const MAX_CHUNK_MB: i64 = 1024;
/// The chunk size of a collection without its own nor a `chunksize` setting, in MiB (9.0.4).
const DEFAULT_CHUNK_MB: i64 = 128;
/// Where a collection keeps its balancing settings, inside its shard key's options.
const BALANCING: &str = "balancing";
/// The field of a catalog change's description that marks a change of the balancing settings,
/// which gives no change stream event.
pub const BALANCING_CHANGE: &str = "$balancing";
/// Attempts before a change of the balancer's settings that keep changing gives up.
const ATTEMPTS: usize = 16;
/// `config.collections`' fields of the balancing settings.
const MAX_CHUNK_SIZE_BYTES: &str = "maxChunkSizeBytes";
const ENABLE_AUTO_MERGE: &str = "enableAutoMerge";
const DEFRAGMENT: &str = "defragmentCollection";
const DEFRAGMENTATION_PHASE: &str = "defragmentationPhase";
/// `defragmentationPhase` once a defragmentation was turned off.
const FINISHED: &str = "finished";
/// The balancer's modes.
const MODE_FULL: &str = "full";
const MODE_OFF: &str = "off";

fn ok() -> Document {
    Document::new().with("ok", 1.0)
}

/// `listShards`: the documents of `config.shards`.
pub fn list_shards<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin::admin_only(LIST_SHARDS, ctx.db)?;
    let draining = draining_filter(ctx.cmd)?;
    let mut shards = ctx.backend.config_documents(super::config_db::SHARDS)?;
    if let Some(want) = draining {
        shards.retain(|d| matches!(d.get("draining"), Some(Value::Boolean(true))) == want);
    }
    let shards = Value::Array(shards.into_iter().map(Value::Document).collect());
    Ok(Document::new().with("shards", shards).with("ok", 1.0))
}

/// `listShards`' `filter`, a structure whose only field is `draining` (measured on 9.0.4).
fn draining_filter(cmd: &Document) -> Result<Option<bool>, CommandError> {
    const PATH: &str = "listShardsRequest.filter";
    let filter = match cmd.get("filter") {
        None => return Ok(None),
        Some(Value::Document(f)) => f,
        Some(v) => return Err(wrong_type(PATH, v, "object")),
    };
    if let Some((k, _)) = filter.iter().find(|(k, _)| *k != "draining") {
        let msg = format!("BSON field '{PATH}.{k}' is an unknown field.");
        return Err(CommandError::new(crate::error::IDL_UNKNOWN_FIELD, msg));
    }
    super::args::bool_field(filter, PATH, "draining")
}

/// The document of `config.settings` with `_id` `id`, if there is one.
fn setting<B: Backend + ?Sized>(
    backend: &mut B,
    id: &str,
) -> Result<Option<Document>, CommandError> {
    let all = backend.config_documents(SETTINGS)?;
    Ok(all.into_iter().find(|d| matches!(d.get("_id"), Some(Value::String(s)) if s == id)))
}

/// Whether the balancer's document stops it.
fn stopped(balancer: Option<&Document>) -> bool {
    balancer.is_some_and(|d| {
        matches!(d.get("mode"), Some(Value::String(m)) if m == MODE_OFF)
            || matches!(d.get("stopped"), Some(Value::Boolean(true)))
    })
}

/// Writes the balancer's `mode` and `stopped`, keeping its other fields.
fn set_balancer<B: Backend + ?Sized>(
    backend: &mut B,
    mode: &str,
    stop: bool,
) -> Result<(), CommandError> {
    for _ in 0..ATTEMPTS {
        let read = setting(backend, BALANCER)?;
        let mut now = read.clone().unwrap_or_else(|| Document::new().with("_id", BALANCER));
        now.insert("mode", mode);
        now.insert("stopped", stop);
        let w = SettingsWrite { id: Value::from(BALANCER), read, now: Some(now) };
        if backend.config_settings_write(vec![w])? {
            return Ok(());
        }
    }
    Err(CommandError::new(WRITE_CONFLICT, "the balancer's settings kept changing"))
}

/// `balancerStart`: the balancer runs again and finishes the defragmentations asked meanwhile.
pub fn balancer_start<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin::admin_only(BALANCER_START, ctx.db)?;
    set_balancer(ctx.backend, MODE_FULL, false)?;
    for (ns, c) in ctx.backend.collections()? {
        let Some(mut b) = balancing_of(&c)? else { continue };
        if b.settings.get(DEFRAGMENT).is_none() {
            continue;
        }
        b.settings.remove(DEFRAGMENT);
        b.settings.remove(DEFRAGMENTATION_PHASE);
        write_balancing(ctx.backend, ns, b)?;
    }
    Ok(ok())
}

/// `balancerStop`.
pub fn balancer_stop<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin::admin_only(BALANCER_STOP, ctx.db)?;
    set_balancer(ctx.backend, MODE_OFF, true)?;
    Ok(ok())
}

/// `balancerStatus`: `off` when stopped; the rounds are those of a balancer that looks at the
/// cluster every 2 s since this node started.
pub fn balancer_status<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    admin::admin_only(BALANCER_STATUS, ctx.db)?;
    let off = stopped(setting(ctx.backend, BALANCER)?.as_ref());
    let facts = ctx.backend.process();
    let rounds = facts.now_ms.saturating_sub(facts.start_ms) / ROUND_MS;
    Ok(Document::new()
        .with("mode", if off { MODE_OFF } else { MODE_FULL })
        .with("inBalancerRound", false)
        .with("numBalancerRounds", i64::try_from(rounds).unwrap_or(i64::MAX))
        .with("term", 1i64)
        .with("ok", 1.0))
}

/// `chunkSize` of `configureCollectionBalancing`, in whole MiB (truncated, as 9.0.4 does).
fn chunk_size(cmd: &Document) -> Result<Option<i64>, CommandError> {
    let path = format!("{CONFIGURE_COLLECTION_BALANCING}.chunkSize");
    let mb = match cmd.get("chunkSize") {
        None => return Ok(None),
        Some(Value::Int32(i)) => i64::from(*i),
        Some(Value::Int64(i)) => *i,
        Some(Value::Double(d)) => d.trunc() as i64,
        Some(Value::Decimal128(d)) => teseladb_mql::decimal_saturating(*d),
        Some(v) => return Err(wrong_types(&path, v, &["decimal", "double", "long", "int"])),
    };
    if !(0..=MAX_CHUNK_MB).contains(&mb) {
        let msg = format!("Chunk size '{mb}' out of range [1MB, 1GB]");
        return Err(CommandError::new(INVALID_OPTIONS, msg));
    }
    Ok(Some(mb))
}

/// The namespace a command names in its first field.
fn namespace(ctx: &Ctx<'_, impl Backend + ?Sized>, name: &str) -> Result<Namespace, CommandError> {
    let full = match ctx.first() {
        Value::String(s) => s.clone(),
        v => return Err(wrong_type(&format!("{name}.{name}"), v, "string")),
    };
    // A `refrouter` 9.0.4 names `db.` by the database (measured on 03-11-2026).
    let shown = full.strip_suffix('.').unwrap_or(&full);
    parse_full(&full).filter(|n| !n.db.is_empty() && !n.coll.is_empty()).ok_or_else(|| {
        CommandError::new(INVALID_NAMESPACE, format!("Invalid namespace specified '{shown}'"))
    })
}

/// A collection with a shard key as its balancing settings live: its options, its shard key's
/// document and the settings kept there.
struct Balancing {
    options: Document,
    key: Document,
    settings: Document,
}

/// The balancing settings of a collection, if it has a shard key.
fn balancing_of(c: &CollectionEntry) -> Result<Option<Balancing>, CommandError> {
    let options = Document::from(RawDoc::from_bytes(&c.options)?);
    let Some(Value::Document(key)) = options.get(SHARD_KEY).cloned() else { return Ok(None) };
    let settings = match key.get(BALANCING) {
        Some(Value::Document(b)) => b.clone(),
        _ => Document::new(),
    };
    Ok(Some(Balancing { options, key, settings }))
}

/// Stores the settings of `b` with the shard key of collection `ns`.
fn write_balancing<B: Backend + ?Sized>(
    backend: &mut B,
    ns: Namespace,
    b: Balancing,
) -> Result<(), CommandError> {
    let Balancing { mut options, mut key, settings } = b;
    key.insert(BALANCING, settings);
    options.insert(SHARD_KEY, key);
    backend.catalog_write(CatalogCommand::Modify {
        ns,
        options: options.to_bytes()?,
        description: Document::new().with(BALANCING_CHANGE, true).to_bytes()?,
    })?;
    Ok(())
}

/// The refusal of a collection without a shard key.
fn not_sharded(ns: &Namespace) -> CommandError {
    let msg =
        format!("Expected to match one doc for query {{ _id: \"{}\" }}", super::names::full(ns));
    CommandError::new(NAMESPACE_NOT_SHARDED, msg)
}

/// `configureCollectionBalancing`.
pub fn configure_collection_balancing<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let name = CONFIGURE_COLLECTION_BALANCING;
    admin::admin_only(name, ctx.db)?;
    let ns = namespace(ctx, name)?;
    let chunk_mb = chunk_size(ctx.cmd)?;
    let auto_merge = super::args::bool_field(ctx.cmd, name, "enableAutoMerger")?;
    let defragment = super::args::bool_field(ctx.cmd, name, DEFRAGMENT)?;
    let coll = ctx.backend.collection(&ns)?.ok_or_else(|| not_sharded(&ns))?;
    let mut b = balancing_of(&coll)?.ok_or_else(|| not_sharded(&ns))?;
    let balancing = &mut b.settings;
    match chunk_mb {
        Some(0) => {
            balancing.remove(MAX_CHUNK_SIZE_BYTES);
        }
        Some(mb) => {
            balancing.insert(MAX_CHUNK_SIZE_BYTES, mb << 20);
        }
        None => {}
    }
    if let Some(on) = auto_merge {
        balancing.insert(ENABLE_AUTO_MERGE, on);
    }
    let running = !stopped(setting(ctx.backend, BALANCER)?.as_ref());
    match defragment {
        Some(true) if !running => {
            balancing.insert(DEFRAGMENT, true);
        }
        Some(false) if balancing.get(DEFRAGMENT).is_some() => {
            balancing.insert(DEFRAGMENTATION_PHASE, FINISHED);
        }
        _ => {}
    }
    write_balancing(ctx.backend, ns, b)?;
    Ok(ok())
}

/// `balancerCollectionStatus`: the collection's chunk size in MiB and whether it is as the
/// balancer wants it (a defragmentation waiting for the balancer is not).
pub fn balancer_collection_status<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let name = BALANCER_COLLECTION_STATUS;
    admin::admin_only(name, ctx.db)?;
    let ns = namespace(ctx, name)?;
    let unsharded =
        || CommandError::new(NAMESPACE_NOT_SHARDED, "Collection unsharded or undefined");
    let coll = ctx.backend.collection(&ns)?.ok_or_else(unsharded)?;
    let balancing = balancing_of(&coll)?.ok_or_else(unsharded)?.settings;
    let mb = match balancing.get(MAX_CHUNK_SIZE_BYTES) {
        Some(Value::Int64(b)) => b >> 20,
        _ => match setting(ctx.backend, CHUNK_SIZE)?.and_then(|d| d.get("value").cloned()) {
            Some(Value::Int32(v)) => i64::from(v),
            Some(Value::Int64(v)) => v,
            Some(Value::Double(v)) => v as i64,
            _ => DEFAULT_CHUNK_MB,
        },
    };
    let mut reply = Document::new().with("chunkSize", i32::try_from(mb).unwrap_or(i32::MAX));
    match balancing.get(DEFRAGMENT) {
        Some(_) => {
            reply.push("balancerCompliant", false);
            reply.push("firstComplianceViolation", "defragmentingChunks");
            reply.push("details", Document::new().with("currentPhase", "none"));
        }
        None => reply.push("balancerCompliant", true),
    }
    reply.push("ok", 1.0);
    Ok(reply)
}

/// The balancing fields `config.collections` shows after `noBalance` for collection options
/// `options`, in the order they were set.
pub fn config_fields(options: &[u8]) -> Document {
    let Ok(raw) = RawDoc::from_bytes(options) else { return Document::new() };
    let Some(RawValue::Document(key)) = raw.get(SHARD_KEY) else { return Document::new() };
    match key.get(BALANCING) {
        Some(RawValue::Document(b)) => Document::from(b),
        _ => Document::new(),
    }
}

/// The size at which the ranges of a collection with options `options` split, if it has its own
/// (`configureCollectionBalancing`'s `chunkSize`).
pub fn max_chunk_bytes(options: &[u8]) -> Option<u64> {
    match config_fields(options).get(MAX_CHUNK_SIZE_BYTES) {
        Some(Value::Int64(b)) => u64::try_from(*b).ok(),
        _ => None,
    }
}

/// Whether a catalog change's description is a change of the balancing settings.
pub fn is_balancing_change(description: &[u8]) -> bool {
    RawDoc::from_bytes(description).is_ok_and(|d| d.get(BALANCING_CHANGE).is_some())
}

/// `setAllowMigrations`: blocks or allows the migrations of a collection with a shard key;
/// `config.collections` shows `permitMigrations: false` while they are blocked.
pub fn set_allow_migrations<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let name = SET_ALLOW_MIGRATIONS;
    let text = match ctx.first() {
        Value::String(s) => s.clone(),
        v => return Err(wrong_type(&format!("{name}.{name}"), v, "string")),
    };
    let allow = match ctx.cmd.get("allowMigrations") {
        Some(Value::Boolean(b)) => *b,
        Some(v) => return Err(wrong_type(&format!("{name}.allowMigrations"), v, "bool")),
        None => return Err(super::args::missing(name, "allowMigrations")),
    };
    admin::admin_only(name, ctx.db)?;
    let ns =
        parse_full(&text).filter(|n| !n.db.is_empty() && !n.coll.is_empty()).ok_or_else(|| {
            let shown = text.strip_suffix('.').unwrap_or(&text);
            CommandError::new(INVALID_NAMESPACE, format!("Invalid namespace specified: {shown}"))
        })?;
    let unsharded = || {
        CommandError::new(
            NAMESPACE_NOT_SHARDED,
            "Collection must be sharded so migrations can be blocked",
        )
    };
    let coll = ctx.backend.collection(&ns)?.ok_or_else(unsharded)?;
    let mut b = balancing_of(&coll)?.ok_or_else(unsharded)?;
    let balancing = &mut b.settings;
    let blocked = balancing.get(PERMIT_MIGRATIONS).is_some();
    if blocked == !allow {
        return Ok(ok());
    }
    match allow {
        true => {
            balancing.remove(PERMIT_MIGRATIONS);
        }
        false => balancing.push(PERMIT_MIGRATIONS, false),
    }
    write_balancing(ctx.backend, ns, b)?;
    Ok(ok())
}

/// Whether the migrations of collection `c` are allowed (`setAllowMigrations`).
pub fn migrations_allowed(c: &CollectionEntry) -> bool {
    config_fields(&c.options).get(PERMIT_MIGRATIONS).is_none()
}
