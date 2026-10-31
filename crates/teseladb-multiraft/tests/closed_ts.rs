//! Closed marks in the heartbeats (007 FR-012): the leader of a group closes a mark and its
//! followers hear it, with the leader's last index then, in the heartbeats of its term.

mod common;

use common::*;
use teseladb_hlc::Hlc;
use teseladb_types::GroupId;

/// Ticks between two heartbeats of a leader (`RaftConfig::heartbeat_ticks`), and one more.
const HEARTBEAT: u32 = 6;

fn mark(physical: u64) -> Hlc {
    Hlc { physical, logical: 0 }
}

#[test]
fn followers_hear_the_leaders_closed_mark_with_its_last_index() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    for p in [b"a", b"b"] {
        c.propose(l, p).unwrap();
    }
    // A leader closes only within its lease, which a round of heartbeats gives it.
    c.ticks(HEARTBEAT);
    let f = c.others(l)[0];
    assert!(!c.nodes.get_mut(&f).unwrap().core.set_closed(G, mark(5)), "a follower closes nothing");
    assert!(!c.nodes.get_mut(&l).unwrap().core.set_closed(GroupId(77), mark(5)));
    let last = c.nodes[&l].core.status(G).unwrap().last_index;
    assert!(c.nodes.get_mut(&l).unwrap().core.set_closed(G, mark(10)));
    // Entries proposed after the mark closed do not move its index.
    c.propose(l, b"c").unwrap();
    c.ticks(HEARTBEAT);
    for n in c.others(l) {
        assert_eq!(c.nodes[&n].core.closed(G), Some((mark(10), last)), "node {n:?}");
    }
    assert_eq!(c.nodes[&l].core.closed(G), None, "the leader hears no heartbeat of its own");
    // A newer mark replaces it.
    let now = c.nodes[&l].core.status(G).unwrap().last_index;
    assert!(c.nodes.get_mut(&l).unwrap().core.set_closed(G, mark(20)));
    c.ticks(HEARTBEAT);
    assert_eq!(c.nodes[&f].core.closed(G), Some((mark(20), now)));
}

#[test]
fn a_closed_mark_holds_after_the_leader_changes() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.propose(l, b"a").unwrap();
    c.ticks(HEARTBEAT);
    let last = c.nodes[&l].core.status(G).unwrap().last_index;
    assert!(c.nodes.get_mut(&l).unwrap().core.set_closed(G, mark(30)));
    c.ticks(HEARTBEAT);
    let to = c.others(l)[0];
    assert!(c.nodes.get_mut(&l).unwrap().core.transfer_leader(G, to));
    c.settle();
    c.ticks(HEARTBEAT);
    assert_eq!(c.leader(), Some(to));
    c.ticks(HEARTBEAT);
    // The new leader closed nothing yet: its heartbeats carry no mark, and what the followers
    // heard stays (no later leader writes at or below it either); an older mark replaces nothing.
    let other = c.others(to).into_iter().find(|n| *n != l).unwrap();
    assert_eq!(c.nodes[&other].core.closed(G), Some((mark(30), last)));
    assert!(c.nodes.get_mut(&to).unwrap().core.set_closed(G, mark(25)));
    c.ticks(HEARTBEAT);
    assert_eq!(c.nodes[&other].core.closed(G), Some((mark(30), last)));
}

#[test]
fn a_leader_without_its_lease_closes_nothing() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.settle();
    // Cut off from its voters, its lease runs out before another node can be elected: from then
    // on a follower may have a new leader that knows nothing of a mark it would close.
    c.isolate(l);
    c.ticks(40);
    assert!(!c.nodes.get_mut(&l).unwrap().core.set_closed(G, mark(10)));
}
