//! Draining and removing a node (007 T021, FR-015): what Group 0 accepts, how far a drain is, and
//! the `topologyVersion` counter the node shows for its state.

use teseladb_meta::command::{Group0Command, NodeAddrs, NodeState};
use teseladb_meta::drain::{DrainError, drain, progress, remove, topology_counter};
use teseladb_meta::group0::{Group0Sm, NodeInfo, RangeDesc, ReplicaRole};
use teseladb_types::{GroupId, NodeId};

fn n(i: u64) -> NodeId {
    NodeId(i)
}

fn cluster(count: u64) -> Group0Sm {
    let mut sm = Group0Sm { cluster_id: [1; 16], ..Group0Sm::default() };
    for i in 1..=count {
        let info = NodeInfo {
            id: n(i),
            addrs: NodeAddrs { name: vec![i as u8], cores: 1, ..NodeAddrs::default() },
            state: NodeState::Active,
            joined_index: i,
            cert_fingerprint: [0; 32],
            boot_epoch: 1,
            tags: Vec::new(),
        };
        sm.nodes.insert(n(i), info);
    }
    sm
}

fn add_range(sm: &mut Group0Sm, g: u64, replicas: &[u64], leader: u64) {
    let desc = RangeDesc {
        group: GroupId(g),
        start: vec![g as u8],
        end: vec![g as u8 + 1],
        core: 0,
        replicas: replicas.iter().map(|&i| (n(i), ReplicaRole::Voter)).collect(),
        replica_epoch: 1,
        generation: 1,
        leader_hint: Some(n(leader)),
        leader_term: 0,
        pending_split: None,
        settling: false,
    };
    sm.insert_range(desc);
}

#[test]
fn a_drain_marks_the_node_draining_unless_it_is_the_last_active_one() {
    let mut sm = cluster(3);
    assert_eq!(
        drain(&sm, n(2)),
        Ok(Group0Command::SetNodeState { node: n(2), state: NodeState::Draining })
    );
    assert_eq!(drain(&sm, n(9)), Err(DrainError::UnknownNode(n(9))));
    sm.nodes.get_mut(&n(2)).unwrap().state = NodeState::Draining;
    // Asked again: the same command, which changes nothing.
    assert_eq!(
        drain(&sm, n(2)),
        Ok(Group0Command::SetNodeState { node: n(2), state: NodeState::Draining })
    );
    sm.nodes.get_mut(&n(3)).unwrap().state = NodeState::Removed;
    assert_eq!(drain(&sm, n(3)), Err(DrainError::Removed(n(3))));
    // Node 1 is the only active node left: nothing would take its replicas.
    assert_eq!(drain(&sm, n(1)), Err(DrainError::LastActive(n(1))));
    let msg = DrainError::LastActive(n(1)).to_string();
    assert!(msg.contains("last active node"), "{msg}");
}

#[test]
fn the_progress_counts_replicas_leaders_and_the_group0_vote() {
    let mut sm = cluster(4);
    add_range(&mut sm, 1, &[1, 2, 3], 2);
    add_range(&mut sm, 2, &[2, 3, 4], 3);
    add_range(&mut sm, 3, &[1, 3, 4], 1);
    let voters = [n(1), n(2), n(3)];
    let p = progress(&sm, &voters, n(2));
    assert_eq!((p.replicas, p.leaders, p.group0_voter), (2, 1, true));
    assert!(!p.drained());
    let p = progress(&sm, &[n(1), n(3), n(4)], n(2));
    assert!(!p.drained(), "still two replicas");
    let p = progress(&sm, &[n(1), n(3), n(4)], n(9));
    assert!(p.drained(), "a node with nothing is drained");
}

#[test]
fn only_a_node_that_holds_nothing_is_removed() {
    let mut sm = cluster(4);
    add_range(&mut sm, 1, &[1, 2, 3], 1);
    let voters = [n(1), n(2), n(3)];
    let err = remove(&sm, &voters, n(2)).unwrap_err();
    assert_eq!(err, DrainError::StillHolds { node: n(2), replicas: 1, group0_voter: true });
    let msg = err.to_string();
    assert!(msg.contains("1 replica") && msg.contains("drain it first"), "{msg}");
    assert_eq!(
        remove(&sm, &voters, n(4)),
        Ok(Group0Command::SetNodeState { node: n(4), state: NodeState::Removed })
    );
    assert_eq!(remove(&sm, &voters, n(9)), Err(DrainError::UnknownNode(n(9))));
    sm.nodes.get_mut(&n(4)).unwrap().state = NodeState::Removed;
    assert_eq!(remove(&sm, &voters, n(4)), Err(DrainError::Removed(n(4))));
    // A Group 0 voter without replicas is not removed while it votes.
    let err = remove(&sm, &[n(1), n(2), n(4)], n(4));
    assert_eq!(err, Err(DrainError::Removed(n(4))));
    sm.nodes.get_mut(&n(4)).unwrap().state = NodeState::Draining;
    let err = remove(&sm, &[n(1), n(2), n(4)], n(4)).unwrap_err();
    assert!(err.to_string().contains("votes in Group 0"), "{err}");
}

#[test]
fn the_topology_counter_grows_with_the_state() {
    let c = [NodeState::Active, NodeState::Draining, NodeState::Removed].map(topology_counter);
    assert!(c[0] < c[1] && c[1] < c[2], "{c:?}");
    assert_eq!(c[0], 0, "an active node shows the counter it started with");
}
