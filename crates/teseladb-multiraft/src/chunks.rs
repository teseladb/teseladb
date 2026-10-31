//! Chunked transfer of a group snapshot: the sender cuts the snapshot's data in
//! chunks of at most [`CHUNK_LEN`] and the receiver puts them back together per group before
//! stepping the `SnapshotReq` with the whole data. The connection between two nodes keeps the order,
//! so a chunk that does not continue the transfer in progress either starts a new one (offset 0) or
//! is discarded.

use std::collections::BTreeMap;

use teseladb_types::{Bytes, GroupId};

/// Maximum data of a chunk.
pub const CHUNK_LEN: usize = 1 << 20;
/// Maximum size of a snapshot being received.
pub const MAX_SNAPSHOT_LEN: u64 = 1 << 30;

/// A piece of a snapshot in transit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// Group.
    pub group: GroupId,
    /// Index of the snapshot.
    pub index: u64,
    /// Term of the snapshot.
    pub term: u64,
    /// Position of `data` in the snapshot.
    pub offset: u64,
    /// Size of the whole snapshot.
    pub total: u64,
    /// Data.
    pub data: Bytes,
}

/// Cuts a snapshot in chunks (one empty chunk if the data is empty).
pub fn split(group: GroupId, index: u64, term: u64, data: &[u8]) -> Vec<Chunk> {
    let total = data.len() as u64;
    let mut out: Vec<Chunk> = data
        .chunks(CHUNK_LEN)
        .enumerate()
        .map(|(i, c)| Chunk {
            group,
            index,
            term,
            offset: (i * CHUNK_LEN) as u64,
            total,
            data: Bytes::from_vec(c.to_vec()),
        })
        .collect();
    if out.is_empty() {
        out.push(Chunk { group, index, term, offset: 0, total, data: Bytes::new() });
    }
    out
}

/// A chunk that cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkError {
    /// It does not continue the transfer in progress and does not start a new one.
    OutOfOrder,
    /// It goes past the declared size, or the size is above [`MAX_SNAPSHOT_LEN`].
    TooLarge,
}

#[derive(Debug)]
struct Partial {
    index: u64,
    term: u64,
    total: u64,
    buf: Vec<u8>,
}

/// Transfers being received, one per group.
#[derive(Debug, Default)]
pub struct Reassembly {
    partial: BTreeMap<GroupId, Partial>,
}

/// A snapshot received whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received {
    /// Group.
    pub group: GroupId,
    /// Index.
    pub index: u64,
    /// Term.
    pub term: u64,
    /// Data.
    pub data: Bytes,
}

impl Reassembly {
    /// Adds a chunk; returns the snapshot when it is complete. A failed chunk drops the group's
    /// transfer (the sender starts again after `report_snapshot(false)`).
    pub fn push(&mut self, c: Chunk) -> Result<Option<Received>, ChunkError> {
        let end = c.offset.checked_add(c.data.len() as u64);
        if c.total > MAX_SNAPSHOT_LEN || end.is_none_or(|e| e > c.total) {
            self.partial.remove(&c.group);
            return Err(ChunkError::TooLarge);
        }
        if c.offset == 0 {
            // The declared size comes from the peer: only a bounded part is reserved up front.
            let buf = Vec::with_capacity((c.total as usize).min(16 * CHUNK_LEN));
            self.partial
                .insert(c.group, Partial { index: c.index, term: c.term, total: c.total, buf });
        }
        let Some(p) = self.partial.get_mut(&c.group) else { return Err(ChunkError::OutOfOrder) };
        if (p.index, p.term, p.total) != (c.index, c.term, c.total)
            || p.buf.len() as u64 != c.offset
        {
            self.partial.remove(&c.group);
            return Err(ChunkError::OutOfOrder);
        }
        p.buf.extend_from_slice(&c.data);
        if (p.buf.len() as u64) < p.total {
            return Ok(None);
        }
        let p = self.partial.remove(&c.group).expect("transfer in progress");
        Ok(Some(Received {
            group: c.group,
            index: p.index,
            term: p.term,
            data: Bytes::from_vec(p.buf),
        }))
    }

    /// Transfers in progress.
    pub fn in_progress(&self) -> usize {
        self.partial.len()
    }
}
