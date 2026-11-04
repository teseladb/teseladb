//! A replica of a group with a read cache raises the node's clock to the cache's minimum as it
//! wins the election, before anything of its term goes out (005 FR-003, FR-005, research R12):
//! its followers hear that clock with the first round of the term, and the cluster time it won
//! at stays for the term only.

mod common;

use common::*;
use teseladb_hlc::Hlc;
use teseladb_multiraft::{CoreConfig, GroupDesc, NodeClock};

const MARGIN: u64 = 250;
/// The lease of the test configuration (`RaftConfig::default`), in milliseconds.
const LEASE_MS: u64 = 110;

/// Three voters of `G` whose state machine keeps a read cache, with their clocks.
fn cluster() -> (Cluster, Vec<NodeClock>) {
    let clocks: Vec<NodeClock> = (1..=3).map(|i| NodeClock::for_node(id(i))).collect();
    let mut c = Cluster::with(0, cfg);
    let voters: Vec<_> = (1..=3).map(id).collect();
    for i in 1..=3u64 {
        let clock = clocks[i as usize - 1].clone();
        c.add_node(i, |i| CoreConfig { clock: clock.clone(), ..cfg(i) });
        let desc = GroupDesc { group: G, voters: voters.clone(), learners: vec![] };
        let sm = Appender { cache_margin: Some(MARGIN), ..Appender::default() };
        c.nodes.get_mut(&id(i)).unwrap().core.create_group(desc, sm).unwrap();
    }
    c.settle();
    (c, clocks)
}

fn clock_of(clocks: &[NodeClock], n: teseladb_types::NodeId) -> Hlc {
    clocks[n.0 as usize - 1].0.peek()
}

#[test]
fn the_leader_raises_its_clock_to_its_read_cache_and_its_followers_hear_it() {
    let (mut c, clocks) = cluster();
    let l = c.elect();
    let lease = c.nodes[&l].core.lease_mark(G).expect("the leader's lease mark");
    let floor = Hlc { physical: lease.physical + MARGIN, logical: 0 };
    assert!(lease.physical >= WALL0, "at least its wall clock when it won");
    assert!(clock_of(&clocks, l) >= floor, "its clock at the cache's minimum");
    for f in c.others(l) {
        assert_eq!(c.nodes[&f].core.lease_mark(G), None, "a follower keeps none");
        assert!(clock_of(&clocks, f) >= floor, "the term's first round carried it");
    }
    // Cut off, it loses the term to another voter: the mark stays with the term it won.
    c.isolate(l);
    let mut next = l;
    for _ in 0..1_000 {
        c.tick();
        if let Some(n) = c.leader().filter(|&n| n != l) {
            next = n;
            break;
        }
    }
    assert_ne!(next, l, "the other two elect a leader");
    let second = c.nodes[&next].core.lease_mark(G).expect("the new leader's mark");
    assert!(
        second.physical + MARGIN > floor.physical,
        "its cache starts above the clock the first leader handed out"
    );
    c.heal();
    c.ticks(5);
    assert_eq!(c.nodes[&l].core.lease_mark(G), None, "the old leader no longer leads that term");
}

#[test]
fn a_group_without_a_read_cache_leaves_the_clock_alone() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    assert_eq!(c.nodes[&l].core.lease_mark(G), None);
    assert_eq!(c.nodes[&l].core.lease_mark(teseladb_types::GroupId(999)), None, "unknown group");
}

/// Elections one after another do not push the clock ahead of the wall clocks: each leader starts
/// its cache a lease above the clock it heard and at least the margin above its wall clock, and a
/// new term comes only after the previous lease ended, so the cluster time stays within the margin
/// plus a lease of the wall time. Raising the cache's minimum a margin above the clock it heard,
/// which already held the previous raise, added 250 ms per election (seed 4086 of `splits`, heavy
/// plan: about 12 s ahead, and restarted nodes waited that long).
#[test]
fn elections_one_after_another_keep_the_clock_within_the_margin_of_the_wall_time() {
    const ELECTIONS: u64 = 12;
    let (mut c, clocks) = cluster();
    let mut l = c.elect();
    for _ in 0..ELECTIONS {
        c.isolate(l);
        let old = l;
        for _ in 0..1_000 {
            c.tick();
            if let Some(n) = c.leader().filter(|&n| n != old) {
                l = n;
                break;
            }
        }
        assert_ne!(l, old, "another voter leads");
        c.heal();
        c.ticks(2);
    }
    let wall = WALL0 + c.now.as_millis();
    let ahead = (1..=3).map(|i| clock_of(&clocks, id(i)).physical).max().unwrap();
    assert!(
        ahead <= wall + MARGIN + LEASE_MS,
        "the clock is {} ms ahead of the wall time after {ELECTIONS} elections",
        ahead - wall
    );
}

/// While it leads, the leader keeps its clock at least at its wall clock, and its frames carry it:
/// its lease reads, served at its wall clock until the lease ends, stay within a lease of the clock
/// its voters hand to the next leader, which starts its cache a lease above that clock. Raised only
/// as it won, the clock fell behind the wall clock during a long term (seed 730 of `range-docs`,
/// heavy plan, with the clocks of a majority ahead together).
#[test]
fn a_leader_keeps_its_clock_at_its_wall_clock_for_its_whole_term_and_its_voters_hear_it() {
    let (mut c, clocks) = cluster();
    let l = c.elect();
    for _ in 0..200 {
        c.tick();
        assert_eq!(c.leader(), Some(l), "one term");
    }
    let wall = WALL0 + c.now.as_millis();
    assert!(clock_of(&clocks, l).physical >= wall, "the leader's clock");
    for f in c.others(l) {
        // The last frame left at most a tick ago.
        let tick = 10;
        assert!(
            clock_of(&clocks, f).physical + tick >= wall,
            "a follower heard {}, {} ms below the leader's wall clock",
            clock_of(&clocks, f).physical,
            wall.saturating_sub(clock_of(&clocks, f).physical)
        );
    }
}

/// A leader cut off from its voters whose wall clock jumps ahead does not carry that clock into
/// the cluster time (005 FR-006): nothing vouches for its clock, so it gives out no mark and its
/// clock stays where the others left it. Seed 313 of `multirange` (plan `heavy` at 200 %): a node
/// whose clock had jumped 6 s ahead kept leading a range while cut off, raised the cluster time
/// to its wall clock each tick and, once healed, handed it to the others; a node that restarted
/// afterwards waited for its wall clock to pass a bound 3 s ahead, past the end of the run, and
/// with a third node lost no write was answered after the heal.
#[test]
fn a_leader_cut_off_with_a_clock_ahead_leaves_the_cluster_time_alone() {
    const AHEAD_MS: u64 = 6_000;
    let (mut c, clocks) = cluster();
    let l = c.elect();
    c.ticks(20);
    c.isolate(l);
    c.skew.insert(l, AHEAD_MS);
    // Longer than the second a jump keeps a clock aside: after it, cut off, the node is its own
    // median.
    c.ticks(300);
    assert!(c.nodes[&l].core.status(G).is_some_and(|s| s.role == teseladb_raft::Role::Leader));
    let wall = WALL0 + c.now.as_millis();
    let ahead = clock_of(&clocks, l);
    assert!(ahead.physical < wall + AHEAD_MS / 2, "its clock went to {ahead:?} at {wall}");
    c.heal();
    c.skew.remove(&l);
    c.ticks(50);
    let wall = WALL0 + c.now.as_millis();
    for n in (1..=3).map(id) {
        let clock = clock_of(&clocks, n);
        assert!(clock.physical < wall + AHEAD_MS / 2, "{n:?} at {clock:?}, the wall at {wall}");
    }
}
