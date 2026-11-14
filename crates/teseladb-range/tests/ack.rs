//! Acknowledging a write to the client (005 T014, FR-010, D-034): `w: 1` once the entry is in
//! the leader's log (on its disk with `j: true`), `w: <n>` once n replicas have it on disk
//! whatever `j` says, `w: "majority"` once it is applied on the leader, `w: 0` never, and a `w`
//! above the number of replicas fails with `UnsatisfiableWriteConcern` (100).

use teseladb_raft::{HardState, NodeId, Role, Status};
use teseladb_range::ack::{
    AckWhen, UNSATISFIABLE_WRITE_CONCERN, Unsatisfiable, W, WriteConcern, ack_when, acked,
};

fn status(last_index: u64, persisted: u64, followers: &[u64]) -> Status {
    let mut progress = vec![(NodeId(1), 0)];
    progress.extend(followers.iter().enumerate().map(|(i, m)| (NodeId(i as u64 + 2), *m)));
    Status {
        id: NodeId(1),
        role: Role::Leader,
        leader: NodeId(1),
        hard_state: HardState::default(),
        last_index,
        persisted,
        applied: 0,
        progress,
        rtts: vec![],
    }
}

fn wc(w: W, j: bool) -> WriteConcern {
    WriteConcern { w, j }
}

#[test]
fn each_write_concern_waits_for_its_point() {
    assert_eq!(ack_when(wc(W::Zero, true), 3), Ok(AckWhen::Never));
    assert_eq!(ack_when(wc(W::Nodes(0), false), 3), Ok(AckWhen::Never));
    assert_eq!(ack_when(wc(W::Nodes(1), false), 3), Ok(AckWhen::LeaderLog));
    assert_eq!(ack_when(wc(W::Nodes(1), true), 3), Ok(AckWhen::LeaderDisk));
    for j in [false, true] {
        assert_eq!(ack_when(wc(W::Nodes(2), j), 3), Ok(AckWhen::Replicas(2)));
        assert_eq!(ack_when(wc(W::Nodes(3), j), 3), Ok(AckWhen::Replicas(3)));
        assert_eq!(ack_when(wc(W::Majority, j), 3), Ok(AckWhen::Applied));
    }
    assert_eq!(ack_when(wc(W::Nodes(4), true), 3), Err(Unsatisfiable));
    assert_eq!(UNSATISFIABLE_WRITE_CONCERN, 100);
}

#[test]
fn a_write_is_acknowledged_once_its_point_is_reached() {
    // Entry 5: in the leader's log, not yet on its disk nor on any follower.
    let st = status(5, 4, &[3, 4]);
    assert!(acked(AckWhen::LeaderLog, 5, &st, false));
    assert!(!acked(AckWhen::LeaderDisk, 5, &st, false));
    assert!(!acked(AckWhen::Replicas(2), 5, &st, false));
    // On the leader's disk and one follower's.
    let st = status(5, 5, &[5, 4]);
    assert!(acked(AckWhen::LeaderDisk, 5, &st, false));
    assert!(acked(AckWhen::Replicas(2), 5, &st, false));
    assert!(!acked(AckWhen::Replicas(3), 5, &st, false));
    assert!(!acked(AckWhen::Applied, 5, &st, false));
    assert!(acked(AckWhen::Applied, 5, &st, true));
    // `w: 0` never answers.
    assert!(!acked(AckWhen::Never, 5, &status(9, 9, &[9, 9]), true));
    // An entry the leader no longer has (another leader replaced it) is not acknowledged.
    assert!(!acked(AckWhen::LeaderLog, 6, &st, false));
}
