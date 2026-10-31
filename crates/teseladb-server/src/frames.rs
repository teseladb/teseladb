//! Translation between the frames of `teseladb-multiraft` (sans-I/O) and the messages of the
//! inter-node port of `teseladb-io`: the same fields, in each crate's own types.

use teseladb_io::peer::{
    GroupHeartbeat as WireHeartbeat, GroupHeartbeatResp as WireHeartbeatResp, PeerMsg,
};
use teseladb_multiraft::chunks::Chunk;
use teseladb_multiraft::{GroupHeartbeat, GroupHeartbeatResp, PeerFrame};
use teseladb_types::GroupId;

/// The message that carries `f` on the inter-node port.
pub fn to_wire(f: PeerFrame) -> PeerMsg {
    match f {
        PeerFrame::Raft { wall_ms, hlc, msgs } => PeerMsg::RaftBatch { wall_ms, hlc, msgs },
        PeerFrame::Heartbeats { wall_ms, hlc, beats } => PeerMsg::Heartbeats {
            wall_ms,
            hlc,
            beats: beats
                .into_iter()
                .map(|h| WireHeartbeat {
                    group: h.group,
                    term: h.term,
                    commit: h.commit,
                    lease_seq: h.lease_seq,
                    closed: h.closed,
                })
                .collect(),
        },
        PeerFrame::HeartbeatResps { wall_ms, hlc, resps } => PeerMsg::HeartbeatResps {
            wall_ms,
            hlc,
            resps: resps
                .into_iter()
                .map(|r| WireHeartbeatResp {
                    group: r.group,
                    term: r.term,
                    ok: r.ok,
                    lease_seq: r.lease_seq,
                })
                .collect(),
        },
        PeerFrame::SnapshotChunk(c) => PeerMsg::SnapshotChunk {
            group: c.group.0,
            index: c.index,
            term: c.term,
            offset: c.offset,
            total: c.total,
            data: c.data,
        },
        PeerFrame::SnapshotAck { group, index, offset, ok } => {
            PeerMsg::SnapshotAck { group: group.0, index, offset, ok }
        }
        PeerFrame::Forward { req, deadline_ms, body } => {
            PeerMsg::Forward { req, deadline_ms, body }
        }
        PeerFrame::ForwardResp { req, body } => PeerMsg::ForwardResp { req, body },
    }
}

/// The frame a message of the inter-node port carries for the multi-Raft core (`None` for the
/// messages that are not the core's: `Hello` and those of later specifications).
pub fn from_wire(m: PeerMsg) -> Option<PeerFrame> {
    Some(match m {
        PeerMsg::RaftBatch { wall_ms, hlc, msgs } => PeerFrame::Raft { wall_ms, hlc, msgs },
        PeerMsg::Heartbeats { wall_ms, hlc, beats } => PeerFrame::Heartbeats {
            wall_ms,
            hlc,
            beats: beats
                .into_iter()
                .map(|h| GroupHeartbeat {
                    group: h.group,
                    term: h.term,
                    commit: h.commit,
                    lease_seq: h.lease_seq,
                    closed: h.closed,
                })
                .collect(),
        },
        PeerMsg::HeartbeatResps { wall_ms, hlc, resps } => PeerFrame::HeartbeatResps {
            wall_ms,
            hlc,
            resps: resps
                .into_iter()
                .map(|r| GroupHeartbeatResp {
                    group: r.group,
                    term: r.term,
                    ok: r.ok,
                    lease_seq: r.lease_seq,
                })
                .collect(),
        },
        PeerMsg::SnapshotChunk { group, index, term, offset, total, data } => {
            PeerFrame::SnapshotChunk(Chunk {
                group: GroupId(group),
                index,
                term,
                offset,
                total,
                data,
            })
        }
        PeerMsg::SnapshotAck { group, index, offset, ok } => {
            PeerFrame::SnapshotAck { group: GroupId(group), index, offset, ok }
        }
        PeerMsg::Forward { req, deadline_ms, body } => {
            PeerFrame::Forward { req, deadline_ms, body }
        }
        PeerMsg::ForwardResp { req, body } => PeerFrame::ForwardResp { req, body },
        _ => return None,
    })
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_types::Bytes;

    fn frames() -> Vec<PeerFrame> {
        let g = GroupId(7);
        vec![
            PeerFrame::Raft {
                wall_ms: 1_800_000_000_789,
                hlc: teseladb_hlc::Hlc { physical: 1_800_000_000_790, logical: 3 },
                msgs: Vec::new(),
            },
            PeerFrame::Heartbeats {
                wall_ms: 1_800_000_000_123,
                hlc: teseladb_hlc::Hlc { physical: 1_800_000_000_124, logical: 1 },
                beats: vec![GroupHeartbeat {
                    group: g,
                    term: 2,
                    commit: 3,
                    lease_seq: 4,
                    closed: Some((teseladb_hlc::Hlc { physical: 5, logical: 6 }, 7)),
                }],
            },
            PeerFrame::HeartbeatResps {
                wall_ms: 1_800_000_000_456,
                hlc: teseladb_hlc::Hlc { physical: 1_800_000_000_457, logical: 2 },
                resps: vec![GroupHeartbeatResp { group: g, term: 2, ok: true, lease_seq: 4 }],
            },
            PeerFrame::SnapshotChunk(Chunk {
                group: g,
                index: 9,
                term: 2,
                offset: 0,
                total: 3,
                data: Bytes::from(vec![1, 2, 3]),
            }),
            PeerFrame::SnapshotAck { group: g, index: 9, offset: 3, ok: false },
            PeerFrame::Forward { req: 5, deadline_ms: 100, body: Bytes::from(vec![8]) },
            PeerFrame::ForwardResp { req: 5, body: Bytes::new() },
        ]
    }

    #[test]
    fn every_frame_crosses_the_wire_unchanged() {
        for f in frames() {
            assert_eq!(from_wire(to_wire(f.clone())), Some(f));
        }
    }

    #[test]
    fn messages_that_are_not_the_cores_are_left_out() {
        assert_eq!(from_wire(PeerMsg::RangeRetire { group: GroupId(1) }), None);
    }
}
