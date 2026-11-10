//! A group that went quiet (no request for a while, so its leader stopped its heartbeats) loses
//! its leader to a partition that drops packets without closing the connections (iptables
//! `DROP`, a cable): no connection drops, so nothing reports the leader unreachable. The other
//! two replicas have to notice the silence of the leader's node and elect a new leader (005
//! FR-016, the 002 battery's `rollback-w1` against a real cluster, where none was elected in
//! 90 s).

mod common;

use common::*;
use teseladb_raft::Role;
use teseladb_types::GroupId;

/// Longest a majority may take to elect after the cut: a few election timeouts.
const ELECT_WITHIN_TICKS: u32 = 300;

#[test]
fn a_quiet_group_elects_a_new_leader_when_its_leader_is_cut_off_silently() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.propose(l, b"a").unwrap();
    // Idle well past `quiesce_after_ticks`: the group is quiescent on every replica.
    c.ticks(400);
    assert!(c.nodes.values().all(|n| n.core.is_quiesced(G)), "quiescent");
    c.isolate(l);
    let mut elected = None;
    for _ in 0..ELECT_WITHIN_TICKS {
        c.tick();
        elected = c
            .others(l)
            .into_iter()
            .find(|n| c.nodes[n].core.status(G).is_some_and(|s| s.role == Role::Leader));
        if elected.is_some() {
            break;
        }
    }
    let n = elected.expect("the majority elects a leader");
    c.propose(n, b"b").unwrap();
    c.ticks(5);
    assert!(c.sm(n).data.ends_with(b"b"));
    // Healed, the old leader follows the new one.
    c.heal();
    c.ticks(100);
    assert!(c.sm(l).data.ends_with(b"b"));
}

/// A quiet follower that stops hearing another follower stays quiet: only its leader's silence
/// wakes it.
#[test]
fn a_silent_follower_does_not_wake_the_other_quiet_follower() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.propose(l, b"a").unwrap();
    c.ticks(400);
    assert!(c.nodes.values().all(|n| n.core.is_quiesced(G)), "quiescent");
    let others = c.others(l);
    let (f, g) = (others[0], others[1]);
    c.isolate(g);
    c.ticks(ELECT_WITHIN_TICKS);
    assert!(c.nodes[&f].core.is_quiesced(G), "its leader still answers");
    assert_eq!(c.nodes[&f].core.status(G).unwrap().leader, l);
}

/// The round the authorization fence asks for (013 FR-012, D-073): a quiet leader wakes up to send
/// it and reads back when it left once a majority answered; cut off, it never gets there.
#[test]
fn a_confirm_round_wakes_a_quiet_leader_and_needs_a_majority() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.propose(l, b"a").unwrap();
    c.ticks(400);
    assert!(c.nodes[&l].core.is_quiesced(G));
    let f = c.others(l)[0];
    assert_eq!(c.nodes[&f].core.quorum_heard_at(G), None, "a follower");
    assert_eq!(c.nodes[&l].core.quorum_heard_at(GroupId(999)), None, "no such group");
    let asked = c.now;
    let now = c.now;
    c.nodes.get_mut(&l).unwrap().core.send_confirm_round(G, now);
    c.nodes.get_mut(&f).unwrap().core.send_confirm_round(GroupId(999), now);
    assert!(!c.nodes[&l].core.is_quiesced(G));
    c.ticks(3);
    assert!(c.nodes[&l].core.quorum_heard_at(G).is_some_and(|h| h >= asked));
    c.isolate(l);
    c.ticks(1);
    let cut = c.now;
    let now = c.now;
    c.nodes.get_mut(&l).unwrap().core.send_confirm_round(G, now);
    c.ticks(50);
    assert!(c.nodes[&l].core.quorum_heard_at(G).is_some_and(|h| h < cut));
}

/// A quiet leader holds no lease, so the reads served on other threads with it (005 FR-028)
/// would go back to the core: `wake_leader` wakes it and, alone in its group, it reads with its
/// lease again at once; a follower or an awake leader is left as it is.
#[test]
fn a_quiet_single_leader_wakes_and_reads_with_its_lease_again() {
    let mut c = Cluster::new(1);
    let l = c.elect();
    c.propose(l, b"a").unwrap();
    c.ticks(400);
    let now = c.now;
    assert!(c.nodes[&l].core.is_quiesced(G));
    assert!(!c.nodes[&l].core.lease_readable(G, now), "no lease while quiet");
    assert!(c.nodes.get_mut(&l).unwrap().core.wake_leader(G));
    assert!(!c.nodes[&l].core.is_quiesced(G));
    assert!(c.nodes[&l].core.lease_readable(G, now), "its lease again");
    assert!(!c.nodes.get_mut(&l).unwrap().core.wake_leader(G), "already awake");
    assert!(!c.nodes.get_mut(&l).unwrap().core.wake_leader(GroupId(999)), "no such group");
}

/// Three replicas: a follower is not woken by it.
#[test]
fn wake_leader_leaves_a_quiet_follower_alone() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.propose(l, b"a").unwrap();
    c.ticks(400);
    let f = c.others(l)[0];
    assert!(!c.nodes.get_mut(&f).unwrap().core.wake_leader(G));
    assert!(c.nodes[&f].core.is_quiesced(G));
}
