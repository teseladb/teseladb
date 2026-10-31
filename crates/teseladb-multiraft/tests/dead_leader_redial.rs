//! A leader killed with `SIGKILL` leaves its followers redialing it: every failed dial reports
//! the node unreachable again. A follower that is not quiescent already counts down to its
//! election, and those reports must not start its count again, or the group elects only once
//! the redial backoff leaves a gap longer than the election timeout (005 SC-004: the replacement
//! took up to 1.5 s, past the 1 s the criterion allows).

mod common;

use common::*;
use teseladb_raft::{RaftConfig, Role};

/// Ticks between two failed dials: the follower redials the dead leader more often than its
/// election timeout.
const REDIAL_EVERY_TICKS: u32 = 10;

#[test]
fn failed_redials_to_a_dead_leader_do_not_hold_back_the_election() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.propose(l, b"a").unwrap();
    c.ticks(5);
    assert!(c.nodes.values().all(|n| !n.core.is_quiesced(G)), "the group is busy");
    c.isolate(l);
    // The election timeout runs out within its maximum, plus a round for the votes.
    let within = RaftConfig::new(id(1)).election_ticks_max + 5;
    let mut elected = None;
    for t in 0..within {
        if t % REDIAL_EVERY_TICKS == 0 {
            for f in c.others(l) {
                c.nodes.get_mut(&f).unwrap().core.report_unreachable(l);
            }
        }
        c.tick();
        elected = c
            .others(l)
            .into_iter()
            .find(|n| c.nodes[n].core.status(G).is_some_and(|s| s.role == Role::Leader));
        if elected.is_some() {
            break;
        }
    }
    let n = elected.expect("a follower elects itself within the election timeout");
    c.propose(n, b"b").unwrap();
    c.ticks(5);
    assert!(c.sm(n).data.ends_with(b"b"));
}

/// A dropped connection wakes only the quiet followers of the node that dropped: a quiet leader
/// stays quiet, and so does a follower whose leader is another node.
#[test]
fn a_dropped_connection_wakes_only_the_quiet_followers_of_that_node() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.propose(l, b"a").unwrap();
    // Idle past `quiesce_after_ticks`: quiet everywhere.
    c.ticks(400);
    assert!(c.nodes.values().all(|n| n.core.is_quiesced(G)), "quiescent");
    let others = c.others(l);
    let (f, g) = (others[0], others[1]);
    c.nodes.get_mut(&l).unwrap().core.report_unreachable(f);
    c.nodes.get_mut(&f).unwrap().core.report_unreachable(g);
    assert!(c.nodes[&l].core.is_quiesced(G), "the leader stays quiet");
    assert!(c.nodes[&f].core.is_quiesced(G), "its leader is another node");
    c.nodes.get_mut(&f).unwrap().core.report_unreachable(l);
    assert!(!c.nodes[&f].core.is_quiesced(G), "the follower of the dropped node wakes");
}
