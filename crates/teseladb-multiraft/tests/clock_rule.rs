//! The clock rule of a core (005 FR-006): the marks of reads and proposals come from a wall clock
//! the rule checked, a node out of margin assigns none and hands its leaderships over, and a node
//! that never heard another clock assigns none in a group with other voters.

mod common;

use common::*;
use teseladb_multiraft::{CoreConfig, Done, NodeClock, OpError, PeerFrame, ReadMode};
use teseladb_raft::{MessageType, Role};

/// A frame from a peer whose HLC says nothing yet.
const NO_HLC: teseladb_hlc::Hlc = teseladb_hlc::Hlc { physical: 0, logical: 0 };

/// Offset Group 0 reports for a node far from the voters' median.
const OUT_OF_MARGIN_MS: i64 = 300;

#[test]
fn a_read_mark_comes_from_the_checked_wall_clock_and_is_withheld_out_of_margin() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.ticks(5);
    let now = c.now;
    let wall = WALL0 + now.as_millis();
    let core = &mut c.nodes.get_mut(&l).unwrap().core;
    let first = core.read_mark(now).expect("a mark in margin");
    assert!(first.physical >= wall, "{first:?} below the wall clock {wall}");
    let second = core.read_mark(now).expect("a mark in margin");
    assert!(second > first, "each mark is above the previous one");
    // Group 0 reports the node out of margin: no mark until it is back.
    core.on_clock_report(OUT_OF_MARGIN_MS, now);
    assert_eq!(core.read_mark(now), None);
    core.on_clock_report(0, now);
    assert!(core.read_mark(now).is_some_and(|m| m > second));
}

#[test]
fn a_read_mark_at_the_bound_on_disk_is_withheld() {
    // The bound in `node.meta` is the first tick's wall clock: every mark would reach it.
    let bounded = |i| CoreConfig { clock: NodeClock::with_bound(id(i), WALL0), ..cfg(i) };
    let mut c = Cluster::with(1, bounded);
    let l = c.elect();
    let now = c.now;
    assert_eq!(c.nodes[&l].core.read_mark(now), None);
    assert_eq!(c.propose(l, b"x"), Err(OpError::Overloaded));
}

#[test]
fn configurations_are_equal_only_when_they_share_the_node_clock() {
    let a = cfg(1);
    let shared = CoreConfig { core: a.core, ..a.clone() };
    assert_eq!(a, shared, "a clone shares the clock");
    let own_clock = CoreConfig { clock: NodeClock::for_node(id(1)), ..a.clone() };
    assert_ne!(a, own_clock, "the same settings over another clock are another node's");
}

#[test]
fn a_leader_that_never_heard_another_clock_assigns_no_mark() {
    // The cores tick with a wall clock of 0 (not known yet): their frames say nothing of it, so
    // the leader elected this way has heard no other clock.
    let mut c = Cluster::new(3);
    let mut leader = None;
    for _ in 0..1_000 {
        c.now = c.now + TICK;
        let now = c.now;
        for node in c.nodes.values_mut() {
            node.core.on_tick(now, 0);
        }
        c.settle();
        leader = c.leader().filter(|&l| {
            let s = c.nodes[&l].core.status(G).unwrap();
            s.applied >= s.hard_state.commit && s.hard_state.commit > 0
        });
        if leader.is_some() {
            break;
        }
    }
    let l = leader.expect("a leader");
    assert_eq!(c.propose(l, b"x"), Err(OpError::Overloaded), "no mark before hearing a clock");
    // Real wall clocks: the first one is a jump from 0, out of margin for a second; then the
    // peers' heartbeats are heard and a leader assigns marks again.
    c.ticks(150);
    let l = c.leader().expect("a leader");
    let op = c.propose(l, b"x").unwrap();
    c.ticks(5);
    assert_eq!(c.results(l, op), vec![Ok(Done::Applied(1))]);
}

#[test]
fn a_single_voter_out_of_margin_keeps_leading_but_assigns_no_mark() {
    let mut c = Cluster::new(1);
    let l = c.elect();
    let now = c.now;
    c.nodes.get_mut(&l).unwrap().core.on_clock_report(OUT_OF_MARGIN_MS, now);
    c.settle();
    // No other voter can take the leadership.
    assert_eq!(c.nodes[&l].core.status(G).unwrap().role, Role::Leader);
    assert_eq!(c.propose(l, b"x"), Err(OpError::Overloaded));
    // The report counts for a window: once it expires, marks are assigned again.
    c.ticks(150);
    let op = c.propose(l, b"x").unwrap();
    c.ticks(2);
    assert_eq!(c.results(l, op), vec![Ok(Done::Applied(1))]);
}

#[test]
fn a_stopped_group_hands_nothing_over_when_its_clock_leaves_margin() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.propose(l, b"poison").unwrap();
    c.ticks(3);
    assert!(c.nodes[&l].core.has_stopped_group());
    c.sent.clear();
    let now = c.now;
    c.nodes.get_mut(&l).unwrap().core.on_clock_report(OUT_OF_MARGIN_MS, now);
    c.settle();
    let timeout_now = c.sent.iter().any(|(from, _, f)| {
        *from == l
            && matches!(f, teseladb_multiraft::PeerFrame::Raft { msgs, .. }
                if msgs.iter().any(|(_, m)| m.ty == MessageType::TimeoutNow))
    });
    assert!(!timeout_now, "a stopped group sends no transfer");
    assert_eq!(c.nodes[&l].core.status(G).unwrap().role, Role::Leader);
}

#[test]
fn a_lease_read_out_of_margin_goes_through_read_index() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.ticks(10);
    let now = c.now;
    let core = &mut c.nodes.get_mut(&l).unwrap().core;
    // In margin, a lease read ends at once.
    let lease = core.read(G, ReadMode::Lease, now).unwrap();
    let a = core.drain_actions();
    assert!(a.done.iter().any(|(op, r)| *op == lease && matches!(r, Ok(Done::Read { .. }))));
    // Out of margin the lease is not used: the read waits for a heartbeat round or ends with
    // the leadership it hands over.
    core.on_clock_report(OUT_OF_MARGIN_MS, now);
    let read = core.read(G, ReadMode::Lease, now).unwrap();
    let a = core.drain_actions();
    assert!(a.done.iter().all(|(op, _)| *op != read), "not served from the lease");
    for (to, f) in a.send {
        c.queue.push_back((l, to, f));
    }
    c.settle();
    c.ticks(5);
    let r = c.results(l, read);
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(matches!(r[0], Ok(Done::Read { .. }) | Err(OpError::NotLeader { .. })), "{r:?}");
}

#[test]
fn a_proposal_beyond_the_uncommitted_budget_is_overloaded() {
    // One byte of budget: a second proposal while the first is uncommitted is refused by Raft.
    let tight = |i| {
        let mut c = cfg(i);
        c.raft.max_uncommitted_bytes = 1;
        c
    };
    let mut c = Cluster::with(3, tight);
    let l = c.elect();
    c.isolate(l);
    let first = c.propose(l, b"a").unwrap();
    assert_eq!(c.propose(l, b"b"), Err(OpError::Overloaded));
    // Once the first one commits, the budget is free again.
    c.heal();
    c.ticks(5);
    assert_eq!(c.results(l, first), vec![Ok(Done::Applied(1))]);
    let second = c.propose(l, b"b").unwrap();
    c.ticks(5);
    assert_eq!(c.results(l, second), vec![Ok(Done::Applied(2))]);
}

#[test]
fn only_the_clocks_of_voters_count_for_the_local_rule() {
    // 005 FR-006 (audit A005-5): the median is the voters'; a node that votes in none of this
    // core's groups (a learner, a node being added) says nothing of this node's clock.
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.ticks(5);
    let now = c.now;
    let wall = WALL0 + now.as_millis();
    let core = &mut c.nodes.get_mut(&l).unwrap().core;
    let before = core.clock_offset(now);
    // Three of them, more than the voters: counted, they would make the median.
    for n in 9..12 {
        let frame = teseladb_multiraft::PeerFrame::Heartbeats {
            wall_ms: wall + 10_000,
            hlc: NO_HLC,
            beats: vec![],
        };
        core.on_peer_message(id(n), frame, now);
    }
    assert_eq!(core.clock_offset(now), before, "a non-voter's clock is not a sample");
    assert!(core.read_mark(now).is_some(), "the leader stays in margin");
}

#[test]
fn a_frames_hlc_moves_the_receivers_clock_past_it() {
    // 005 FR-003: a leader elected after another learns, through the voters that answered both,
    // the marks the other may have read at, so its read cache starts above them.
    let clocks: Vec<NodeClock> = (1..=3).map(|i| NodeClock::for_node(id(i))).collect();
    let mut c =
        Cluster::with(3, |i| CoreConfig { clock: clocks[i as usize - 1].clone(), ..cfg(i) });
    let l = c.elect();
    c.ticks(5);
    let now = c.now;
    let wall = WALL0 + now.as_millis();
    // A follower's frame reaches the leader, whose heartbeats then carry the clock on.
    let f = c.others(l)[0];
    let clock = &clocks[l.0 as usize - 1].0;
    let ahead = teseladb_hlc::Hlc { physical: wall + 700, logical: 9 };
    let core = &mut c.nodes.get_mut(&l).unwrap().core;
    let frame = PeerFrame::Raft { wall_ms: wall, hlc: ahead, msgs: vec![] };
    core.on_peer_message(f, frame, now);
    let next = |at: u64| match clock.now(at) {
        teseladb_hlc::Next::Mark(m) => m,
        other => panic!("{other:?}"),
    };
    assert!(next(wall) > ahead, "the next mark is above the frame's HLC");
    // A frame's HLC never moves the clock back.
    core.on_peer_message(
        f,
        PeerFrame::Heartbeats { wall_ms: wall, hlc: NO_HLC, beats: vec![] },
        now,
    );
    assert!(next(wall) > ahead);
    // The frames the node sends carry its clock.
    let mut sent = Vec::new();
    for t in 1..=10 {
        core.on_tick(now + teseladb_types::Nanos::from_millis(10 * t), wall + 10 * t);
        sent.extend(core.drain_actions().send);
    }
    assert!(!sent.is_empty());
    for (_, frame) in sent {
        if let PeerFrame::Raft { hlc, .. }
        | PeerFrame::Heartbeats { hlc, .. }
        | PeerFrame::HeartbeatResps { hlc, .. } = frame
        {
            assert!(hlc.physical >= ahead.physical, "{hlc:?}");
        }
    }
}
