//! `$shardedDataDistribution` (018 FR-017): for each collection with a shard key, the documents
//! and bytes each shard owns, the shard of a chunk being the node that leads its range
//! ([`super::chunks`]). A range here holds no orphaned documents, so their counts are 0. The
//! bytes are the shard's documents times their average size in whole bytes, as a `refrouter`
//! 9.0.4 gives them (measured on 02-11-2026, `t13.js` in the lane's notes).

use std::collections::BTreeMap;

use teseladb_range::read::ReadConcern;
use teseladb_bson::{Document, Value};

use super::chunks::collection_chunks;
use super::sharding::shard_key_of;
use super::{Backend, shard_name};
use crate::error::CommandError;

/// Rows read at a time.
const SCAN: usize = super::query::SCAN_CHUNK;

/// The stage's documents.
pub fn documents<B: Backend + ?Sized>(backend: &mut B) -> Result<Vec<Document>, CommandError> {
    let entries = backend.route_entries()?;
    let mut out = Vec::new();
    for (ns, coll) in backend.collections()? {
        let Some(key) = shard_key_of(&coll)? else { continue };
        let mut owned: BTreeMap<u64, (i64, i64)> = BTreeMap::new();
        for chunk in collection_chunks(&entries, &coll, &key) {
            let (docs, bytes) = owned.entry(chunk.node.0).or_default();
            let mut next = chunk.start.clone();
            loop {
                let rows = backend.scan(&next, &chunk.end, SCAN, ReadConcern::Local)?;
                *docs += i64::try_from(rows.len()).unwrap_or(i64::MAX);
                *bytes +=
                    rows.iter().map(|(_, v)| i64::try_from(v.len()).unwrap_or(0)).sum::<i64>();
                match rows.last() {
                    Some((k, _)) if rows.len() == SCAN => next = [k.as_slice(), &[0]].concat(),
                    _ => break,
                }
            }
        }
        let shards: Vec<Value> = owned
            .into_iter()
            .map(|(node, (docs, bytes))| {
                let size = match docs {
                    0 => 0,
                    n => bytes / n * n,
                };
                Value::Document(
                    Document::new()
                        .with("shardName", shard_name(node))
                        .with("numOrphanedDocs", 0i64)
                        .with("numOwnedDocuments", docs)
                        .with("ownedSizeBytes", size)
                        .with("orphanedSizeBytes", 0i64),
                )
            })
            .collect();
        out.push(
            Document::new()
                .with("ns", super::names::full(&ns))
                .with("shards", Value::Array(shards)),
        );
    }
    Ok(out)
}
