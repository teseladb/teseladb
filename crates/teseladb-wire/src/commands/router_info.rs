//! What a `refrouter` answers about its routing (018 FR-017, T022): `getShardMap`,
//! `getShardVersion`, `flushRouterConfig`, `checkMetadataConsistency`, `appendOplogNote` and
//! `transitionToDedicatedConfigServer`. Every node routes with Group 0's copy it applied, so there
//! is no router cache to flush nor metadata of the shards to disagree with it; the config server
//! is Group 0 itself, named `config` as `refrouter` names a dedicated one.
//!
//! The replies, messages and codes are those of a `refrouter` 9.0.4 of two shards (measured on
//! 02-11-2026, `g1.js` and `g2.js` in the lane's notes).

use teseladb_bson::{Document, Value};

use super::chunks::collection_chunks;
use super::names::parse_full;
use super::sharding::{config_entry, shard_key_of};
use super::{Backend, Ctx, admin};
use crate::error::{BAD_VALUE, CommandError, NAMESPACE_NOT_FOUND, NO_SUCH_KEY, TYPE_MISMATCH};

/// `getShardMap`.
pub const GET_SHARD_MAP: &str = "getShardMap";
/// `getShardVersion`.
pub const GET_SHARD_VERSION: &str = "getShardVersion";
/// `flushRouterConfig`.
pub const FLUSH_ROUTER_CONFIG: &str = "flushRouterConfig";
/// `checkMetadataConsistency`.
pub const CHECK_METADATA_CONSISTENCY: &str = "checkMetadataConsistency";
/// `appendOplogNote`.
pub const APPEND_OPLOG_NOTE: &str = "appendOplogNote";
/// `transitionToDedicatedConfigServer`.
pub const TRANSITION_TO_DEDICATED_CONFIG_SERVER: &str = "transitionToDedicatedConfigServer";
/// The config server's shard name.
const CONFIG: &str = "config";

/// The shards as `config.shards` lists them: name and client address.
fn shards<B: Backend + ?Sized>(backend: &mut B) -> Result<Vec<(String, String)>, CommandError> {
    let text = |d: &Document, f: &str| match d.get(f) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    Ok(backend
        .config_documents(super::config_db::SHARDS)?
        .iter()
        .map(|d| (text(d, "_id"), text(d, "host")))
        .collect())
}

/// `getShardMap`: each shard's address, and `config`, Group 0, at every node's address.
pub fn get_shard_map<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin::admin_only(GET_SHARD_MAP, ctx.db)?;
    let all = shards(ctx.backend)?;
    let config = all.iter().map(|(_, h)| h.as_str()).collect::<Vec<_>>().join(",");
    let (mut map, mut hosts, mut conns) = (Document::new(), Document::new(), Document::new());
    for (name, host) in &all {
        map.push(name, host.as_str());
        hosts.push(host, name.as_str());
        conns.push(host, name.as_str());
    }
    map.push(CONFIG, config.as_str());
    conns.push(&config, CONFIG);
    let mark = ctx.backend.cluster_mark();
    let (t, i) = teseladb_hlc::to_timestamp(mark);
    let time = format!("{{ forceReloadIncrement: 0, topologyTime: Timestamp({t}, {i}) }}");
    Ok(Document::new()
        .with("timeInStore", time)
        .with("map", map)
        .with("hosts", hosts)
        .with("connStrings", conns)
        .with("ok", 1.0))
}

/// `getShardVersion`: of a database, its primary and version; of a collection with a shard key,
/// the version of its chunks (`fullMetadata`: with them).
pub fn get_shard_version<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    admin::admin_only(GET_SHARD_VERSION, ctx.db)?;
    let name = match ctx.first() {
        Value::String(s) => s.clone(),
        v => {
            let msg = format!("namespace has invalid type {}", crate::types::type_name(v));
            return Err(CommandError::new(BAD_VALUE, msg));
        }
    };
    let ns = parse_full(&name).filter(|n| !n.coll.is_empty());
    let Some(ns) = ns else {
        let db = ctx
            .backend
            .config_documents("databases")?
            .into_iter()
            .find(|d| matches!(d.get("_id"), Some(Value::String(s)) if *s == name));
        let Some(db) = db else {
            return Err(CommandError::new(
                NAMESPACE_NOT_FOUND,
                format!("database {name} not found"),
            ));
        };
        let mut out = Document::new();
        for f in ["primary", "version"] {
            let key = if f == "primary" { "primaryShard" } else { f };
            out.push(key, db.get(f).cloned().unwrap_or(Value::Null));
        }
        return Ok(out.with("ok", 1.0));
    };
    if !ctx.backend.databases()?.contains(&ns.db) {
        return Err(CommandError::new(
            NAMESPACE_NOT_FOUND,
            format!("database {} not found", ns.db),
        ));
    }
    let no_table = || {
        let msg = format!("Collection {name} does not have a routing table.");
        CommandError::new(NAMESPACE_NOT_FOUND, msg)
    };
    let coll = ctx.backend.collection(&ns)?.ok_or_else(no_table)?;
    let key = shard_key_of(&coll)?.ok_or_else(no_table)?;
    let chunks = collection_chunks(&ctx.backend.route_entries()?, &coll, &key);
    let entry = config_entry(&ns, &coll, &key);
    let minor = u32::try_from(chunks.len().saturating_sub(1)).unwrap_or(u32::MAX);
    let mut out = Document::new()
        .with("version", Value::Timestamp { time: 1, increment: minor })
        .with("versionEpoch", entry.get("lastmodEpoch").cloned().unwrap_or(Value::Null))
        .with("versionTimestamp", entry.get("timestamp").cloned().unwrap_or(Value::Null));
    if matches!(ctx.cmd.get("fullMetadata"), Some(Value::Boolean(true))) {
        let list = chunks
            .into_iter()
            .map(|c| Value::Array(vec![Value::Document(c.min), Value::Document(c.max)]))
            .collect::<Vec<_>>();
        out.push("chunks", list);
    }
    Ok(out.with("ok", 1.0))
}

/// `flushRouterConfig`: nothing cached to flush; any argument.
pub fn flush_router_config<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    admin::admin_only(FLUSH_ROUTER_CONFIG, ctx.db)?;
    Ok(Document::new().with("flushed", true).with("ok", 1.0))
}

/// `checkMetadataConsistency`: the catalog is one, in Group 0, so nothing disagrees: an empty
/// cursor on the database (`1`) or the collection.
pub fn check_metadata_consistency<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let ns = match ctx.first() {
        Value::String(c) => format!("{}.{c}", ctx.db),
        Value::Int32(1) | Value::Int64(1) => format!("{}.$cmd.aggregate", ctx.db),
        Value::Double(d) if *d == 1.0 => format!("{}.$cmd.aggregate", ctx.db),
        _ => {
            let msg = format!(
                "Invalid command format: the '{CHECK_METADATA_CONSISTENCY}' field must specify a \
                 collection name or 1"
            );
            return Err(CommandError::new(BAD_VALUE, msg));
        }
    };
    Ok(super::query::cursor_reply(0, &ns, "firstBatch", Vec::new()).with("ok", 1.0))
}

/// `appendOplogNote`: each shard answers; a `refrouter` adds the shards' answers under `raw` and,
/// when they fail, their error with an empty `errmsg`.
pub fn append_oplog_note<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    admin::admin_only(APPEND_OPLOG_NOTE, ctx.db)?;
    let each = match ctx.cmd.get("data") {
        Some(Value::Document(_)) => None,
        None => Some(CommandError::new(NO_SUCH_KEY, "Missing expected field \"data\"")),
        Some(v) => {
            let msg = format!(
                "\"data\" had the wrong type. Expected object, found {}",
                crate::types::type_name(v)
            );
            Some(CommandError::new(TYPE_MISMATCH, msg))
        }
    };
    let mut raw = Document::new();
    for (_, host) in shards(ctx.backend)? {
        let answer = match &each {
            None => Document::new().with("ok", 1.0),
            Some(e) => e.to_document(),
        };
        raw.push(&host, answer);
    }
    Ok(match each {
        None => Document::new().with("raw", raw).with("ok", 1.0),
        Some(e) => Document::new()
            .with("raw", raw)
            .with("code", e.code.code)
            .with("codeName", e.code.name)
            .with("ok", 0.0)
            .with("errmsg", ""),
    })
}

/// `transitionToDedicatedConfigServer`: Group 0 is the config server and holds no data, as a
/// dedicated one, so the transition is over.
pub fn transition_to_dedicated_config_server<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    admin::admin_only(TRANSITION_TO_DEDICATED_CONFIG_SERVER, ctx.db)?;
    Ok(Document::new()
        .with("state", "completed")
        .with("msg", "removeshard completed successfully")
        .with("shard", CONFIG)
        .with("ok", 1.0))
}
