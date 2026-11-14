//! Group configuration: decoding of `ConfState`, changes on learners and validation of
//! `RaftConfig`.

use teseladb_raft::{ConfChange, ConfChangeKind, ConfState, NodeId, RaftConfig};
use teseladb_types::{Decoder, Encoder};

#[test]
fn a_member_list_longer_than_the_input_is_rejected() {
    let mut e = Encoder::new();
    e.var(1_000);
    e.u64(1);
    let bytes = e.finish();
    assert!(ConfState::decode(&mut Decoder::new(&bytes)).is_err());
}

#[test]
fn removing_a_learner_keeps_the_voters() {
    let c = ConfState { voters: vec![NodeId(1), NodeId(2)], learners: vec![NodeId(3)] };
    let out = ConfChange { kind: ConfChangeKind::Remove, node: NodeId(3) }
        .apply(&c)
        .expect("apply succeeds");
    assert_eq!(out, ConfState { voters: vec![NodeId(1), NodeId(2)], learners: vec![] });
}

#[test]
fn zero_flow_limits_are_invalid() {
    let ok = RaftConfig::new(NodeId(1));
    assert_eq!(ok.validate(), Ok(()));
    let no_inflight = RaftConfig { max_inflight_msgs: 0, ..ok.clone() };
    assert_eq!(no_inflight.validate(), Err("zero flow limits"));
    let no_bytes = RaftConfig { max_msg_bytes: 0, ..ok };
    assert_eq!(no_bytes.validate(), Err("zero flow limits"));
}

#[test]
fn a_leader_cannot_step_down_before_one_election_timeout() {
    let ok = RaftConfig::new(NodeId(1));
    let early = RaftConfig { step_down_ticks: ok.election_ticks_min - 1, ..ok };
    assert_eq!(early.validate(), Err("a leader cannot step down before one election timeout"));
}
