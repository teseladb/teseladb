//! The history of the ranges (007 FR-017, `config.changelog`): each split, merge, replica change
//! and core change that a holder of descriptors (Group 0 or a `Meta2` range) completes, with the
//! mark of the entry that completed it. The history is bounded by its bytes: it keeps the last
//! changes that fit in [`MAX_BYTES`], the size of a reference router's capped
//! `config.changelog`.

use std::collections::VecDeque;

use teseladb_types::{DecodeError, Decoder, Encoder, GroupId, NodeId};

/// Bytes of changes a holder keeps, the size of a reference router's capped `config.changelog`
/// (209 715 200, measured on 9.0.4).
pub const MAX_BYTES: u64 = 200 * 1024 * 1024;

/// What changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    /// The range split at `at`; `other` is the new range, `[at, end)`.
    Split {
        /// The key of the cut.
        at: Vec<u8>,
        /// The new range.
        other: GroupId,
    },
    /// The range absorbed its right neighbour `other`; `end` is the end of both.
    Merge {
        /// The range absorbed.
        other: GroupId,
    },
    /// Replicas were added and removed.
    Replicas {
        /// The nodes added.
        added: Vec<NodeId>,
        /// The nodes removed.
        removed: Vec<NodeId>,
    },
    /// Every replica runs on `core` now.
    Core {
        /// The new core.
        core: u16,
    },
}

/// A change of one range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// Its number in the holder's history, from 1.
    pub seq: u64,
    /// The mark of the entry that completed it, in milliseconds.
    pub time_ms: u64,
    /// The range.
    pub group: GroupId,
    /// The range's first key and the first key past it, before the change (a merge: both
    /// ranges together).
    pub start: Vec<u8>,
    /// See `start`.
    pub end: Vec<u8>,
    /// The node the range's leader was on, if known.
    pub leader: Option<NodeId>,
    /// What changed.
    pub kind: ChangeKind,
}

/// The history of a holder of descriptors.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changelog {
    /// The changes, oldest first.
    pub changes: VecDeque<Change>,
    /// The number of the last change.
    pub last_seq: u64,
    /// The bytes of `changes` ([`Change::size`]).
    bytes: u64,
}

const SPLIT: u8 = 1;
const MERGE: u8 = 2;
const REPLICAS: u8 = 3;
const CORE: u8 = 4;

fn put_nodes(e: &mut Encoder, nodes: &[NodeId]) {
    e.var(nodes.len() as u64);
    for n in nodes {
        e.u64(n.0);
    }
}

fn get_nodes(d: &mut Decoder<'_>) -> Result<Vec<NodeId>, DecodeError> {
    let n = d.var()?;
    (0..n).map(|_| d.u64().map(NodeId)).collect()
}

fn encode_change(e: &mut Encoder, c: &Change) {
    e.u64(c.seq).u64(c.time_ms).u64(c.group.0).bytes(&c.start).bytes(&c.end);
    e.u64(c.leader.map_or(0, |n| n.0));
    match &c.kind {
        ChangeKind::Split { at, other } => {
            e.u8(SPLIT).bytes(at).u64(other.0);
        }
        ChangeKind::Merge { other } => {
            e.u8(MERGE).u64(other.0);
        }
        ChangeKind::Replicas { added, removed } => {
            e.u8(REPLICAS);
            put_nodes(e, added);
            put_nodes(e, removed);
        }
        ChangeKind::Core { core } => {
            e.u8(CORE).u16(*core);
        }
    }
}

impl Change {
    /// The bytes the holder keeps for it: its encoding.
    pub fn size(&self) -> u64 {
        let mut e = Encoder::new();
        encode_change(&mut e, self);
        e.len() as u64
    }
}

impl Changelog {
    /// The bytes of the changes kept.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Records `change` with the next number; the oldest ones go while the bytes kept pass
    /// [`MAX_BYTES`].
    pub fn record(&mut self, mut change: Change) {
        self.last_seq += 1;
        change.seq = self.last_seq;
        self.bytes += change.size();
        self.changes.push_back(change);
        while self.bytes > MAX_BYTES {
            // `bytes` past the bound means a change is kept.
            self.bytes -= self.changes.pop_front().map_or(self.bytes, |old| old.size());
        }
    }

    /// Writes it into `e`.
    pub fn encode(&self, e: &mut Encoder) {
        e.u64(self.last_seq).var(self.changes.len() as u64);
        for c in &self.changes {
            encode_change(e, c);
        }
    }

    /// Reads what [`Changelog::encode`] wrote.
    pub fn decode(d: &mut Decoder<'_>) -> Result<Changelog, DecodeError> {
        let last_seq = d.u64()?;
        let n = d.var()?;
        let mut changes = VecDeque::new();
        let mut bytes = 0;
        for _ in 0..n {
            let (seq, time_ms, group) = (d.u64()?, d.u64()?, GroupId(d.u64()?));
            let (start, end) = (d.bytes()?.to_vec(), d.bytes()?.to_vec());
            let leader = Some(d.u64()?).filter(|n| *n != 0).map(NodeId);
            let at = d.position();
            let kind = match d.u8()? {
                SPLIT => ChangeKind::Split { at: d.bytes()?.to_vec(), other: GroupId(d.u64()?) },
                MERGE => ChangeKind::Merge { other: GroupId(d.u64()?) },
                REPLICAS => ChangeKind::Replicas { added: get_nodes(d)?, removed: get_nodes(d)? },
                CORE => ChangeKind::Core { core: d.u16()? },
                _ => return Err(DecodeError::InvalidValue { offset: at }),
            };
            let change = Change { seq, time_ms, group, start, end, leader, kind };
            bytes += change.size();
            changes.push_back(change);
        }
        Ok(Changelog { changes, last_seq, bytes })
    }
}
