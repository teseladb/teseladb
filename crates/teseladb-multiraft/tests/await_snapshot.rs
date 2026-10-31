//! A replica restarted without the data of its own snapshot (007 T016): it keeps voting and
//! appending, so that its group elects and commits without it having the data; it applies nothing
//! until it has it, from its own snapshot or from the leader's, which it asks for once the layer
//! above learns that no replica will serve its own.

mod common;

use common::*;
use teseladb_multiraft::{CoreConfig, Done, GroupDesc};
use teseladb_types::NodeId;

fn small(i: u64) -> CoreConfig {
    CoreConfig { snapshot_entries: 3, ..cfg(i) }
}

/// Every node applied and compacted several entries; then follower `f` restarts without the
/// data of its snapshot, the leader `l` restarts too and the third node is lost: `l` can only lead
/// with `f`'s vote.
fn restarted_without_data() -> (Cluster, NodeId, NodeId) {
    let mut c = Cluster::with(3, small);
    let l = c.elect();
    for i in 0..8u8 {
        c.propose(l, &[b'a' + i]).unwrap();
        c.ticks(1);
    }
    c.ticks(5);
    let (f, lost) = (c.others(l)[0], c.others(l)[1]);
    assert!(c.nodes[&f].core.store().group(G).unwrap().snapshot_index > 0, "compacted");
    c.isolate(lost);
    let recovered = || GroupDesc { group: G, voters: vec![], learners: vec![] };
    c.restart(f);
    // Its state machine claims entries it does not hold: only what Raft hands out is applied.
    let without = Appender { applied: 1_000, ..Appender::default() };
    without.missing_data.set(true);
    c.nodes.get_mut(&f).unwrap().core.create_group(recovered(), without).unwrap();
    c.restart(l);
    c.nodes.get_mut(&l).unwrap().core.create_group(recovered(), Appender::default()).unwrap();
    (c, l, f)
}

/// Ticks until `l` leads with its term's entry applied.
fn wait_for_leader(c: &mut Cluster, l: NodeId) {
    for _ in 0..1_000 {
        c.tick();
        if c.leader() == Some(l) {
            let s = c.nodes[&l].core.status(G).unwrap();
            if s.applied >= s.hard_state.commit {
                return;
            }
        }
    }
    panic!("no leader: the replica waiting for its snapshot's data does not vote");
}

fn write(c: &mut Cluster, l: NodeId, payload: &[u8]) {
    let op = c.propose(l, payload).unwrap();
    c.ticks(5);
    assert!(matches!(c.results(l, op)[..], [Ok(Done::Applied { .. })]), "committed with f");
}

#[test]
fn a_replica_restarted_without_its_snapshots_data_votes_and_takes_the_leaders_snapshot() {
    let (mut c, l, f) = restarted_without_data();
    wait_for_leader(&mut c, l);
    assert!(c.nodes[&f].core.awaited_snapshot(G).is_some(), "it still waits for its data");
    write(&mut c, l, b"while f waits");
    assert_eq!(c.sm(f).entries, 0, "nothing applied without the data");
    assert_ne!(c.leader(), Some(f));
    // The leader takes a snapshot past the follower's, below the follower's commit index.
    for i in 0..3u8 {
        write(&mut c, l, &[b'k' + i]);
    }
    assert!(c.nodes.get_mut(&l).unwrap().core.snapshot_now(G));
    c.ticks(2);
    let own = c.nodes[&f].core.snapshot_index(G).unwrap();
    let leaders = c.nodes[&l].core.snapshot_index(G).unwrap();
    assert!(leaders > own);
    assert!(leaders <= c.nodes[&f].core.status(G).unwrap().hard_state.commit);
    // No replica will serve the data of its own snapshot: it asks the leader for its snapshot,
    // whose data arrives after it parks it.
    c.nodes.get_mut(&f).unwrap().core.reject_awaited(G);
    c.ticks(5);
    assert_eq!(c.nodes[&f].core.snapshot_index(G), Some(own), "parked until its data is here");
    assert!(c.nodes[&f].core.awaited_snapshot(G).is_some());
    c.sm(f).missing_data.set(false);
    let now = c.now;
    c.nodes.get_mut(&f).unwrap().core.snapshot_data_ready(G, now);
    c.ticks(5);
    assert_eq!(c.nodes[&f].core.awaited_snapshot(G), None);
    assert_eq!(c.nodes[&f].core.snapshot_index(G), Some(leaders));
    // Right away the leader replicates to it again.
    write(&mut c, l, b"after");
    assert_eq!(c.sm(f).data, c.sm(l).data);
}

#[test]
fn a_replica_whose_own_snapshots_data_comes_back_applies_what_it_held_back() {
    let (mut c, l, f) = restarted_without_data();
    wait_for_leader(&mut c, l);
    write(&mut c, l, b"while f waits");
    assert_eq!(c.sm(f).entries, 0);
    c.sm(f).missing_data.set(false);
    let now = c.now;
    c.nodes.get_mut(&f).unwrap().core.snapshot_data_ready(G, now);
    c.ticks(5);
    assert_eq!(c.nodes[&f].core.awaited_snapshot(G), None);
    assert_eq!(c.sm(f).data, c.sm(l).data);
    // It can lead again.
    let st = c.nodes[&f].core.status(G).unwrap();
    assert_eq!(st.applied, st.hard_state.commit);
}

#[test]
fn a_replica_waiting_for_its_snapshots_data_takes_no_snapshot_of_its_own() {
    let (mut c, l, f) = restarted_without_data();
    wait_for_leader(&mut c, l);
    let before = c.nodes[&f].core.snapshot_index(G);
    assert!(!c.nodes.get_mut(&f).unwrap().core.snapshot_now(G));
    assert_eq!(c.nodes[&f].core.snapshot_index(G), before);
}

#[test]
fn a_replica_told_its_snapshot_is_stale_before_any_leader_asks_the_one_elected() {
    let (mut c, l, f) = restarted_without_data();
    // Its own snapshot waits for its data, and no node leads yet: the nodes it asks do nothing
    // with the request.
    while c.nodes[&f].core.awaited_snapshot(G).is_none() {
        c.tick();
    }
    assert_eq!(c.leader(), None);
    c.nodes.get_mut(&f).unwrap().core.reject_awaited(G);
    c.tick();
    wait_for_leader(&mut c, l);
    for i in 0..3u8 {
        write(&mut c, l, &[b'k' + i]);
    }
    // The leader's entries bring the request again, and the leader's newer snapshot comes once it
    // is saved: the one the follower waits for changes.
    let own = c.nodes[&f].core.awaited_snapshot(G).map(<[u8]>::to_vec);
    assert!(c.nodes.get_mut(&l).unwrap().core.snapshot_now(G));
    let leaders = c.nodes[&l].core.snapshot_index(G);
    for i in 0..20u8 {
        if c.nodes[&f].core.awaited_snapshot(G).map(<[u8]>::to_vec) != own {
            break;
        }
        write(&mut c, l, &[b'n', i]);
        c.ticks(10);
    }
    assert_ne!(c.nodes[&f].core.awaited_snapshot(G).map(<[u8]>::to_vec), own);
    c.sm(f).missing_data.set(false);
    let now = c.now;
    c.nodes.get_mut(&f).unwrap().core.snapshot_data_ready(G, now);
    c.ticks(10);
    assert_eq!(c.nodes[&f].core.snapshot_index(G), leaders);
    write(&mut c, l, b"after");
    assert_eq!(c.sm(f).data, c.sm(l).data);
}

fn is_chunk(_: NodeId, _: NodeId, f: &teseladb_multiraft::PeerFrame) -> bool {
    matches!(f, teseladb_multiraft::PeerFrame::SnapshotChunk(_))
}

#[test]
fn a_transfer_lost_while_the_replica_asks_again_goes_again_after_its_timeout() {
    let (mut c, l, f) = restarted_without_data();
    wait_for_leader(&mut c, l);
    for i in 0..3u8 {
        write(&mut c, l, &[b'k' + i]);
    }
    assert!(c.nodes.get_mut(&l).unwrap().core.snapshot_now(G));
    c.ticks(2);
    let own = c.nodes[&f].core.awaited_snapshot(G).map(<[u8]>::to_vec);
    // The leader's snapshot is lost on its way (the replica restarted meanwhile), and the replica
    // asks for it again while the leader still counts it as in flight: that ask ends nothing, and
    // the transfer goes again once its timeout passes (007 T016, splits heavy seed 1827).
    c.drop = Some(is_chunk);
    c.nodes.get_mut(&f).unwrap().core.reject_awaited(G);
    c.ticks(2);
    c.nodes.get_mut(&f).unwrap().core.reject_awaited(G);
    c.ticks(2);
    c.drop = None;
    for _ in 0..1_500 {
        if c.nodes[&f].core.awaited_snapshot(G).map(<[u8]>::to_vec) != own {
            break;
        }
        c.tick();
    }
    assert_ne!(c.nodes[&f].core.awaited_snapshot(G).map(<[u8]>::to_vec), own, "never sent again");
}
