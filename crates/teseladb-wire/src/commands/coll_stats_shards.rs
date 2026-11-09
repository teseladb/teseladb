//! The parts of `collStats` that a `refrouter` 9.0.4 adds to a shard's answer (010 FR-018, T055;
//! measured on the native `refrouter` on 09-11-2026): the database's primary shard of a collection
//! without a shard key, the number of chunks, and a document per shard that holds the collection.
//! A shard here is a node (D-102): each node's document measures the ranges it serves.

use std::collections::BTreeMap;

use teseladb_keys::KeyBuilder;
use teseladb_meta::catalog::CollectionEntry;
use teseladb_router::table::RouteEntry;
use teseladb_bson::{Document, Value};

use super::{Backend, shard_name};
use crate::error::CommandError;

/// The shard a `refrouter` names as the primary of database `db`: the `primary` of its document of
/// `config.databases`, or the first node when the node keeps no such document.
pub(super) fn primary_of<B: Backend + ?Sized>(
    backend: &mut B,
    db: &str,
) -> Result<String, CommandError> {
    let primary = backend.config_documents("databases")?.into_iter().find_map(|d| {
        match (d.get("_id"), d.get("primary")) {
            (Some(Value::String(id)), Some(Value::String(p))) if id == db => Some(p.clone()),
            _ => None,
        }
    });
    Ok(primary.unwrap_or_else(|| shard_name(1)))
}

/// The node that serves a range: where Group 0 wants its leader, else its first voter, as the
/// chunks name it ([`super::chunks::collection_chunks`]).
fn node_of(e: &RouteEntry) -> u64 {
    e.leader_hint.or_else(|| e.voters.first().copied()).map_or(1, |n| n.0)
}

/// Key intervals of a collection that one node serves, by node.
pub(super) type Holders = BTreeMap<u64, Vec<(Vec<u8>, Vec<u8>)>>;

/// The nodes that serve ranges of collection `c` (its documents and its indexes), each with the
/// key intervals of the collection it serves, in node order.
pub(super) fn holders<B: Backend + ?Sized>(
    backend: &mut B,
    c: &CollectionEntry,
) -> Result<Holders, CommandError> {
    let lo = KeyBuilder::index_prefix(c.tenant, c.id, 0).as_bytes().to_vec();
    let hi = KeyBuilder::collection_end(c.tenant, c.id).as_bytes().to_vec();
    let mut out = Holders::new();
    for e in backend.route_entries()? {
        let end = if e.end.is_empty() { hi.clone() } else { e.end.clone().min(hi.clone()) };
        let start = e.start.clone().max(lo.clone());
        if start < end {
            out.entry(node_of(&e)).or_default().push((start, end));
        }
    }
    Ok(out)
}

/// What `collStats` measured of a collection, whole or on one node, already scaled.
pub(super) struct Part {
    pub(super) count: i64,
    pub(super) size: i64,
    pub(super) storage: i64,
    pub(super) index_sizes: Vec<(String, i64)>,
}

/// What a shard's document shows besides the sizes, by the collection's kind (015 FR-010).
pub(super) struct Kind {
    /// A capped collection's `max` and `maxSize`, after `capped`.
    pub(super) capped: Option<(Value, i64)>,
    /// A time series' `timeseries`, after `size`, in place of `count` and `avgObjSize`.
    pub(super) series: Option<Document>,
}

/// A shard's document of an existing collection `ns` with `nindexes` indexes, in a `refrouter`
/// 9.0.4's order (measured on 09-11-2026): its average object size a whole number and only with
/// documents, no orphaned documents and no free storage of its own (DV-037).
pub(super) fn shard_doc(ns: &str, p: &Part, kind: &Kind, nindexes: i32, scale: i64) -> Document {
    let total_index: i64 = p.index_sizes.iter().map(|(_, b)| b).sum();
    let mut sizes = Document::new();
    for (name, b) in &p.index_sizes {
        sizes.push(name, number(*b));
    }
    let mut d = Document::new().with("ns", ns).with("size", number(p.size));
    match &kind.series {
        Some(ts) => d.push("timeseries", ts.clone()),
        None => {
            d.push("count", number(p.count));
            if p.count > 0 {
                d.push("avgObjSize", i32::try_from(p.size / p.count).unwrap_or(i32::MAX));
            }
        }
    }
    d.push("numOrphanDocs", 0);
    d.push("storageSize", number(p.storage));
    d.push("freeStorageSize", 0);
    d.push("capped", kind.capped.is_some());
    if let Some((max, max_size)) = &kind.capped {
        d.push("max", max.clone());
        // An `int` in a shard's document, a `long` in the router's (measured).
        d.push("maxSize", number(*max_size));
    }
    d.with("nindexes", nindexes)
        .with("indexBuilds", Value::Array(Vec::new()))
        .with("totalIndexSize", number(total_index))
        .with("indexSizes", sizes)
        .with("totalSize", number(p.storage + total_index))
        .with("scaleFactor", number(scale))
        .with("ok", 1.0)
}

/// A shard's document of collection `ns`, which does not exist in a database that does.
pub(super) fn missing_shard_doc(ns: &str, scale: i64) -> Document {
    Document::new()
        .with("ns", ns)
        .with("size", 0)
        .with("count", 0)
        .with("numOrphanDocs", 0)
        .with("storageSize", 0)
        .with("totalSize", 0)
        .with("nindexes", 0)
        .with("totalIndexSize", 0)
        .with("indexSizes", Document::new())
        .with("scaleFactor", number(scale))
        .with("ok", 1.0)
}

/// A count as RefDb writes it: an `int` when it fits.
fn number(n: i64) -> Value {
    i32::try_from(n).map_or(Value::Int64(n), Value::Int32)
}
