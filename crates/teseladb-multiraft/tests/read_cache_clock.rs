//! A replica of a group with a read cache raises the node's clock to the cache's minimum as it
//! wins the election, before anything of its term goes out (005 FR-003, FR-005, research R12):
//! its followers hear that clock with the first round of the term, and the cluster time it won
//! at stays for the term only.

mod common;

use common::*;
use teseladb_hlc::Hlc;
use teseladb_multiraft::{CoreConfig, Durable, GroupDesc, GroupImport, MultiRaftCore, NodeClock};
use teseladb_raftlog::{CoreStore, Disk, MemDisk, StorageConfig};
use teseladb_types::GroupId;

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

/// Many groups of one node winning their first terms within a few ticks (the halves of several
/// splits by load in a row) leave the clock within the margin plus a lease of the wall time: each
/// cache starts a lease above what the node heard from other nodes, and only above its own clock
/// by a millisecond, as every mark this node gave out is below it. A lease above its own clock,
/// which already held the previous raise, added 111 ms per election: twelve took it 1,5 s ahead,
/// and a reader whose mark ran past the wall clock of a transaction record's leader waited for
/// that clock to pass it (H-013 of the native tests, 11-11-2026).
#[test]
fn many_groups_of_one_node_elected_together_keep_its_clock_near_the_wall_time() {
    const GROUPS: u64 = 12;
    let clock = NodeClock::for_node(id(1));
    let mut c = Cluster::with(0, cfg);
    let mine = clock.clone();
    c.add_node(1, |i| CoreConfig { clock: mine.clone(), ..cfg(i) });
    for g in 0..GROUPS {
        let desc = GroupDesc { group: GroupId(100 + g), voters: vec![id(1)], learners: vec![] };
        let sm = Appender { cache_margin: Some(MARGIN), ..Appender::default() };
        c.nodes.get_mut(&id(1)).unwrap().core.create_group(desc, sm).unwrap();
    }
    c.settle();
    c.ticks(5);
    let marks: Vec<u64> = (0..GROUPS)
        .map(|g| c.nodes[&id(1)].core.lease_mark(GroupId(100 + g)).expect("it leads").physical)
        .collect();
    let (first, last) = (marks.iter().min().unwrap(), marks.iter().max().unwrap());
    assert!(
        last - first < LEASE_MS,
        "the elections stacked {} ms on one another: {marks:?}",
        last - first
    );
    let ahead = clock.0.peek().physical;
    assert!(ahead <= last + MARGIN + 1, "the clock went to {ahead}, past the last cache");
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
/// the cluster time (005 FR-006): nothing vouches for its clock, so it gives out no mark, for a
/// write or a read, and its clock stays where the others left it. Seed 313 of `multirange` (plan `heavy` at 200 %): a node
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
    // Nor does it give a read a mark from that clock: a read's mark goes into the clock too
    // (seeds 1 569 of `multirange` and 1 392 of `splits`, plan `heavy`).
    let now = c.now;
    assert_eq!(c.nodes[&l].core.read_mark(now), None, "nothing vouches for its clock");
    c.heal();
    c.skew.remove(&l);
    c.ticks(50);
    let wall = WALL0 + c.now.as_millis();
    for n in (1..=3).map(id) {
        let clock = clock_of(&clocks, n);
        assert!(clock.physical < wall + AHEAD_MS / 2, "{n:?} at {clock:?}, the wall at {wall}");
    }
}

/// A group of one voter next to `G` on the same core.
const H: GroupId = GroupId(8);
/// Ticks after which the wall clock is past a clock raised the margin above it.
const PAST_THE_MARGIN: u32 = 40;

/// Node 1 alone with its clock, ticked once: the only voter of the groups a test creates on it.
fn alone() -> (Cluster, NodeClock) {
    let clock = NodeClock::for_node(id(1));
    let mut c = Cluster::with(0, cfg);
    let mine = clock.clone();
    c.add_node(1, |i| CoreConfig { clock: mine.clone(), ..cfg(i) });
    c.ticks(1);
    (c, clock)
}

fn cached() -> Appender {
    Appender { cache_margin: Some(MARGIN), ..Appender::default() }
}

/// Group `g` as the log has it, for a core that opens it again.
fn recovered(g: GroupId) -> GroupDesc {
    GroupDesc { group: g, voters: vec![], learners: vec![] }
}

fn core_of(c: &mut Cluster) -> &mut MultiRaftCore<Appender> {
    &mut c.nodes.get_mut(&id(1)).unwrap().core
}

/// Creates group `g` on node 1, its only voter, which leads it at once.
fn lead_alone(c: &mut Cluster, g: GroupId) {
    let desc = GroupDesc { group: g, voters: vec![id(1)], learners: vec![] };
    core_of(c).create_group(desc, cached()).unwrap();
    c.settle();
}

/// The minimum of the read cache of the term node 1 leads `g` in.
fn minimum(c: &Cluster, g: GroupId) -> u64 {
    c.nodes[&id(1)].core.lease_mark(g).expect("alone, it leads").physical + MARGIN
}

fn wall(c: &Cluster) -> u64 {
    WALL0 + c.now.as_millis()
}

fn run_disk(core: &mut MultiRaftCore<Appender>, disk: &mut MemDisk, c: &Cluster) {
    for _ in 0..10_000 {
        let Some(op) = core.poll_io(c.now) else { return };
        let d = disk.execute(op);
        core.on_io_done(d, c.now);
    }
    panic!("the disk never empties");
}

/// Another core of node 1, with the node's clock, that imports `imp` of `G` and opens it: the
/// minimum of the read cache of the term it wins.
fn minimum_on_another_core(c: &Cluster, clock: &NodeClock, imp: GroupImport) -> u64 {
    let mut disk = MemDisk::new();
    let (store, _) = CoreStore::recover(&StorageConfig::default(), 1, &mut disk).unwrap();
    let config = CoreConfig { core: 1, clock: clock.clone(), ..cfg(1) };
    let mut core = MultiRaftCore::new(config, store, 9);
    core.on_tick(c.now, wall(c));
    core.import_group(G, imp).unwrap();
    run_disk(&mut core, &mut disk, c);
    assert_eq!(core.take_durable(), vec![Durable::Imported(G)]);
    core.create_group(recovered(G), cached()).unwrap();
    run_disk(&mut core, &mut disk, c);
    core.lease_mark(G).expect("alone, it leads").physical + MARGIN
}

/// A replica that led the last term of its log wins the next one without the margin over its wall
/// clock (005 FR-005, H-057 of the native tests): every read of its keys was this node's, at or
/// below its clock. With the margin, each range of a node that was opened again took the node's
/// clock 250 ms above its wall clock, and the reads of a transaction at a mark of that clock
/// waited on every other range until the wall clock passed it.
#[test]
fn a_replica_that_led_the_last_term_of_its_log_wins_the_next_at_its_own_clock() {
    let (mut c, clock) = alone();
    lead_alone(&mut c, G);
    assert!(minimum(&c, G) >= wall(&c) + MARGIN, "nothing says who read before its first term");
    c.ticks(PAST_THE_MARGIN);
    core_of(&mut c).close_group(G).unwrap();
    core_of(&mut c).create_group(recovered(G), cached()).unwrap();
    c.settle();
    assert_eq!(c.nodes[&id(1)].core.status(G).unwrap().hard_state.term, 2);
    assert!(
        minimum(&c, G) <= wall(&c) + 1,
        "its cache starts {} ms above its wall clock",
        minimum(&c, G) - wall(&c)
    );
    assert!(clock.0.peek().physical <= wall(&c) + 1, "its clock stays at its wall clock");
}

/// After a restart nothing in memory says the node served the reads, and its clock starts over
/// at its wall clock, which may be behind the marks it read at: it takes the margin (FR-005).
#[test]
fn a_replica_of_a_node_that_restarted_takes_the_margin_again() {
    let (mut c, _clock) = alone();
    lead_alone(&mut c, G);
    c.ticks(PAST_THE_MARGIN);
    c.restart(id(1));
    c.ticks(1);
    core_of(&mut c).create_group(recovered(G), cached()).unwrap();
    c.settle();
    assert_eq!(c.nodes[&id(1)].core.status(G).unwrap().hard_state.term, 2);
    assert!(minimum(&c, G) >= wall(&c) + MARGIN, "the margin over its wall clock");
}

/// A replica handed over to another core of its node takes with it the term it led (FR-005):
/// the reads were this node's whichever core served them, and the core that takes it wins the
/// next term at the node's clock. A hand-over that says nothing of who led takes the margin.
#[test]
fn a_replica_that_changes_core_wins_its_next_term_at_the_clock_of_its_node() {
    let (mut c, clock) = alone();
    lead_alone(&mut c, G);
    c.ticks(PAST_THE_MARGIN);
    let imp = core_of(&mut c).export_group(G).unwrap();
    assert_eq!(imp.led, Some(1), "the term it led goes with it");
    let unknown = GroupImport { led: None, ..imp.clone() };
    let own = minimum_on_another_core(&c, &clock, imp);
    assert!(own <= wall(&c) + 1, "its cache starts {} ms above the wall clock", own - wall(&c));
    let margin = minimum_on_another_core(&c, &clock, unknown);
    assert!(margin >= wall(&c) + MARGIN, "the margin over its wall clock");
}

/// The half a split cut from a range takes its first term without the margin on the node that
/// leads, in the term that proposed the cut, the range it was cut from (FR-005): until the cut
/// the reads of its keys were that leader's. A cut of an earlier term of that range, or of a
/// range this node does not have, says nothing of who read since.
#[test]
fn the_half_of_a_split_takes_its_first_term_at_the_clock_of_the_leader_that_cut_it() {
    const BORN: GroupId = GroupId(20);
    const OF_AN_OLD_CUT: GroupId = GroupId(21);
    const OF_NOBODY: GroupId = GroupId(22);
    let (mut c, _clock) = alone();
    lead_alone(&mut c, G);
    lead_alone(&mut c, H);
    c.ticks(PAST_THE_MARGIN);
    let born = |c: &mut Cluster, g: GroupId, parent: (GroupId, u64)| {
        let desc = GroupDesc { group: g, voters: vec![id(1)], learners: vec![] };
        core_of(c).create_group_cut_from(desc, cached(), parent).unwrap();
        c.settle();
        minimum(c, g)
    };
    // The last entry of `G` is of the term node 1 leads it in.
    let cut = c.nodes[&id(1)].core.status(G).unwrap().hard_state.commit;
    let own = born(&mut c, BORN, (G, cut));
    assert!(own <= wall(&c) + 1, "its cache starts {} ms above the wall clock", own - wall(&c));
    // `H` is opened again and node 1 leads another term of it: its entry is of the one before.
    let old_cut = c.nodes[&id(1)].core.status(H).unwrap().hard_state.commit;
    core_of(&mut c).close_group(H).unwrap();
    core_of(&mut c).create_group(recovered(H), cached()).unwrap();
    c.settle();
    c.ticks(PAST_THE_MARGIN);
    let old = born(&mut c, OF_AN_OLD_CUT, (H, old_cut));
    assert!(old >= wall(&c) + MARGIN, "another leader may have read between the terms");
    let nobody = born(&mut c, OF_NOBODY, (GroupId(999), 1));
    assert!(nobody >= wall(&c) + MARGIN, "this node does not lead the range it was cut from");
}

/// A replica that led wins a later term after another node led one in between: that node read
/// at its own clock, and the margin over the wall clock stands (FR-005). The entries of the term
/// in between are in its log, above the last one of the term it led.
#[test]
fn a_replica_takes_the_margin_again_once_another_node_led_in_between() {
    let (mut c, _clocks) = cluster();
    let first = c.elect();
    let mut leader = first;
    let mut terms_of_others = 0;
    let mut checked = false;
    for _ in 0..50 {
        // The leader is cut off until another voter wins a term.
        c.isolate(leader);
        let old = leader;
        for _ in 0..1_000 {
            c.tick();
            if let Some(n) = c.leader().filter(|&n| n != old) {
                leader = n;
                break;
            }
        }
        assert_ne!(leader, old, "another voter leads");
        if leader == first {
            // In the tick it won: its cache starts the margin above the wall clock it read then.
            let lease = c.nodes[&first].core.lease_mark(G).expect("it leads again");
            assert!(lease.physical >= wall(&c), "the margin over its wall clock");
            checked = true;
            break;
        }
        terms_of_others += 1;
        c.heal();
        c.ticks(5);
    }
    assert!(checked && terms_of_others > 0, "the first leader won again after another led");
}
