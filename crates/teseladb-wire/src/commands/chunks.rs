//! The chunks of a collection with a shard key and the commands on them (018 FR-004, FR-006):
//! `split`, `splitVector`, `moveChunk`, `moveRange`, `mergeChunks` and `clearJumboFlag`. A chunk
//! is the part of a range that holds the collection's documents, `[min, max)` in shard key values
//! (the shard key part of the range's bounds, [`collection_chunks`]), and its shard is the node
//! that leads the range (contract `config-db.md`). A split cuts the range at the shard key value
//! (`enc(shard key) · enc(MinKey)`, before every document with that value), a move sends the
//! range's leadership to the node, adding a replica there first if it has none, and a merge joins
//! neighbouring ranges, as Group 0 orders.
//!
//! The replies, messages and codes are those of a `refrouter` 9.0.4 of two shards (measured on
//! 02-11-2026, `k1.js` and `k2.js` in the lane's notes); several of `split`'s errors carry no
//! code ([`UNCODED`]). `split` with `find` or `bounds` cuts at the shard key of the chunk's middle
//! document (its documents in key order, the one at half their number), as the shard does.

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_keys::{Direction, KeyBuilder, decode_untyped, values_len};
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_meta::command::Group0Command;
use teseladb_router::table::RouteEntry;
use teseladb_types::{GroupId, NodeId};

use super::names::parse_full;
use super::sharding::{ShardKey, document_key, shard_key_of};
use super::{Backend, Ctx, admin, display, shard_name};
use crate::error::{
    BAD_VALUE, CommandError, ILLEGAL_OPERATION, INVALID_NAMESPACE, INVALID_OPTIONS,
    NAMESPACE_NOT_FOUND, NAMESPACE_NOT_SHARDED, SHARD_NOT_FOUND, STALE_CONFIG, UNCODED,
};
use crate::types::wrong_type;
use teseladb_range::read::ReadConcern;

/// `split`.
pub const SPLIT: &str = "split";
/// `splitVector`.
pub const SPLIT_VECTOR: &str = "splitVector";
/// `moveChunk`.
pub const MOVE_CHUNK: &str = "moveChunk";
/// `moveRange`.
pub const MOVE_RANGE: &str = "moveRange";
/// `mergeChunks`.
pub const MERGE_CHUNKS: &str = "mergeChunks";
/// `clearJumboFlag`.
pub const CLEAR_JUMBO_FLAG: &str = "clearJumboFlag";
/// `moveChunk`'s code when no chunk has the bounds it names.
const NO_CHUNK_AT_BOUNDS: crate::error::Code =
    crate::error::Code { code: 656_452, name: "Location656452" };
/// `moveChunk`'s code when `find` has no shard key.
const NO_SHARD_KEY_IN_QUERY: crate::error::Code =
    crate::error::Code { code: 656_450, name: "Location656450" };
/// `clearJumboFlag`'s code when no chunk has the bounds it names.
const NO_JUMBO_CHUNK_AT_BOUNDS: crate::error::Code =
    crate::error::Code { code: 51_261, name: "Location51261" };
/// How many times a merge waits for the previous one of the same command.
const MERGE_WAITS: usize = 200;
/// Documents read at a time while looking for a chunk's middle.
const SCAN_BATCH: usize = 4_096;

/// A chunk of a collection: the part of range `group` in `[start, end)` that holds its documents.
#[derive(Clone, Debug, PartialEq)]
pub struct Chunk {
    /// The range.
    pub group: GroupId,
    /// First key of the chunk.
    pub start: Vec<u8>,
    /// First key past it.
    pub end: Vec<u8>,
    /// `min` in shard key values.
    pub min: Document,
    /// `max` in shard key values.
    pub max: Document,
    /// The node that leads the range.
    pub node: NodeId,
}

/// Where the documents of collection `(tenant, collection)` are: `[start, end)`.
fn documents_area(tenant: u32, collection: u32) -> (Vec<u8>, Vec<u8>) {
    let start = KeyBuilder::index_prefix(tenant, collection, teseladb_keys::DOCUMENTS);
    let end = KeyBuilder::index_prefix(tenant, collection, teseladb_keys::DOCUMENTS + 1);
    (start.as_bytes().to_vec(), end.as_bytes().to_vec())
}

/// Whether field `v` of a shard key is hashed.
fn hashed(v: &Value) -> bool {
    matches!(v, Value::String(s) if s == "hashed")
}

/// The bound of key `at` of the documents' area `[lo, hi)` in shard key values: `MinKey` at `lo`,
/// `MaxKey` at `hi`, and otherwise the shard key part of the key (a hashed field as a `long`).
fn bound(key: &ShardKey, at: &[u8], (lo, hi): (&[u8], &[u8])) -> Document {
    let edge = match at {
        a if a <= lo => Some(Value::MinKey),
        a if a >= hi => Some(Value::MaxKey),
        _ => None,
    };
    let dirs = vec![Direction::Ascending; key.key.len()];
    let rest = &at[lo.len().min(at.len())..];
    let values = match edge {
        Some(_) => None,
        None => values_len(rest, &dirs).ok().and_then(|n| decode_untyped(&rest[..n], &dirs).ok()),
    };
    let mut out = Document::new();
    for (i, (field, how)) in key.key.iter().enumerate() {
        let v = match (&edge, &values) {
            (Some(e), _) => e.clone(),
            (None, Some(vs)) => match (&vs[i], hashed(how)) {
                (Value::Int32(n), true) => Value::Int64(i64::from(*n)),
                (v, _) => v.clone(),
            },
            (None, None) => Value::Binary { subtype: 0, bytes: rest.to_vec() },
        };
        out.push(field, v);
    }
    out
}

/// The chunks of collection `coll`, whose shard key is `key`, in the ranges of `entries`, in key
/// order.
pub fn collection_chunks(
    entries: &[RouteEntry],
    coll: &CollectionEntry,
    key: &ShardKey,
) -> Vec<Chunk> {
    let (lo, hi) = documents_area(coll.tenant, coll.id);
    let mut out: Vec<Chunk> = entries
        .iter()
        .filter(|e| e.start < hi && (e.end.is_empty() || e.end > lo))
        .map(|e| {
            let start = e.start.clone().max(lo.clone());
            let end = if e.end.is_empty() { hi.clone() } else { e.end.clone().min(hi.clone()) };
            let node = e.leader_hint.or_else(|| e.voters.first().copied()).unwrap_or(NodeId(1));
            Chunk {
                group: e.group,
                min: bound(key, &start, (&lo, &hi)),
                max: bound(key, &end, (&lo, &hi)),
                start,
                end,
                node,
            }
        })
        .collect();
    out.sort_by(|a, b| a.start.cmp(&b.start));
    out
}

/// A collection with a shard key, as a chunk command finds it.
struct Sharded {
    coll: CollectionEntry,
    key: ShardKey,
}

/// The collection a chunk command names, refused as `split` does when it has no shard key.
fn sharded<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>, name: &str) -> Result<Sharded, CommandError> {
    let text = match ctx.first() {
        Value::String(s) => s.clone(),
        v => return Err(wrong_type(&format!("{name}.{name}"), v, "string")),
    };
    let ns =
        parse_full(&text).filter(|n| !n.db.is_empty() && !n.coll.is_empty()).ok_or_else(|| {
            CommandError::new(INVALID_NAMESPACE, format!("Invalid namespace specified '{text}'"))
        })?;
    let unsharded = || {
        let msg = format!("Command not supported on unsharded collection {text}");
        CommandError::new(NAMESPACE_NOT_SHARDED, msg)
    };
    let coll = ctx.backend.collection(&ns)?.ok_or_else(unsharded)?;
    let key = shard_key_of(&coll)?.ok_or_else(unsharded)?;
    Ok(Sharded { coll, key })
}

/// `{ a: 1 }` as the shell writes it.
fn shown(d: &Document) -> String {
    display(&Value::Document(d.clone()))
}

/// Whether `d` has exactly the fields of the shard key, in its order.
fn is_key_value(key: &ShardKey, d: &Document) -> bool {
    d.len() == key.key.len() && d.iter().zip(key.key.iter()).all(|((a, _), (b, _))| a == b)
}

/// The key where a split at shard key value `value` cuts: before every document with it.
fn cut_at(s: &Sharded, value: &Document) -> Result<Vec<u8>, CommandError> {
    let mut doc = value.clone();
    doc.push("_id", Value::MinKey);
    document_key(&s.coll, &s.key.key, &doc)
}

/// The collection's chunks now.
fn chunks_of<B: Backend + ?Sized>(
    backend: &mut B,
    s: &Sharded,
) -> Result<Vec<Chunk>, CommandError> {
    Ok(collection_chunks(&backend.route_entries()?, &s.coll, &s.key))
}

/// The chunk that holds key `at`.
fn holding(chunks: &[Chunk], at: &[u8]) -> Option<Chunk> {
    chunks.iter().find(|c| c.start.as_slice() <= at && at < c.end.as_slice()).cloned()
}

/// The chunk with bounds exactly `[min, max)`.
fn with_bounds(chunks: &[Chunk], min: &Document, max: &Document) -> Option<Chunk> {
    chunks.iter().find(|c| &c.min == min && &c.max == max).cloned()
}

/// The two bounds of a `bounds` array, if it is one.
fn two_bounds(v: &Value) -> Option<(Document, Document)> {
    match v {
        Value::Array(a) => match a.as_slice() {
            [Value::Document(min), Value::Document(max)] => Some((min.clone(), max.clone())),
            _ => None,
        },
        _ => None,
    }
}

/// The key of the document a chunk command's `find` names: its shard key value.
fn find_key(s: &Sharded, find: &Document) -> Result<Option<Vec<u8>>, CommandError> {
    if !s.key.key.iter().all(|(f, _)| find.get(f).is_some()) {
        return Ok(None);
    }
    let mut value = Document::new();
    for (f, _) in s.key.key.iter() {
        value.push(f, find.get(f).cloned().unwrap_or(Value::Null));
    }
    let mut doc = find.clone();
    doc.remove("_id");
    for (k, v) in value.iter() {
        doc.insert(k, v.clone());
    }
    cut_at(s, &doc).map(Some)
}

/// Asks Group 0 to split `chunk` at `at`, unless `at` is one of its bounds already.
fn split_chunk<B: Backend + ?Sized>(
    backend: &mut B,
    chunk: &Chunk,
    at: Vec<u8>,
) -> Result<(), CommandError> {
    if at <= chunk.start || at >= chunk.end {
        return Ok(());
    }
    backend.range_order(Group0Command::SplitRange { group: chunk.group, at })
}

/// The cut at the middle document of `chunk`, if it has documents with a shard key value after
/// its start.
fn middle_cut<B: Backend + ?Sized>(
    backend: &mut B,
    s: &Sharded,
    chunk: &Chunk,
) -> Result<Option<Vec<u8>>, CommandError> {
    let mut docs = Vec::new();
    let mut next = chunk.start.clone();
    loop {
        let rows = backend.scan(&next, &chunk.end, SCAN_BATCH, ReadConcern::Local)?;
        let short = rows.len() < SCAN_BATCH;
        let Some(last) = rows.last().map(|(k, _)| k.clone()) else { break };
        docs.extend(rows.into_iter().map(|(_, d)| d));
        if short {
            break;
        }
        next = last;
        next.push(0);
    }
    let Some(doc) = docs.get(docs.len() / 2) else { return Ok(None) };
    let doc = Document::from(RawDoc::from_bytes(doc)?);
    let mut at = doc.clone();
    at.remove("_id");
    at.push("_id", Value::MinKey);
    let at = document_key(&s.coll, &s.key.key, &at)?;
    Ok((at > chunk.start).then_some(at))
}

fn ok() -> Document {
    Document::new().with("ok", 1.0)
}

/// `split`: with `middle`, at that shard key value; with `find` or `bounds`, at the middle of the
/// chunk.
pub fn split<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin::admin_only(SPLIT, ctx.db)?;
    let s = sharded(ctx, SPLIT)?;
    let (middle, find, bounds) = (
        ctx.cmd.get("middle").cloned(),
        ctx.cmd.get("find").cloned(),
        ctx.cmd.get("bounds").cloned(),
    );
    if middle.is_some() && find.is_some() {
        return Err(CommandError::new(UNCODED, "cannot specify find and middle together"));
    }
    let chunks = chunks_of(ctx.backend, &s)?;
    match (middle, find, bounds) {
        (Some(Value::Document(m)), _, _) => {
            if !is_key_value(&s.key, &m) {
                let msg = format!(
                    "new split key {} is not valid for shard key pattern {}",
                    shown(&m),
                    shown(&s.key.key)
                );
                return Err(CommandError::new(UNCODED, msg));
            }
            // `MinKey` everywhere is the first chunk's own start.
            let lowest = m.iter().all(|(_, v)| *v == Value::MinKey);
            let at = cut_at(&s, &m)?;
            if let Some(chunk) = holding(&chunks, &at).filter(|_| !lowest) {
                split_chunk(ctx.backend, &chunk, at)?;
            }
        }
        (Some(v), _, _) => {
            let msg = format!(
                "wrong type for 'middle' field, expected object, found middle: {}",
                display(&v)
            );
            return Err(CommandError::new(UNCODED, msg));
        }
        (None, Some(Value::Document(f)), _) => {
            let chunk = find_key(&s, &f)?.and_then(|at| holding(&chunks, &at));
            if let Some(chunk) = chunk
                && let Some(at) = middle_cut(ctx.backend, &s, &chunk)?
            {
                split_chunk(ctx.backend, &chunk, at)?;
            }
        }
        (None, None, Some(b)) => {
            let (min, max) = two_bounds(&b).unwrap_or_default();
            let Some(chunk) = with_bounds(&chunks, &min, &max) else {
                let msg = format!(
                    "no chunk found with the shard key bounds [{}, {})",
                    shown(&min),
                    shown(&max)
                );
                return Err(CommandError::new(UNCODED, msg));
            };
            if let Some(at) = middle_cut(ctx.backend, &s, &chunk)? {
                split_chunk(ctx.backend, &chunk, at)?;
            }
        }
        _ => return Err(CommandError::new(UNCODED, "need to specify find/bounds or middle")),
    }
    Ok(ok())
}

/// `splitVector`: a `refrouter` refuses it on `admin` and asks for a range owned by one shard.
pub fn split_vector<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    if ctx.db == admin::ADMIN_DB {
        let msg = "Performing splitVector across dbs isn't supported via refrouter";
        return Err(CommandError::new(ILLEGAL_OPERATION, msg));
    }
    let ns = match ctx.first() {
        Value::String(s) => s.clone(),
        v => return Err(wrong_type(&format!("{SPLIT_VECTOR}.{SPLIT_VECTOR}"), v, "string")),
    };
    let msg = format!(
        "The range {{min: {{}} , max{{}} }} for the namespace {ns} is required to be owned by one shard"
    );
    Err(CommandError::new(INVALID_OPTIONS, msg))
}

/// The node of shard `name`.
fn node_named<B: Backend + ?Sized>(
    backend: &mut B,
    name: &str,
) -> Result<Option<NodeId>, CommandError> {
    let listed = backend
        .config_documents(super::config_db::SHARDS)?
        .iter()
        .any(|d| matches!(d.get("_id"), Some(Value::String(s)) if s == name));
    let number = name.strip_prefix(super::SHARD_PREFIX).and_then(|n| n.parse::<u64>().ok());
    Ok(number.filter(|_| listed).map(NodeId))
}

/// Sends the leadership of `chunk`'s range to `node`, which gets a replica first if it has none.
fn move_chunk_to<B: Backend + ?Sized>(
    backend: &mut B,
    chunk: &Chunk,
    node: NodeId,
) -> Result<(), CommandError> {
    let entry = backend.route_entries()?.into_iter().find(|e| e.group == chunk.group);
    let Some(entry) = entry else { return Ok(()) };
    if !entry.voters.contains(&node) {
        let leaving: Vec<NodeId> =
            entry.voters.iter().copied().filter(|n| *n == chunk.node).collect();
        backend.range_order(Group0Command::SetReplicas {
            group: chunk.group,
            epoch: entry.generation,
            add: vec![node],
            remove: leaving,
        })?;
    }
    backend.range_order(Group0Command::SetLeaderHint { group: chunk.group, node, term: 0 })
}

/// A required string field of an IDL command.
fn required_string(cmd: &Document, name: &str, field: &str) -> Result<String, CommandError> {
    match super::args::string(cmd, name, field)? {
        Some(s) => Ok(s.to_owned()),
        None => Err(super::args::missing(name, field)),
    }
}

/// `moveChunk`: the chunk of `find` or `bounds` goes to shard `to`.
pub fn move_chunk<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin::admin_only(MOVE_CHUNK, ctx.db)?;
    let to = required_string(ctx.cmd, MOVE_CHUNK, "to")?;
    let s = sharded(ctx, MOVE_CHUNK)?;
    let node = node_named(ctx.backend, &to)?.ok_or_else(|| {
        let msg = format!("Could not find destination shard :: caused by :: Shard {to} not found");
        CommandError::new(SHARD_NOT_FOUND, msg)
    })?;
    let chunks = chunks_of(ctx.backend, &s)?;
    let chunk = match (ctx.cmd.get("find"), ctx.cmd.get("bounds")) {
        (Some(Value::Document(f)), _) => {
            let at = find_key(&s, f)?.ok_or_else(|| {
                let msg = format!("no shard key found in chunk query {}", shown(f));
                CommandError::new(NO_SHARD_KEY_IN_QUERY, msg)
            })?;
            holding(&chunks, &at)
        }
        (_, Some(b)) => {
            let (min, max) = two_bounds(b).unwrap_or_default();
            let found = with_bounds(&chunks, &min, &max);
            let msg = format!(
                "no chunk found with the shard key bounds [{},{})",
                shown(&min),
                shown(&max)
            );
            Some(found.ok_or_else(|| CommandError::new(NO_CHUNK_AT_BOUNDS, msg))?)
        }
        _ => return Err(CommandError::new(INVALID_OPTIONS, "need to specify query or bounds")),
    };
    let start = ctx.backend.now_ms();
    if let Some(chunk) = chunk.filter(|c| c.node != node) {
        move_chunk_to(ctx.backend, &chunk, node)?;
    }
    let millis = ctx.backend.now_ms().saturating_sub(start);
    Ok(Document::new().with("millis", i64::try_from(millis).unwrap_or(i64::MAX)).with("ok", 1.0))
}

/// A bound of `moveRange`, checked against the shard key.
fn range_bound(s: &Sharded, cmd: &Document, field: &str) -> Result<Option<Document>, CommandError> {
    match cmd.get(field) {
        None => Ok(None),
        Some(Value::Document(d)) if is_key_value(&s.key, d) => Ok(Some(d.clone())),
        Some(Value::Document(d)) => {
            let msg = format!(
                "The '{field}' bound {} is not valid for shard key pattern {}",
                shown(d),
                shown(&s.key.key)
            );
            Err(CommandError::new(INVALID_OPTIONS, msg))
        }
        Some(v) => Err(wrong_type(&format!("{MOVE_RANGE}.{field}"), v, "object")),
    }
}

/// Waits until the chunk at `at` starts there (a split asked for it went through).
fn wait_for_cut<B: Backend + ?Sized>(
    backend: &mut B,
    s: &Sharded,
    at: &[u8],
) -> Result<(), CommandError> {
    for _ in 0..MERGE_WAITS {
        if chunks_of(backend, s)?.iter().any(|c| c.start == at) || !backend.pause() {
            return Ok(());
        }
    }
    Ok(())
}

/// `moveRange`: the range from `min` (or to `max`) inside one chunk goes to shard `toShard`,
/// cut out of its chunk first.
pub fn move_range<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin::admin_only(MOVE_RANGE, ctx.db)?;
    let to = required_string(ctx.cmd, MOVE_RANGE, "toShard")?;
    let s = match sharded(ctx, MOVE_RANGE) {
        Ok(s) => s,
        Err(e) if e.code == NAMESPACE_NOT_SHARDED => {
            let name = super::args::string(ctx.cmd, MOVE_RANGE, MOVE_RANGE)?.unwrap_or_default();
            let msg = format!("Collection '{name}' not found in cluster catalog");
            return Err(CommandError::new(NAMESPACE_NOT_FOUND, msg));
        }
        Err(e) => return Err(e),
    };
    let min = range_bound(&s, ctx.cmd, "min")?;
    let max = range_bound(&s, ctx.cmd, "max")?;
    if min.is_none() && max.is_none() {
        return Err(CommandError::new(
            INVALID_OPTIONS,
            "Missing required parameter 'min' or 'max'",
        ));
    }
    let node = node_named(ctx.backend, &to)?.ok_or_else(|| {
        let msg = format!("Could not find destination shard :: caused by :: Shard {to} not found");
        CommandError::new(SHARD_NOT_FOUND, msg)
    })?;
    let low = min.as_ref().map(|m| cut_at(&s, m)).transpose()?;
    let high = max.as_ref().map(|m| cut_at(&s, m)).transpose()?;
    for at in [&low, &high].into_iter().flatten() {
        if let Some(chunk) = holding(&chunks_of(ctx.backend, &s)?, at) {
            split_chunk(ctx.backend, &chunk, at.clone())?;
            wait_for_cut(ctx.backend, &s, at)?;
        }
    }
    let chunks = chunks_of(ctx.backend, &s)?;
    let moved = match (&low, &high) {
        (Some(lo), _) => holding(&chunks, lo),
        (None, Some(hi)) => chunks.iter().find(|c| &c.end == hi).cloned(),
        (None, None) => None,
    };
    if let Some(chunk) = moved.filter(|c| c.node != node) {
        move_chunk_to(ctx.backend, &chunk, node)?;
    }
    Ok(ok())
}

/// `mergeChunks`: the chunks between `bounds`, all on one shard, become one.
pub fn merge_chunks<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin::admin_only(MERGE_CHUNKS, ctx.db)?;
    let bounds = match ctx.cmd.get("bounds") {
        Some(Value::Array(a)) => a.clone(),
        Some(v) => return Err(wrong_type(&format!("{MERGE_CHUNKS}.bounds"), v, "array")),
        None => return Err(super::args::missing(MERGE_CHUNKS, "bounds")),
    };
    let s = sharded(ctx, MERGE_CHUNKS)?;
    let (min, max) = two_bounds(&Value::Array(bounds))
        .ok_or_else(|| CommandError::new(BAD_VALUE, "only a min and max bound may be specified"))?;
    let chunks = chunks_of(ctx.backend, &s)?;
    let first = chunks.iter().position(|c| c.min == min);
    let last = chunks.iter().position(|c| c.max == max);
    let run = match (first, last) {
        (Some(a), Some(b)) if a <= b => &chunks[a..=b],
        _ => &[],
    };
    if run.is_empty() || run.iter().any(|c| c.node != run[0].node) {
        let msg = format!(
            "Exceeded maximum number of 10 retries attempting '{MERGE_CHUNKS}' :: caused by :: \
             Range with bounds [{}, {}) is not owned by this shard.",
            shown(&min),
            shown(&max)
        );
        return Err(CommandError::new(STALE_CONFIG, msg));
    }
    let left = run[0].group;
    for right in run[1..].iter().map(|c| c.group) {
        ctx.backend.range_order(Group0Command::MergeRanges { left, right })?;
        for _ in 0..MERGE_WAITS {
            let gone = !ctx.backend.route_entries()?.iter().any(|e| e.group == right);
            if gone || !ctx.backend.pause() {
                break;
            }
        }
    }
    Ok(ok())
}

/// `clearJumboFlag`: TeselaDB marks no chunk jumbo (a range splits at any key), so it only finds
/// the chunk.
pub fn clear_jumbo_flag<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    admin::admin_only(CLEAR_JUMBO_FLAG, ctx.db)?;
    let s = sharded(ctx, CLEAR_JUMBO_FLAG)?;
    match (ctx.cmd.get("find"), ctx.cmd.get("bounds")) {
        (Some(Value::Document(_)), _) => Ok(ok()),
        (_, Some(b)) => {
            let (min, max) = two_bounds(b).unwrap_or_default();
            match with_bounds(&chunks_of(ctx.backend, &s)?, &min, &max) {
                Some(_) => Ok(ok()),
                None => {
                    let msg = format!(
                        "no chunk found with the shard key bounds [{}, {})",
                        shown(&min),
                        shown(&max)
                    );
                    Err(CommandError::new(NO_JUMBO_CHUNK_AT_BOUNDS, msg))
                }
            }
        }
        _ => Err(CommandError::new(INVALID_OPTIONS, "need to specify find or bounds")),
    }
}

/// The documents of `config.chunks` of every collection with a shard key in `collections`, from
/// the ranges of `entries` (018 FR-006): `_id`, `uuid`, `min`, `max`, `shard`, `lastmod`,
/// `onCurrentShardSince`, `history` and `jumbo`, as a `refrouter` 9.0.4 lists them.
pub fn config_chunks<'a>(
    entries: &[RouteEntry],
    collections: impl Iterator<Item = (&'a Namespace, &'a CollectionEntry)>,
) -> Vec<Document> {
    let mut out = Vec::new();
    for (_, coll) in collections {
        let Ok(Some(key)) = shard_key_of(coll) else { continue };
        let since = Value::Timestamp {
            time: u32::try_from(coll.created_index).unwrap_or(u32::MAX),
            increment: 1,
        };
        for (i, c) in collection_chunks(entries, coll, &key).into_iter().enumerate() {
            let mut id = [0u8; 12];
            id[..4].copy_from_slice(&coll.id.to_be_bytes());
            id[4..].copy_from_slice(&c.group.0.to_be_bytes());
            let shard = shard_name(c.node.0);
            let history =
                Document::new().with("validAfter", since.clone()).with("shard", shard.as_str());
            out.push(
                Document::new()
                    .with("_id", Value::ObjectId(id))
                    .with("uuid", Value::Binary { subtype: 4, bytes: coll.uuid.to_vec() })
                    .with("min", c.min)
                    .with("max", c.max)
                    .with("shard", shard.as_str())
                    .with(
                        "lastmod",
                        Value::Timestamp {
                            time: 1,
                            increment: u32::try_from(i).unwrap_or(u32::MAX),
                        },
                    )
                    .with("onCurrentShardSince", since.clone())
                    .with("history", vec![Value::Document(history)])
                    .with("jumbo", false),
            );
        }
    }
    out
}
