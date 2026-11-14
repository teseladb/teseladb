//! Raft messages and their encoding (`RaftBatch` frame, contract `peer-frames.md` of specification 007), hard state and
//! snapshots.

use teseladb_types::{Bytes, Decoder, Encoder, NodeId};

pub use teseladb_types::DecodeError;

use crate::conf::ConfState;

/// Entry kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// State machine data.
    Normal,
    /// Configuration change (an encoded [`crate::ConfChange`]).
    ConfChange,
}

/// Log entry.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Entry {
    /// Index.
    pub index: u64,
    /// Term.
    pub term: u64,
    /// Kind.
    pub kind: EntryKind,
    /// Data (empty in a new leader's `Noop`).
    pub data: Bytes,
}

impl Entry {
    /// Bytes counted for the flow limits.
    #[must_use]
    pub fn size(&self) -> usize {
        self.data.len() + 24
    }
}

/// Hard state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct HardState {
    /// Current term.
    pub term: u64,
    /// Vote in the current term (0 = none).
    pub vote: NodeId,
    /// Known commit index.
    pub commit: u64,
}

/// Snapshot metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct SnapshotMeta {
    /// Index it reflects.
    pub index: u64,
    /// Term of that index.
    pub term: u64,
    /// Configuration at that index.
    pub conf: ConfState,
}

/// Snapshot (the caller provides the data; on the network it travels in separate chunks).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Snapshot {
    /// Metadata.
    pub meta: SnapshotMeta,
    /// State machine data.
    pub data: Bytes,
}

/// Message type (the numbers are stable).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MessageType {
    /// Entries (or a probe) from the leader.
    AppendReq = 1,
    /// A follower's response.
    AppendResp = 2,
    /// Vote request.
    VoteReq = 3,
    /// Vote.
    VoteResp = 4,
    /// Pre-vote request.
    PreVoteReq = 5,
    /// Pre-vote.
    PreVoteResp = 6,
    /// Install a snapshot.
    SnapshotReq = 7,
    /// Leadership transfer: start the election now.
    TimeoutNow = 8,
    /// `ReadIndex` read requested from the leader.
    ReadIndexReq = 9,
    /// Index of a read.
    ReadIndexResp = 10,
    /// Heartbeat (the core coalesces them per node pair in the `Heartbeats` frame).
    Heartbeat = 11,
    /// Response to a heartbeat (`HeartbeatResps`).
    HeartbeatResp = 12,
}

impl MessageType {
    fn from_u8(b: u8) -> Option<MessageType> {
        use MessageType::{
            AppendReq, AppendResp, Heartbeat, HeartbeatResp, PreVoteReq, PreVoteResp, ReadIndexReq,
            ReadIndexResp, SnapshotReq, TimeoutNow, VoteReq, VoteResp,
        };
        Some(match b {
            1 => AppendReq,
            2 => AppendResp,
            3 => VoteReq,
            4 => VoteResp,
            5 => PreVoteReq,
            6 => PreVoteResp,
            7 => SnapshotReq,
            8 => TimeoutNow,
            9 => ReadIndexReq,
            10 => ReadIndexResp,
            11 => Heartbeat,
            12 => HeartbeatResp,
            _ => return None,
        })
    }
}

/// Raft message of a group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// Type.
    pub ty: MessageType,
    /// Source.
    pub from: NodeId,
    /// Destination.
    pub to: NodeId,
    /// Sender's term.
    pub term: u64,
    /// Term of entry `index` (the one before the entries in `AppendReq`; the last one in votes).
    pub log_term: u64,
    /// Index (the one before the entries in `AppendReq`, the committed one in a response).
    pub index: u64,
    /// Leader's commit index.
    pub commit: u64,
    /// Rejection.
    pub reject: bool,
    /// Hint of a rejection (the follower's last index).
    pub reject_hint: u64,
    /// Context: heartbeat round, read context or term of the transferring leader.
    pub ctx: u64,
    /// Entries (consecutive indexes from `index + 1`).
    pub entries: Vec<Entry>,
    /// Snapshot (`SnapshotReq`).
    pub snapshot: Option<Snapshot>,
}

impl Message {
    /// Empty message of a type.
    #[must_use]
    pub fn new(ty: MessageType, from: NodeId, to: NodeId, term: u64) -> Message {
        Message {
            ty,
            from,
            to,
            term,
            log_term: 0,
            index: 0,
            commit: 0,
            reject: false,
            reject_hint: 0,
            ctx: 0,
            entries: Vec::new(),
            snapshot: None,
        }
    }

    /// Binary encoding.
    pub fn encode(&self, e: &mut Encoder) {
        e.u8(self.ty as u8).u64(self.from.0).u64(self.to.0).var(self.term).var(self.log_term);
        e.var(self.index)
            .var(self.commit)
            .u8(u8::from(self.reject))
            .var(self.reject_hint)
            .var(self.ctx);
        e.var(self.entries.len() as u64);
        for en in &self.entries {
            e.var(en.term).u8(match en.kind {
                EntryKind::Normal => 0,
                EntryKind::ConfChange => 1,
            });
            e.bytes(&en.data);
        }
        if self.ty == MessageType::SnapshotReq {
            let s = self.snapshot.clone().unwrap_or_default();
            e.var(s.meta.index).var(s.meta.term);
            s.meta.conf.encode(e);
            e.bytes(&s.data);
        }
    }

    /// Encoding into a new buffer.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        self.encode(&mut e);
        e.finish()
    }

    /// Decodes a message.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when the bytes end early or hold an unknown type or value.
    pub fn decode(d: &mut Decoder<'_>) -> Result<Message, DecodeError> {
        let at = d.position();
        let ty = MessageType::from_u8(d.u8()?).ok_or(DecodeError::InvalidValue { offset: at })?;
        let mut m = Message::new(ty, NodeId(d.u64()?), NodeId(d.u64()?), d.var()?);
        m.log_term = d.var()?;
        m.index = d.var()?;
        m.commit = d.var()?;
        m.reject = d.bool()?;
        m.reject_hint = d.var()?;
        m.ctx = d.var()?;
        let n = d.var()?;
        if n > d.remaining() as u64 {
            return Err(DecodeError::InvalidValue { offset: d.position() });
        }
        for i in 0..n {
            let term = d.var()?;
            let at = d.position();
            let kind = match d.u8()? {
                0 => EntryKind::Normal,
                1 => EntryKind::ConfChange,
                _ => return Err(DecodeError::InvalidValue { offset: at }),
            };
            let data = Bytes::copy_from_slice(d.bytes()?);
            m.entries.push(Entry { index: m.index + 1 + i, term, kind, data });
        }
        if ty == MessageType::SnapshotReq {
            let index = d.var()?;
            let term = d.var()?;
            let conf = ConfState::decode(d)?;
            let data = Bytes::copy_from_slice(d.bytes()?);
            m.snapshot = Some(Snapshot { meta: SnapshotMeta { index, term, conf }, data });
        }
        Ok(m)
    }

    /// Decodes a message that takes all of `b`.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] for malformed bytes or bytes left after the message.
    pub fn from_bytes(b: &[u8]) -> Result<Message, DecodeError> {
        let mut d = Decoder::new(b);
        let m = Message::decode(&mut d)?;
        d.finish()?;
        Ok(m)
    }
}
