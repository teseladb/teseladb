//! 007 FR-011: a stray frame (one whose groups this core does not run) is split into a frame for
//! each core of the node that runs one of its groups; the parts of groups no core runs are dropped,
//! and the frames every core takes whatever its groups never stray.

use teseladb_multiraft::chunks::Chunk;
use teseladb_multiraft::{PeerFrame, route_stray};
use teseladb_raft::{Message, MessageType};
use teseladb_types::{Bytes, GroupId, NodeId};

const HLC: teseladb_hlc::Hlc = teseladb_hlc::Hlc { physical: 7, logical: 1 };

/// Groups 1 and 3 run on core 2, group 2 on core 1; no core runs group 4.
fn holder(g: GroupId) -> Option<u16> {
    match g.0 {
        1 | 3 => Some(2),
        2 => Some(1),
        _ => None,
    }
}

fn msg(term: u64) -> Message {
    Message::new(MessageType::Heartbeat, NodeId(1), NodeId(2), term)
}

#[test]
fn raft_messages_go_to_the_core_of_each_group_and_unknown_groups_are_dropped() {
    let msgs = (1..=4).map(|g| (GroupId(g), msg(g))).collect();
    let frame = PeerFrame::Raft { wall_ms: 9, hlc: HLC, msgs };
    let parts = route_stray(frame, holder);
    let expected = vec![
        (1, PeerFrame::Raft { wall_ms: 9, hlc: HLC, msgs: vec![(GroupId(2), msg(2))] }),
        (
            2,
            PeerFrame::Raft {
                wall_ms: 9,
                hlc: HLC,
                msgs: vec![(GroupId(1), msg(1)), (GroupId(3), msg(3))],
            },
        ),
    ];
    assert_eq!(parts, expected);
}

#[test]
fn a_frame_of_groups_no_core_runs_goes_nowhere() {
    let frame = PeerFrame::Raft { wall_ms: 1, hlc: HLC, msgs: vec![(GroupId(4), msg(1))] };
    assert!(route_stray(frame, holder).is_empty());
}

#[test]
fn snapshot_chunks_and_acks_go_to_their_group_core() {
    let chunk = |g| Chunk {
        group: GroupId(g),
        index: 5,
        term: 2,
        offset: 0,
        total: 3,
        data: Bytes::from_vec(vec![1, 2, 3]),
    };
    let to = route_stray(PeerFrame::SnapshotChunk(chunk(2)), holder);
    assert_eq!(to, vec![(1, PeerFrame::SnapshotChunk(chunk(2)))]);
    assert!(route_stray(PeerFrame::SnapshotChunk(chunk(4)), holder).is_empty());
    let ack = |g| PeerFrame::SnapshotAck { group: GroupId(g), index: 5, offset: 3, ok: true };
    assert_eq!(route_stray(ack(3), holder), vec![(2, ack(3))]);
    assert!(route_stray(ack(4), holder).is_empty());
}

#[test]
fn frames_every_core_takes_never_stray() {
    let frames = [
        PeerFrame::Heartbeats { wall_ms: 1, hlc: HLC, beats: vec![] },
        PeerFrame::HeartbeatResps { wall_ms: 1, hlc: HLC, resps: vec![] },
        PeerFrame::Forward { req: 1, deadline_ms: 10, hlc: None, body: Bytes::from_vec(vec![1]) },
        PeerFrame::ForwardResp { req: 1, hlc: None, body: Bytes::from_vec(vec![2]) },
    ];
    for f in frames {
        assert!(route_stray(f, holder).is_empty());
    }
}
