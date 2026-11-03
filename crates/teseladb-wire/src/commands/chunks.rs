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
use super::sharding::{ShardKey, document_key, is_hashed, shard_key_of, shard_values};
use super::{Backend, Ctx, admin, display, shard_name};
use crate::error::{
    BAD_VALUE, CONFLICTING_OPERATION_IN_PROGRESS, CommandError, ILLEGAL_OPERATION,
    INVALID_NAMESPACE, INVALID_OPTIONS, NAMESPACE_NOT_FOUND, NAMESPACE_NOT_SHARDED,
    OPERATION_FAILED, SHARD_NOT_FOUND, STALE_CONFIG, UNCODED,
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
/// `mergeAllChunksOnShard`.
pub const MERGE_ALL_CHUNKS_ON_SHARD: &str = "mergeAllChunksOnShard";
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
/// `split`'s refusal of a chunk whose documents all have its start's shard key value.
const CANNOT_SPLIT: crate::error::Code = crate::error::Code { code: 87, name: "CannotSplit" };
/// `split`'s refusal of a `bounds` element that is not a document.
const BOUND_NOT_OBJECT: crate::error::Code =
    crate::error::Code { code: 10_065, name: "Location10065" };
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
    /// The range's voters.
    pub voters: Vec<NodeId>,
    /// The range's generation, which a change of its replicas names.
    pub generation: u64,
}

/// Where the documents of collection `(tenant, collection)` are: `[start, end)`.
fn documents_area(tenant: u32, collection: u32) -> (Vec<u8>, Vec<u8>) {
    let start = KeyBuilder::index_prefix(tenant, collection, teseladb_keys::DOCUMENTS);
    let end = KeyBuilder::index_prefix(tenant, collection, teseladb_keys::DOCUMENTS + 1);
    (start.as_bytes().to_vec(), end.as_bytes().to_vec())
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
            (None, Some(vs)) => match (&vs[i], is_hashed(how)) {
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
                voters: e.voters.clone(),
                generation: e.generation,
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

/// The key where a split at shard key value `value` cuts: before every document with it. A bound
/// names its fields whole (`{"a.b": 1}`) and a hashed field by its hash, as a `refrouter` takes them.
/// `MinKey` everywhere is where the collection's first chunk starts.
fn cut_at(s: &Sharded, value: &Document) -> Result<Vec<u8>, CommandError> {
    if value.iter().all(|(_, v)| *v == Value::MinKey) {
        return Ok(documents_area(s.coll.tenant, s.coll.id).0);
    }
    super::zones::bound_key(&s.coll, value)
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

/// The chunk that holds key `at` of the collection's documents: the ranges cover every key, so
/// only ranges read while they change can miss it.
fn chunk_at(chunks: &[Chunk], at: &[u8]) -> Result<Chunk, CommandError> {
    holding(chunks, at)
        .ok_or_else(|| CommandError::new(STALE_CONFIG, "no range holds the chunk's key"))
}

/// The chunk with bounds exactly `[min, max)`.
fn with_bounds(chunks: &[Chunk], min: &Document, max: &Document) -> Option<Chunk> {
    chunks.iter().find(|c| &c.min == min && &c.max == max).cloned()
}

/// The documents of the `bounds` array of command `name` (`moveChunk`, `mergeChunks`,
/// `clearJumboFlag`), checked as its parser checks them.
fn bounds_of(name: &str, v: &Value) -> Result<Vec<Document>, CommandError> {
    let Value::Array(a) = v else {
        return Err(wrong_type(&format!("{name}.bounds"), v, "array"));
    };
    let doc = |(i, b): (usize, &Value)| match b {
        Value::Document(d) => Ok(d.clone()),
        other => Err(wrong_type(&format!("{name}.bounds.{i}"), other, "object")),
    };
    a.iter().enumerate().map(doc).collect()
}

/// The two bounds of `moveChunk` and `clearJumboFlag`.
fn two_bounds(name: &str, v: &Value) -> Result<(Document, Document), CommandError> {
    match <[Document; 2]>::try_from(bounds_of(name, v)?) {
        Ok([min, max]) => Ok((min, max)),
        Err(_) => {
            Err(CommandError::new(INVALID_OPTIONS, "bounds can only have exactly 2 elements"))
        }
    }
}

/// The two bounds of `split`, checked as a `refrouter` 9.0.4 checks them.
fn split_bounds(s: &Sharded, v: &Value) -> Result<(Document, Document), CommandError> {
    let Value::Array(a) = v else {
        let msg =
            format!("wrong type for 'bounds' field, expected array, found bounds: {}", display(v));
        return Err(CommandError::new(UNCODED, msg));
    };
    match a.len() {
        0 => return Err(CommandError::new(UNCODED, "need to specify find/bounds or middle")),
        1 => return Err(CommandError::new(UNCODED, "upper bound not specified")),
        _ => {}
    }
    let doc = |i: usize| match &a[i] {
        Value::Document(d) => Ok(d.clone()),
        _ => {
            let msg = format!("invalid parameter: expected an object ({i})");
            Err(CommandError::new(BOUND_NOT_OBJECT, msg))
        }
    };
    let (min, max) = (doc(0)?, doc(1)?);
    if !is_key_value(&s.key, &min) || !is_key_value(&s.key, &max) {
        return Err(CommandError::new(UNCODED, invalid_bounds(s, &min, &max)));
    }
    Ok((min, max))
}

/// The message of bounds that are not values of the shard key.
fn invalid_bounds(s: &Sharded, min: &Document, max: &Document) -> String {
    format!(
        "shard key bounds [{},{}) are not valid for shard key pattern {}",
        shown(min),
        shown(max),
        shown(&s.key.key)
    )
}

/// The key of the document a chunk command's `find` names: its shard key value, a hashed field
/// hashed.
fn find_key(s: &Sharded, find: &Document) -> Result<Option<Vec<u8>>, CommandError> {
    let mut value = Document::new();
    for (field, kind) in s.key.key.iter() {
        let Some(v) = find.get(field) else { return Ok(None) };
        let one = |v: Value| Document::new().with("v", v);
        let mut held = shard_values(&one(kind.clone()), &one(v.clone()))?;
        value.push(field, held.pop().unwrap_or(Value::Null));
    }
    cut_at(s, &value).map(Some)
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

/// The cut at the shard key value of the middle document of `chunk`, if it has documents.
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
    document_key(&s.coll, &s.key.key, &at).map(Some)
}

/// Splits `chunk` at its middle document, as `split` with `find` or `bounds` does; a `refrouter`
/// 9.0.4 refuses an empty collection, a chunk with no documents and one whose documents all have
/// its start's shard key value.
fn split_in_middle<B: Backend + ?Sized>(
    backend: &mut B,
    s: &Sharded,
    chunk: &Chunk,
) -> Result<(), CommandError> {
    let (lo, hi) = documents_area(s.coll.tenant, s.coll.id);
    if backend.scan(&lo, &hi, 1, ReadConcern::Local)?.is_empty() {
        let msg = "cannot use split with find or bounds option on an empty collection";
        return Err(CommandError::new(INVALID_OPTIONS, msg));
    }
    let Some(at) = middle_cut(backend, s, chunk)? else {
        let msg = "can't open a cursor to scan the range (desired range is possibly empty)";
        return Err(CommandError::new(OPERATION_FAILED, msg));
    };
    if at <= chunk.start {
        let msg = "Unable to find median in chunk because chunk is indivisible.";
        return Err(CommandError::new(CANNOT_SPLIT, msg));
    }
    split_chunk(backend, chunk, at)
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
            let at = cut_at(&s, &m)?;
            split_chunk(ctx.backend, &chunk_at(&chunks, &at)?, at)?;
        }
        (Some(v), _, _) => {
            let msg = format!(
                "wrong type for 'middle' field, expected object, found middle: {}",
                display(&v)
            );
            return Err(CommandError::new(UNCODED, msg));
        }
        (None, Some(Value::Document(f)), _) => {
            let at = find_key(&s, &f)?.ok_or_else(|| {
                let msg = format!("no shard key found in chunk query {}", shown(&f));
                CommandError::new(UNCODED, msg)
            })?;
            split_in_middle(ctx.backend, &s, &chunk_at(&chunks, &at)?)?;
        }
        (None, Some(v), _) => {
            let msg = format!(
                "wrong type for 'find' field, expected object, found find: {}",
                display(&v)
            );
            return Err(CommandError::new(UNCODED, msg));
        }
        (None, None, Some(b)) => {
            let (min, max) = split_bounds(&s, &b)?;
            let Some(chunk) = with_bounds(&chunks, &min, &max) else {
                let msg = format!(
                    "no chunk found with the shard key bounds [{}, {})",
                    shown(&min),
                    shown(&max)
                );
                return Err(CommandError::new(UNCODED, msg));
            };
            split_in_middle(ctx.backend, &s, &chunk)?;
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
    if !chunk.voters.contains(&node) {
        let leaving: Vec<NodeId> =
            chunk.voters.iter().copied().filter(|n| *n == chunk.node).collect();
        backend.range_order(Group0Command::SetReplicas {
            group: chunk.group,
            epoch: chunk.generation,
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
            let (min, max) = two_bounds(MOVE_CHUNK, b)?;
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
    check_migrations(&s)?;
    let start = ctx.backend.now_ms();
    if let Some(chunk) = chunk.filter(|c| c.node != node) {
        move_chunk_to(ctx.backend, &chunk, node)?;
    }
    let millis = ctx.backend.now_ms().saturating_sub(start);
    Ok(Document::new().with("millis", i64::try_from(millis).unwrap_or(i64::MAX)).with("ok", 1.0))
}

/// A bound of `moveRange` as its parser reads it: a document, if given.
fn range_bound(cmd: &Document, field: &str) -> Result<Option<Document>, CommandError> {
    match cmd.get(field) {
        None => Ok(None),
        Some(Value::Document(d)) => Ok(Some(d.clone())),
        Some(v) => Err(wrong_type(&format!("{MOVE_RANGE}.{field}"), v, "object")),
    }
}

/// Bound `field` of `moveRange`, checked against the shard key.
fn check_range_bound(
    s: &Sharded,
    field: &str,
    bound: Option<&Document>,
) -> Result<(), CommandError> {
    match bound {
        Some(d) if !is_key_value(&s.key, d) => {
            let msg = format!(
                "The '{field}' bound {} is not valid for shard key pattern {}",
                shown(d),
                shown(&s.key.key)
            );
            Err(CommandError::new(INVALID_OPTIONS, msg))
        }
        _ => Ok(()),
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
    let name = super::args::string(ctx.cmd, MOVE_RANGE, MOVE_RANGE)?.unwrap_or_default().to_owned();
    let min = range_bound(ctx.cmd, "min")?;
    let max = range_bound(ctx.cmd, "max")?;
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
    // A `refrouter` 9.0.4 names a database alone (`db` or `db.`) by the database.
    let s = sharded(ctx, MOVE_RANGE).map_err(|e| match e.code {
        INVALID_NAMESPACE | NAMESPACE_NOT_SHARDED => {
            let shown = name.strip_suffix('.').unwrap_or(&name);
            let msg = format!("Collection '{shown}' not found in cluster catalog");
            CommandError::new(NAMESPACE_NOT_FOUND, msg)
        }
        _ => e,
    })?;
    check_range_bound(&s, "min", min.as_ref())?;
    check_range_bound(&s, "max", max.as_ref())?;
    check_migrations(&s)?;
    let low = min.as_ref().map(|m| cut_at(&s, m)).transpose()?;
    let high = max.as_ref().map(|m| cut_at(&s, m)).transpose()?;
    for at in [&low, &high].into_iter().flatten() {
        let chunk = chunk_at(&chunks_of(ctx.backend, &s)?, at)?;
        split_chunk(ctx.backend, &chunk, at.clone())?;
        wait_for_cut(ctx.backend, &s, at)?;
    }
    let chunks = chunks_of(ctx.backend, &s)?;
    let moved = match (&low, &high) {
        (Some(lo), _) => holding(&chunks, lo),
        (None, hi) => chunks.iter().find(|c| Some(&c.end) == hi.as_ref()).cloned(),
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
        Some(v) => bounds_of(MERGE_CHUNKS, v)?,
        None => return Err(super::args::missing(MERGE_CHUNKS, "bounds")),
    };
    let s = sharded(ctx, MERGE_CHUNKS)?;
    let Ok([min, max]) = <[Document; 2]>::try_from(bounds) else {
        return Err(CommandError::new(BAD_VALUE, "only a min and max bound may be specified"));
    };
    if super::zones::compare(&min, &max)?.is_ge() {
        let msg = format!("min: {} should be less than max: {}", shown(&min), shown(&max));
        return Err(CommandError::new(BAD_VALUE, msg));
    }
    if !is_key_value(&s.key, &min) || !is_key_value(&s.key, &max) {
        return Err(CommandError::new(INVALID_OPTIONS, invalid_bounds(&s, &min, &max)));
    }
    let chunks = chunks_of(ctx.backend, &s)?;
    let first = chunks.iter().position(|c| c.min == min);
    let last = chunks.iter().position(|c| c.max == max);
    // The bounds are in order, so the first chunk comes before the last one.
    let run = match (first, last) {
        (Some(a), Some(b)) => chunks.get(a..=b).unwrap_or_default(),
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
    merge_run(ctx.backend, &run[0], &run[1..])?;
    Ok(ok())
}

/// Joins the neighbouring chunks `rest` into the range of `first`, the chunk before them.
fn merge_run<B: Backend + ?Sized>(
    backend: &mut B,
    first: &Chunk,
    rest: &[Chunk],
) -> Result<(), CommandError> {
    let left = first.group;
    for right in rest.iter().map(|c| c.group) {
        backend.range_order(Group0Command::MergeRanges { left, right })?;
        for _ in 0..MERGE_WAITS {
            let gone = !backend.route_entries()?.iter().any(|e| e.group == right);
            if gone || !backend.pause() {
                break;
            }
        }
    }
    Ok(())
}

/// `mergeAllChunksOnShard`: every run of neighbouring chunks of the collection on the shard,
/// of at most `maxNumberOfChunksToMerge` chunks, becomes one. A `refrouter` 9.0.4 leaves out the
/// chunks split or moved in the last `minSnapshotHistoryWindowInSeconds`; the ranges here keep
/// their history across a merge, so every run is merged at once.
pub fn merge_all_chunks_on_shard<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let name = MERGE_ALL_CHUNKS_ON_SHARD;
    let text = match ctx.first() {
        Value::String(s) => s.clone(),
        v => return Err(wrong_type(&format!("{name}.{name}"), v, "string")),
    };
    let shard = required_string(ctx.cmd, name, "shard")?;
    let most = match ctx.cmd.get("maxNumberOfChunksToMerge") {
        Some(Value::Int32(n)) => usize::try_from(*n).unwrap_or(0).max(2),
        Some(v) => return Err(wrong_type(&format!("{name}.maxNumberOfChunksToMerge"), v, "int")),
        None => usize::MAX,
    };
    admin::admin_only(name, ctx.db)?;
    let ns =
        parse_full(&text).filter(|n| !n.db.is_empty() && !n.coll.is_empty()).ok_or_else(|| {
            CommandError::new(INVALID_NAMESPACE, "invalid namespace specified for request")
        })?;
    let node = node_named(ctx.backend, &shard)?
        .ok_or_else(|| CommandError::new(SHARD_NOT_FOUND, format!("Shard {shard} not found")))?;
    let unsharded =
        || CommandError::new(NAMESPACE_NOT_SHARDED, format!("Collection {text} is not sharded"));
    let coll = ctx.backend.collection(&ns)?.ok_or_else(unsharded)?;
    let key = shard_key_of(&coll)?.ok_or_else(unsharded)?;
    let s = Sharded { coll, key };
    let chunks = chunks_of(ctx.backend, &s)?;
    let mut runs: Vec<Vec<Chunk>> = Vec::new();
    for c in chunks.into_iter() {
        let joins = runs
            .last()
            .and_then(|r| r.last())
            .is_some_and(|p| p.node == node && c.node == node && p.end == c.start)
            && runs.last().is_some_and(|r| r.len() < most);
        match joins {
            true => runs.last_mut().into_iter().for_each(|r| r.push(c.clone())),
            false => runs.push(vec![c]),
        }
    }
    for run in runs.iter().filter(|r| r.len() > 1) {
        merge_run(ctx.backend, &run[0], &run[1..])?;
    }
    Ok(ok())
}

/// Refuses a migration of a collection whose migrations `setAllowMigrations` blocked.
fn check_migrations(s: &Sharded) -> Result<(), CommandError> {
    match super::balancing::migrations_allowed(&s.coll) {
        true => Ok(()),
        false => Err(CommandError::new(
            CONFLICTING_OPERATION_IN_PROGRESS,
            "Command request failed on source shard. :: caused by :: Can't execute moveChunk because migrations for this collection are disallowed",
        )),
    }
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
            let (min, max) = two_bounds(CLEAR_JUMBO_FLAG, b)?;
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
