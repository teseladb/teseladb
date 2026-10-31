//! What a core sends to other nodes. The server translates these frames to those of the
//! inter-node protocol (contract `peer-frames.md` of 007): Raft messages travel in one batch per
//! destination, and heartbeats and their answers, the most frequent messages with thousands of
//! groups per node, in compact lists of a few numbers each.

use std::collections::BTreeMap;

use teseladb_hlc::Hlc;
use teseladb_raft::{Message, MessageType};
use teseladb_types::{Bytes, GroupId, NodeId};

use crate::chunks::Chunk;

/// A group's heartbeat inside [`PeerFrame::Heartbeats`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupHeartbeat {
    /// Group.
    pub group: GroupId,
    /// Leader's term.
    pub term: u64,
    /// Commit index the follower may take.
    pub commit: u64,
    /// Heartbeat round (`ctx` of the Raft message).
    pub lease_seq: u64,
    /// The leader's closed mark and its last log index when it closed it (007 FR-012): it writes
    /// nothing more at that mark or below, and every write at or below it is at that index or
    /// before.
    pub closed: Option<(Hlc, u64)>,
}

/// A group's answer to a heartbeat inside [`PeerFrame::HeartbeatResps`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupHeartbeatResp {
    /// Group.
    pub group: GroupId,
    /// Responder's term.
    pub term: u64,
    /// Accepted.
    pub ok: bool,
    /// Round it answers.
    pub lease_seq: u64,
}

/// A frame for another node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerFrame {
    /// Raft messages of several groups.
    Raft {
        /// Sender's wall clock (Unix ms): every frame from a peer is a clock sample (005 FR-006).
        wall_ms: u64,
        /// Sender's HLC: the receiver's clock moves past it (005 FR-003), so a leader elected
        /// after another learns, through its voters, the marks the other may have read at.
        hlc: Hlc,
        /// The messages.
        msgs: Vec<(GroupId, Message)>,
    },
    /// Heartbeats of several groups, with the sender's wall clock when it sent them (005 FR-006:
    /// the receiver compares its clock with its peers').
    Heartbeats {
        /// Sender's wall clock (Unix ms).
        wall_ms: u64,
        /// Sender's HLC (005 FR-003).
        hlc: Hlc,
        /// The heartbeats.
        beats: Vec<GroupHeartbeat>,
    },
    /// Answers to heartbeats of several groups.
    HeartbeatResps {
        /// Sender's wall clock (Unix ms): the leader compares its clock with its followers'
        /// (005 FR-006).
        wall_ms: u64,
        /// Sender's HLC (005 FR-003).
        hlc: Hlc,
        /// The answers.
        resps: Vec<GroupHeartbeatResp>,
    },
    /// A piece of a snapshot.
    SnapshotChunk(Chunk),
    /// End of a snapshot transfer.
    SnapshotAck {
        /// Group.
        group: GroupId,
        /// Index of the snapshot.
        index: u64,
        /// Bytes received.
        offset: u64,
        /// Installed.
        ok: bool,
    },
    /// A request this node forwards to another one.
    Forward {
        /// Request number.
        req: u64,
        /// Milliseconds the sender waits for the answer.
        deadline_ms: u32,
        /// Request, encoded by the layer above.
        body: Bytes,
    },
    /// Answer to a forwarded request.
    ForwardResp {
        /// Request number.
        req: u64,
        /// Answer, encoded by the layer above.
        body: Bytes,
    },
}

#[derive(Default)]
struct ToNode {
    batch: Vec<(GroupId, Message)>,
    heartbeats: Vec<GroupHeartbeat>,
    resps: Vec<GroupHeartbeatResp>,
}

/// The parts of a stray frame ([`MultiRaftCore::take_strays`](crate::MultiRaftCore::take_strays))
/// for each core of the node that runs one of its groups, as `holder` names them; the parts of
/// groups no core runs are dropped (007 FR-011).
pub fn route_stray(
    frame: PeerFrame,
    holder: impl Fn(GroupId) -> Option<u16>,
) -> Vec<(u16, PeerFrame)> {
    match frame {
        PeerFrame::Raft { wall_ms, hlc, msgs } => {
            let mut by_core: BTreeMap<u16, Vec<(GroupId, Message)>> = BTreeMap::new();
            for (g, m) in msgs {
                if let Some(core) = holder(g) {
                    by_core.entry(core).or_default().push((g, m));
                }
            }
            by_core
                .into_iter()
                .map(|(c, msgs)| (c, PeerFrame::Raft { wall_ms, hlc, msgs }))
                .collect()
        }
        PeerFrame::SnapshotChunk(c) => {
            holder(c.group).map(|core| (core, PeerFrame::SnapshotChunk(c))).into_iter().collect()
        }
        f @ PeerFrame::SnapshotAck { group, .. } => {
            holder(group).map(|core| (core, f)).into_iter().collect()
        }
        // The core takes every other frame whatever its groups.
        PeerFrame::Heartbeats { .. }
        | PeerFrame::HeartbeatResps { .. }
        | PeerFrame::Forward { .. }
        | PeerFrame::ForwardResp { .. } => Vec::new(),
    }
}

/// Groups Raft messages per destination: one `Raft` frame and the heartbeats and their answers in
/// their compact lists. Messages to `me` are dropped (a single voter answers itself). A heartbeat
/// of group `g` in term `t` carries `closed(g, t)`.
pub fn coalesce(
    me: NodeId,
    out: Vec<(GroupId, Message)>,
    wall_ms: u64,
    hlc: Hlc,
    closed: &dyn Fn(GroupId, u64) -> Option<(Hlc, u64)>,
) -> Vec<(NodeId, PeerFrame)> {
    let mut by_node: BTreeMap<NodeId, ToNode> = BTreeMap::new();
    for (g, m) in out {
        if m.to == me {
            continue;
        }
        let to = by_node.entry(m.to).or_default();
        match m.ty {
            MessageType::Heartbeat => to.heartbeats.push(GroupHeartbeat {
                group: g,
                term: m.term,
                commit: m.commit,
                lease_seq: m.ctx,
                closed: closed(g, m.term),
            }),
            MessageType::HeartbeatResp => to.resps.push(GroupHeartbeatResp {
                group: g,
                term: m.term,
                ok: !m.reject,
                lease_seq: m.ctx,
            }),
            _ => to.batch.push((g, m)),
        }
    }
    let mut frames = Vec::new();
    for (node, to) in by_node {
        if !to.batch.is_empty() {
            frames.push((node, PeerFrame::Raft { wall_ms, hlc, msgs: to.batch }));
        }
        if !to.heartbeats.is_empty() {
            frames.push((node, PeerFrame::Heartbeats { wall_ms, hlc, beats: to.heartbeats }));
        }
        if !to.resps.is_empty() {
            frames.push((node, PeerFrame::HeartbeatResps { wall_ms, hlc, resps: to.resps }));
        }
    }
    frames
}

/// The Raft messages a heartbeat frame from `from` to `me` stands for.
pub fn expand_heartbeats(
    from: NodeId,
    me: NodeId,
    hs: &[GroupHeartbeat],
) -> Vec<(GroupId, Message)> {
    hs.iter()
        .map(|h| {
            let mut m = Message::new(MessageType::Heartbeat, from, me, h.term);
            (m.commit, m.ctx) = (h.commit, h.lease_seq);
            (h.group, m)
        })
        .collect()
}

/// The Raft messages an answer frame from `from` to `me` stands for.
pub fn expand_heartbeat_resps(
    from: NodeId,
    me: NodeId,
    rs: &[GroupHeartbeatResp],
) -> Vec<(GroupId, Message)> {
    rs.iter()
        .map(|r| {
            let mut m = Message::new(MessageType::HeartbeatResp, from, me, r.term);
            (m.reject, m.ctx) = (!r.ok, r.lease_seq);
            (r.group, m)
        })
        .collect()
}
