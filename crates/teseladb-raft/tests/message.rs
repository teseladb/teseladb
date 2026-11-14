//! Message encoding: round trip of every type and rejection of invalid input.

use teseladb_raft::{
    ConfState, Entry, EntryKind, Message, MessageType, NodeId, Snapshot, SnapshotMeta,
};
use teseladb_types::Bytes;

#[test]
fn every_type_round_trips() {
    use MessageType::*;
    for ty in [
        AppendReq,
        AppendResp,
        VoteReq,
        VoteResp,
        PreVoteReq,
        PreVoteResp,
        SnapshotReq,
        TimeoutNow,
        ReadIndexReq,
        ReadIndexResp,
        Heartbeat,
        HeartbeatResp,
    ] {
        let mut m = Message::new(ty, NodeId(1), NodeId(2), 7);
        m.log_term = 6;
        m.index = 100;
        m.commit = 99;
        m.reject = ty == AppendResp;
        m.reject_hint = 55;
        m.ctx = u64::MAX;
        m.entries = vec![
            Entry { index: 101, term: 7, kind: EntryKind::Normal, data: Bytes::from("a") },
            Entry { index: 102, term: 7, kind: EntryKind::ConfChange, data: Bytes::new() },
        ];
        if ty == SnapshotReq {
            m.snapshot = Some(Snapshot {
                meta: SnapshotMeta {
                    index: 90,
                    term: 5,
                    conf: ConfState {
                        voters: vec![NodeId(1), NodeId(2)],
                        learners: vec![NodeId(3)],
                    },
                },
                data: Bytes::from("data"),
            });
        }
        let b = m.to_bytes();
        assert_eq!(Message::from_bytes(&b).expect("from_bytes succeeds"), m, "{ty:?}");
        assert!(Message::from_bytes(&b[..b.len() - 1]).is_err());
        assert!(Message::from_bytes(&[b.clone(), vec![0]].concat()).is_err());
    }
    // Unknown type, unknown entry kind, too many entries, unsorted configuration.
    let mut bad = Message::new(AppendReq, NodeId(1), NodeId(2), 1).to_bytes();
    bad[0] = 99;
    assert!(Message::from_bytes(&bad).is_err());
    let mut m = Message::new(AppendReq, NodeId(1), NodeId(2), 1);
    m.entries = vec![Entry { index: 1, term: 1, kind: EntryKind::Normal, data: Bytes::new() }];
    let mut b = m.to_bytes();
    let kind_at = b.len() - 2;
    b[kind_at] = 9;
    assert!(Message::from_bytes(&b).is_err());
    let mut many = Message::new(AppendReq, NodeId(1), NodeId(2), 1).to_bytes();
    let n_at = many.len() - 1;
    many[n_at] = 0x7f;
    assert!(Message::from_bytes(&many).is_err());
    let mut snap = Message::new(SnapshotReq, NodeId(1), NodeId(2), 1);
    snap.snapshot = Some(Snapshot {
        meta: SnapshotMeta {
            conf: ConfState { voters: vec![NodeId(2), NodeId(1)], learners: vec![] },
            ..Default::default()
        },
        data: Bytes::new(),
    });
    assert!(Message::from_bytes(&snap.to_bytes()).is_err(), "unsorted voters");
    snap.snapshot = Some(Snapshot {
        meta: SnapshotMeta {
            conf: ConfState { voters: vec![NodeId(1)], learners: vec![NodeId(1)] },
            ..Default::default()
        },
        data: Bytes::new(),
    });
    assert!(Message::from_bytes(&snap.to_bytes()).is_err(), "a learner that also votes");
    let entry = Entry { index: 1, term: 1, kind: EntryKind::Normal, data: Bytes::from("xyz") };
    assert_eq!(entry.size(), 27);
}

#[test]
fn config_validation() {
    use teseladb_raft::RaftConfig;
    let ok = RaftConfig::new(NodeId(1));
    assert_eq!(ok.validate(), Ok(()));
    for bad in [
        RaftConfig { id: NodeId(0), ..ok.clone() },
        RaftConfig { heartbeat_ticks: 0, ..ok.clone() },
        RaftConfig { heartbeat_ticks: 20, ..ok.clone() },
        RaftConfig { election_ticks_max: 15, ..ok.clone() },
        RaftConfig { max_inflight_msgs: 0, ..ok.clone() },
    ] {
        assert!(bad.validate().is_err());
    }
}
