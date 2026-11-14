//! Contract of `MultiRaftCore` (`state-machine-api.md` of 001): proposals end once, errors with
//! their hint, timeouts and dropped proposals, lease and ReadIndex reads, chunked snapshots,
//! configuration changes with learners, forwarded requests and recovery.

mod common;

use common::*;
use teseladb_multiraft::{
    CoreConfig, Done, GroupDesc, NodeClock, OpError, OpId, PeerFrame, ProposeAt, ReadMode,
};
use teseladb_raft::{ConfChange, ConfChangeKind, MAX_ENTRY_BYTES};
use teseladb_types::{Bytes, GroupId, Nanos};

#[test]
fn a_proposal_is_applied_everywhere_and_ends_once() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    let op = c.propose(l, b"abc").unwrap();
    c.settle();
    c.ticks(5);
    assert_eq!(c.results(l, op), vec![Ok(Done::Applied(3))]);
    for n in (1..=3).map(id) {
        assert_eq!(c.sm(n).data, b"abc", "{n:?}");
    }
    // Nothing more arrives for it.
    c.ticks(600);
    assert_eq!(c.results(l, op).len(), 1);
}

#[test]
fn a_follower_answers_not_leader_with_the_leader() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    let f = c.others(l)[0];
    assert_eq!(c.propose(f, b"x"), Err(OpError::NotLeader { hint: l }));
    let now = c.now;
    let r = c.nodes.get_mut(&f).unwrap().core.read(G, ReadMode::ReadIndex, now);
    assert_eq!(r, Err(OpError::NotLeader { hint: l }));
    // A group that is not on the core.
    let r = c.nodes.get_mut(&l).unwrap().core.propose(GroupId(99), b"x", now);
    assert_eq!(r, Err(OpError::GroupNotFound));
    let r = c.nodes.get_mut(&l).unwrap().core.read(GroupId(99), ReadMode::Lease, now);
    assert_eq!(r, Err(OpError::GroupNotFound));
}

#[test]
fn a_proposal_without_a_majority_times_out_and_is_no_longer_awaited() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.isolate(l);
    let op = c.propose(l, b"lost").unwrap();
    // The deadline (5 s) passes: one Timeout.
    c.ticks(510);
    assert_eq!(c.results(l, op), vec![Err(OpError::Timeout)]);
    // Healed, whatever happens to its entry, nothing else is said about it.
    c.heal();
    c.ticks(300);
    assert_eq!(c.results(l, op).len(), 1);
}

#[test]
fn a_proposal_overwritten_by_another_leader_is_dropped() {
    let mut c = Cluster::new(3);
    let old = c.elect();
    c.isolate(old);
    let op = c.propose(old, b"old").unwrap();
    // The other two elect a leader and commit their own entry at the same index.
    let rest = c.others(old);
    let new = loop {
        c.tick();
        if let Some(n) = rest.iter().copied().find(|n| {
            c.nodes[n].core.status(G).is_some_and(|s| s.role == teseladb_raft::Role::Leader)
        }) {
            break n;
        }
    };
    let mine = c.propose(new, b"new").unwrap();
    c.ticks(5);
    assert_eq!(c.results(new, mine), vec![Ok(Done::Applied(3))]);
    c.heal();
    c.ticks(100);
    assert_eq!(c.results(old, op), vec![Err(OpError::ProposalDropped)]);
    assert_eq!(c.sm(old).data, b"new");
}

/// A deposed leader took three proposals the majority never saw, and the new leader writes only
/// its first entry, at the index of the first of them. Once that entry is committed none of the
/// three can commit, and all three end then with `ProposalDropped`, also the two at indexes the
/// new leader's log has not reached, which used to wait for their deadline (the `meta2` seeds 247
/// and 798 of the `light` plan: every client waited on such a proposal and nothing was answered
/// in the 4.5 s after the heal).
#[test]
fn proposals_past_a_new_leaders_log_end_when_their_suffix_is_dropped() {
    let mut c = Cluster::new(3);
    let old = c.elect();
    c.isolate(old);
    let ops: Vec<OpId> =
        [&b"a"[..], b"b", b"c"].iter().map(|p| c.propose(old, p).unwrap()).collect();
    c.settle();
    let rest = c.others(old);
    while !rest
        .iter()
        .any(|n| c.nodes[n].core.status(G).is_some_and(|s| s.role == teseladb_raft::Role::Leader))
    {
        c.tick();
    }
    c.heal();
    // Well under the operation timeout (5 s): 50 ticks of 10 ms.
    c.ticks(50);
    for op in ops {
        assert_eq!(c.results(old, op), vec![Err(OpError::ProposalDropped)], "{op:?}");
    }
    assert!(c.sm(old).data.is_empty(), "none of them was applied");
}

/// The same with a snapshot: the new leader commits five entries with a snapshot every three, so
/// the deposed leader catches up with a snapshot that covers some of its six proposals; those
/// under the snapshot and those above it end at once.
#[test]
fn proposals_a_snapshot_contradicts_end_when_it_is_installed() {
    let mut c = Cluster::with(3, |i| CoreConfig { snapshot_entries: 3, ..cfg(i) });
    let old = c.elect();
    c.isolate(old);
    let ops: Vec<OpId> = (0..6u8).map(|i| c.propose(old, &[b'L', i]).unwrap()).collect();
    c.settle();
    let new = loop {
        c.tick();
        if let Some(l) = c.leader().filter(|&l| l != old) {
            break l;
        }
    };
    for i in 0..5u8 {
        c.propose(new, &[b'N', i]).unwrap();
        c.ticks(1);
    }
    let snap = c.nodes[&new].core.snapshot_index(G).unwrap_or(0);
    let first = c.nodes[&old].core.status(G).unwrap().last_index - 5;
    assert!(snap > first, "the snapshot covers some of the proposals");
    c.heal();
    c.ticks(30);
    for op in ops {
        assert_eq!(c.results(old, op), vec![Err(OpError::ProposalDropped)], "{op:?}");
    }
    assert_eq!(c.sm(old).data, c.sm(new).data, "it caught up with the snapshot");
}

#[test]
fn lease_and_read_index_reads_end_after_applying_the_commit_index() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.propose(l, b"v").unwrap();
    c.ticks(10);
    let commit = c.nodes[&l].core.status(G).unwrap().hard_state.commit;
    for mode in [ReadMode::Lease, ReadMode::ReadIndex] {
        let now = c.now;
        let op = c.nodes.get_mut(&l).unwrap().core.read(G, mode, now).unwrap();
        c.settle();
        assert_eq!(c.results(l, op), vec![Ok(Done::Read { index: commit })], "{mode:?}");
    }
    // Without a valid lease (the leader is cut off) a lease read waits for a majority it does not
    // get: it ends with a timeout, never with a stale answer.
    c.isolate(l);
    c.ticks(200);
    let now = c.now;
    if let Ok(op) = c.nodes.get_mut(&l).unwrap().core.read(G, ReadMode::Lease, now) {
        c.ticks(510);
        let r = c.results(l, op);
        assert_eq!(r.len(), 1);
        assert!(r[0].is_err(), "{r:?}");
    }
}

#[test]
fn a_lagging_follower_receives_the_snapshot_in_chunks() {
    let small = |i| CoreConfig { snapshot_entries: 4, ..cfg(i) };
    let mut c = Cluster::with(3, small);
    let l = c.elect();
    let f = c.others(l)[0];
    c.isolate(f);
    // About 3.6 MiB: the snapshot travels in four chunks of at most 1 MiB.
    for i in 0..12u8 {
        c.propose(l, &vec![i; 300 << 10]).unwrap();
        c.ticks(2);
    }
    c.ticks(20);
    c.heal();
    c.sent.clear();
    c.ticks(200);
    let chunks: Vec<_> = c
        .sent
        .iter()
        .filter_map(|(from, to, fr)| match fr {
            PeerFrame::SnapshotChunk(ch) if *from == l && *to == f => Some(ch.data.len()),
            _ => None,
        })
        .collect();
    assert!(chunks.len() >= 4, "{chunks:?}");
    assert!(chunks.iter().all(|&n| n <= 1 << 20));
    assert!(c.sent.iter().any(|(from, to, fr)| *from == f
        && *to == l
        && matches!(fr, PeerFrame::SnapshotAck { ok: true, .. })));
    assert_eq!(c.sm(f), c.sm(l));
    // The follower keeps up after the snapshot.
    c.propose(l, b"after").unwrap();
    c.ticks(10);
    assert_eq!(c.sm(f).data, c.sm(l).data);
    assert!(c.sm(f).data.ends_with(b"after"));
}

#[test]
fn a_learner_catches_up_and_is_promoted() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    for i in 0..5u8 {
        c.propose(l, &[i]).unwrap();
    }
    c.ticks(5);
    // Node 4 starts with the group in the configuration that will include it.
    c.add_node(4, cfg);
    let desc = GroupDesc { group: G, voters: (1..=3).map(id).collect(), learners: vec![id(4)] };
    c.nodes.get_mut(&id(4)).unwrap().core.create_group(desc, Appender::default()).unwrap();
    let now = c.now;
    let add = ConfChange { kind: ConfChangeKind::AddLearner, node: id(4) };
    let op = c.nodes.get_mut(&l).unwrap().core.propose_conf_change(G, add, now).unwrap();
    c.ticks(20);
    let r = c.results(l, op);
    assert!(matches!(&r[..], [Ok(Done::ConfChanged(conf))] if conf.learners == [id(4)]), "{r:?}");
    assert_eq!(c.sm(id(4)).data, c.sm(l).data);
    let now = c.now;
    let promote = ConfChange { kind: ConfChangeKind::PromoteLearner, node: id(4) };
    let op = c.nodes.get_mut(&l).unwrap().core.propose_conf_change(G, promote, now).unwrap();
    c.ticks(20);
    let r = c.results(l, op);
    assert!(matches!(&r[..], [Ok(Done::ConfChanged(conf))] if conf.voters.len() == 4), "{r:?}");
    // Errors of configuration changes come from Raft.
    let now = c.now;
    let again = ConfChange { kind: ConfChangeKind::AddLearner, node: id(4) };
    let r = c.nodes.get_mut(&l).unwrap().core.propose_conf_change(G, again, now);
    assert_eq!(r, Err(OpError::InvalidConfChange));
    let f = c.others(l)[0];
    let r = c.nodes.get_mut(&f).unwrap().core.propose_conf_change(G, again, now);
    assert!(matches!(r, Err(OpError::NotLeader { .. })));
}

#[test]
fn forwarded_requests_are_answered_or_time_out() {
    let mut c = Cluster::new(2);
    let now = c.now;
    let op = c.nodes.get_mut(&id(1)).unwrap().core.forward(id(2), Bytes::from("hello"), now);
    c.settle();
    let f = c.nodes.get_mut(&id(2)).unwrap().forwarded.pop().unwrap();
    assert_eq!((f.from, &f.body[..]), (id(1), &b"hello"[..]));
    assert!(f.deadline_ms > 0);
    c.nodes.get_mut(&id(2)).unwrap().core.answer_forward(f.from, f.req, Bytes::from("world"));
    c.settle();
    assert_eq!(c.results(id(1), op), vec![Ok(Done::Forwarded(Bytes::from("world")))]);
    // An answer nobody waits for (late, or with another number) is dropped.
    c.nodes.get_mut(&id(2)).unwrap().core.answer_forward(id(1), f.req, Bytes::from("again"));
    c.settle();
    assert_eq!(c.results(id(1), op).len(), 1);
    // Without an answer: Timeout.
    let now = c.now;
    let op = c.nodes.get_mut(&id(1)).unwrap().core.forward(id(2), Bytes::from("x"), now);
    c.ticks(510);
    assert_eq!(c.results(id(1), op), vec![Err(OpError::Timeout)]);
}

#[test]
fn forwarded_requests_to_a_node_whose_connection_dropped_end_at_once() {
    let mut c = Cluster::new(3);
    let now = c.now;
    let n1 = c.nodes.get_mut(&id(1)).unwrap();
    let lost = n1.core.forward(id(2), Bytes::from("x"), now);
    let other = n1.core.forward(id(3), Bytes::from("y"), now);
    // The connection to node 2 drops: nothing will answer what was sent on it (005 SC-004: a
    // request to a dead leader does not wait for its deadline).
    n1.core.report_unreachable(id(2));
    c.settle();
    assert_eq!(c.results(id(1), lost), vec![Err(OpError::Unavailable)]);
    assert!(c.results(id(1), other).is_empty(), "node 3 still answers");
    // Until node 2 is heard from again, a new request to it is not sent and ends at once.
    let now = c.now;
    let n1 = c.nodes.get_mut(&id(1)).unwrap();
    let early = n1.core.forward(id(2), Bytes::from("z"), now);
    let a = n1.core.drain_actions();
    assert!(a.send.iter().all(|(to, _)| *to != id(2)));
    assert!(a.done.iter().any(|(op, r)| *op == early && *r == Err(OpError::Unreachable)));
    c.ticks(50);
    let now = c.now;
    let later = c.nodes.get_mut(&id(1)).unwrap().core.forward(id(2), Bytes::from("w"), now);
    c.settle();
    let f = c.nodes.get_mut(&id(2)).unwrap().forwarded.pop().expect("sent again");
    assert_eq!(&f.body[..], b"w");
    c.nodes.get_mut(&id(2)).unwrap().core.answer_forward(f.from, f.req, Bytes::from("ok"));
    c.settle();
    assert_eq!(c.results(id(1), later), vec![Ok(Done::Forwarded(Bytes::from("ok")))]);
}

#[test]
fn request_numbers_start_at_random_on_every_start() {
    let mut a = Cluster::new(1);
    let now = a.now;
    let first = |c: &mut Cluster| {
        let n = c.nodes.get_mut(&id(1)).unwrap();
        n.core.forward(id(9), Bytes::new(), now);
        let a = n.core.drain_actions();
        match &a.send[..] {
            [(_, PeerFrame::Forward { req, .. })] => *req,
            other => panic!("{other:?}"),
        }
    };
    let r1 = first(&mut a);
    a.restart(id(1));
    let r2 = first(&mut a);
    assert_ne!(r1, r2);
    assert!(r1 > 1 << 16 && r2 > 1 << 16, "{r1} {r2}");
}

#[test]
fn a_restarted_node_recovers_its_groups_and_their_state() {
    let small = |i| CoreConfig { snapshot_entries: 3, ..cfg(i) };
    let mut c = Cluster::with(3, small);
    let l = c.elect();
    for i in 0..10u8 {
        c.propose(l, &[b'a' + i]).unwrap();
        c.ticks(1);
    }
    c.ticks(10);
    let f = c.others(l)[0];
    let before = c.sm(f).clone();
    c.restart(f);
    let node = c.nodes.get_mut(&f).unwrap();
    assert_eq!(node.core.recovered_groups(), vec![G]);
    let desc = GroupDesc { group: G, voters: vec![], learners: vec![] };
    node.core.create_group(desc.clone(), Appender::default()).unwrap();
    assert_eq!(node.core.create_group(desc, Appender::default()), Err(OpError::GroupExists));
    c.ticks(20);
    assert_eq!(c.sm(f).data, before.data);
    c.propose(l, b"z").unwrap();
    c.ticks(10);
    assert!(c.sm(f).data.ends_with(b"z"));
}

#[test]
fn a_closed_group_keeps_its_log_and_opens_again_as_after_a_restart() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    for i in 0..5u8 {
        c.propose(l, &[b'a' + i]).unwrap();
        c.ticks(1);
    }
    c.ticks(10);
    // A write on its way to the disk keeps the group open.
    let now = c.now;
    let leader = c.nodes.get_mut(&l).unwrap();
    leader.core.propose(G, b"w", now).unwrap();
    assert_eq!(leader.core.close_group(G), Err(OpError::Busy));
    c.ticks(10);
    let f = c.others(l)[0];
    let before = c.sm(f).clone();
    let node = c.nodes.get_mut(&f).unwrap();
    node.core.close_group(G).unwrap();
    assert_eq!(node.core.close_group(G), Err(OpError::GroupNotFound));
    assert!(node.core.status(G).is_none());
    assert_eq!(node.core.recovered_groups(), vec![G]);
    // The others go on without it; opened again, it replays its log and catches up.
    c.propose(l, b"y").unwrap();
    c.ticks(10);
    let node = c.nodes.get_mut(&f).unwrap();
    let desc = GroupDesc { group: G, voters: vec![], learners: vec![] };
    node.core.create_group(desc, Appender::default()).unwrap();
    c.ticks(20);
    assert!(c.sm(f).data.starts_with(&before.data));
    assert!(c.sm(f).data.ends_with(b"y"));
}

#[test]
fn a_removed_group_ends_its_operations_and_leaves_the_log() {
    let mut c = Cluster::new(1);
    let l = c.elect();
    c.isolate(l);
    let now = c.now;
    let node = c.nodes.get_mut(&l).unwrap();
    // A proposal that waits for the disk.
    let op = node.core.propose(G, b"x", now).unwrap();
    node.core.remove_group(G).unwrap();
    assert_eq!(node.core.remove_group(G), Err(OpError::GroupNotFound));
    assert!(node.core.status(G).is_none());
    c.settle();
    assert_eq!(c.results(l, op), vec![Err(OpError::GroupNotFound)]);
    c.restart(l);
    assert!(c.nodes[&l].core.recovered_groups().is_empty());
}

#[test]
fn an_entry_the_state_machine_cannot_apply_stops_the_group() {
    let mut c = Cluster::new(1);
    let l = c.elect();
    let op = c.propose(l, b"poison").unwrap();
    c.ticks(3);
    assert_eq!(c.results(l, op), vec![Err(OpError::Unavailable)]);
    assert_eq!(c.propose(l, b"x"), Err(OpError::Unavailable));
    let now = c.now;
    let r = c.nodes.get_mut(&l).unwrap().core.read(G, ReadMode::Lease, now);
    assert_eq!(r, Err(OpError::Unavailable));
}

#[test]
fn too_many_pending_proposals_are_refused() {
    let few = |i| CoreConfig { max_pending_proposals: 2, ..cfg(i) };
    let mut c = Cluster::with(3, few);
    let l = c.elect();
    c.isolate(l);
    c.propose(l, b"1").unwrap();
    c.propose(l, b"2").unwrap();
    assert_eq!(c.propose(l, b"3"), Err(OpError::Overloaded));
    // An entry holds up to 64 MiB (005 FR-023), its envelope included.
    let big = vec![0u8; MAX_ENTRY_BYTES + 1];
    assert_eq!(c.propose(l, &big), Err(OpError::TooLarge));
}

#[test]
fn the_cluster_time_of_a_group_never_goes_down() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    for _ in 0..5 {
        c.propose(l, b"t").unwrap();
        c.ticks(1);
    }
    c.ticks(5);
    // The wall clock of every node goes back one minute: the stamps never go back.
    let before = c.sm(l).last_time;
    assert!(before >= WALL0);
    c.now = c.now + Nanos::from_millis(10);
    let wall = before - 60_000;
    for node in c.nodes.values_mut() {
        node.core.on_tick(c.now, wall);
    }
    c.settle();
    // The jump sets every node aside for a window: no marks meanwhile (005 FR-006).
    assert_eq!(c.propose(l, b"t"), Err(OpError::Overloaded));
    // The next ticks bring the wall clock back (another jump); once a window passes, the stamps
    // go on from where they were.
    c.ticks(250);
    let l = c.elect();
    c.propose(l, b"t").unwrap();
    c.ticks(5);
    assert!(c.sm(l).last_time >= before);
}

#[test]
fn a_proposal_at_a_floor_is_built_at_a_mark_above_it() {
    // 005 FR-004 and FR-008: the range's read cache gives the floor and the write is evaluated at
    // its own mark; the entry goes above the floor even when the node's clock is behind, and
    // later entries stay above it.
    let mut c = Cluster::new(3);
    let l = c.elect();
    let floor = teseladb_hlc::Hlc { physical: WALL0 + 5_000, logical: 7 << 10 };
    let now = c.now;
    let mut seen = None;
    let build = |ts| {
        seen = Some(ts);
        Ok::<_, ()>(b"a".to_vec())
    };
    let (_, ts) = c.nodes.get_mut(&l).unwrap().core.propose_at(G, floor, now, build).unwrap();
    assert_eq!(seen, Some(ts), "built at the mark of the entry");
    assert!(ts > floor, "{ts:?}");
    // The clock is behind the floor: the mark is the next one the node can give (FR-025).
    assert_eq!(ts, teseladb_hlc::succ(floor, teseladb_hlc::node_of(ts)));
    c.settle();
    c.ticks(5);
    assert_eq!(c.sm(l).last_ts, Some(ts), "the entry carries the mark it was proposed at");
    assert_eq!(c.sm(l).data, b"a");
    // A payload that cannot be built proposes nothing.
    let refused =
        c.nodes.get_mut(&l).unwrap().core.propose_at(G, floor, now, |_| Err::<Vec<u8>, _>("no"));
    assert_eq!(refused.map(|_| ()), Err(ProposeAt::Build("no")));
    c.propose(l, b"b").unwrap();
    c.settle();
    c.ticks(5);
    assert!(c.sm(l).last_ts.unwrap() > ts);
    assert_eq!(c.sm(l).data, b"ab");
    // A follower refuses before building anything.
    let follower = c.others(l)[0];
    let never = |_| -> Result<Vec<u8>, ()> { panic!("built on a follower") };
    let got = c.nodes.get_mut(&follower).unwrap().core.propose_at(G, floor, now, never);
    assert_eq!(got.map(|_| ()), Err(ProposeAt::Op(OpError::NotLeader { hint: l })));
}

#[test]
fn no_entry_is_proposed_at_or_above_the_bound_on_disk() {
    // 005 FR-003: until the node writes a new bound, the leader answers `Overloaded`.
    let bound = WALL0 + 1_000_000;
    let clocks: Vec<NodeClock> = (1..=3).map(|i| NodeClock::with_bound(id(i), bound)).collect();
    let mut c =
        Cluster::with(3, |i| CoreConfig { clock: clocks[i as usize - 1].clone(), ..cfg(i) });
    let l = c.elect();
    c.propose(l, b"a").unwrap();
    let clock = &clocks[l.0 as usize - 1].0;
    // A mark from a node far ahead takes the leader's clock to its bound.
    clock.update(teseladb_hlc::Hlc { physical: bound, logical: 0 });
    assert_eq!(c.propose(l, b"b"), Err(OpError::Overloaded));
    assert_eq!(clock.renewal(), Some(bound + teseladb_hlc::AHEAD_MS));
    clock.raise_bound(bound + teseladb_hlc::AHEAD_MS);
    c.propose(l, b"b").unwrap();
    c.settle();
    // The followers took the mark: as the server's clock thread does, each writes a bound above
    // it, and only then answers (research R13).
    for clock in &clocks {
        if let Some(b) = clock.0.renewal() {
            clock.0.raise_bound(b);
        }
    }
    c.ticks(5);
    assert_eq!(c.sm(l).data, b"ab");
}

#[test]
fn a_release_floor_holds_back_the_periodic_snapshots() {
    // 004 FR-011, 005: a state machine over the LSM declares what it has not flushed; the core's
    // periodic snapshots stay at or below that floor until it moves.
    let small = |i| CoreConfig { snapshot_entries: 3, ..cfg(i) };
    let mut c = Cluster::with(3, small);
    let l = c.elect();
    for n in c.nodes.values_mut() {
        n.core.set_release_floor(G, teseladb_raftlog::ReleaseProvider::Lsm, Some(2));
    }
    for i in 0..10u8 {
        c.propose(l, &[b'a' + i]).unwrap();
        c.ticks(1);
    }
    c.ticks(10);
    let snap = |c: &Cluster, n| c.nodes[&n].core.store().group(G).unwrap().snapshot_index;
    assert_eq!(c.nodes[&l].core.store().release_floor_of(G), Some(2));
    assert!(snap(&c, l) <= 2, "snapshot at {}", snap(&c, l));
    for n in c.nodes.values_mut() {
        n.core.set_release_floor(G, teseladb_raftlog::ReleaseProvider::Lsm, None);
    }
    for i in 0..4u8 {
        c.propose(l, &[b'k' + i]).unwrap();
        c.ticks(1);
    }
    c.ticks(10);
    assert!(snap(&c, l) > 2, "the snapshots go on once the floor is gone");
    assert_eq!(c.sm(l).data.len(), 14);
}

#[test]
fn a_snapshot_whose_data_is_missing_waits_for_it() {
    // 005 FR-012: a range's snapshot carries its local state; its data arrives by ingest. Until
    // the layer above says it is here, the follower applies nothing and acknowledges nothing.
    let small = |i| CoreConfig { snapshot_entries: 4, ..cfg(i) };
    let mut c = Cluster::with(3, small);
    let l = c.elect();
    let f = c.others(l)[0];
    c.isolate(f);
    for i in 0..12u8 {
        c.propose(l, &[b'a' + i]).unwrap();
        c.ticks(2);
    }
    c.ticks(20);
    c.sm(f).missing_data.set(true);
    let before = c.sm(f).data.clone();
    c.heal();
    c.sent.clear();
    c.ticks(200);
    let asked = c.nodes[&f].snapshot_data.clone();
    assert!(asked.iter().any(|(g, i)| *g == G && *i > 0), "{asked:?}");
    assert_eq!(c.sm(f).data, before, "nothing applied while the data is missing");
    assert!(
        !c.sent.iter().any(
            |(from, _, fr)| *from == f && matches!(fr, PeerFrame::SnapshotAck { ok: true, .. })
        )
    );
    // Still missing: it asks again.
    let now = c.now;
    c.nodes.get_mut(&f).unwrap().core.snapshot_data_ready(G, now);
    c.ticks(5);
    assert!(c.nodes[&f].snapshot_data.len() > asked.len());
    // The data is here: the snapshot is installed and the follower catches up.
    c.sm(f).missing_data.set(false);
    let now = c.now;
    c.nodes.get_mut(&f).unwrap().core.snapshot_data_ready(G, now);
    c.nodes.get_mut(&f).unwrap().core.snapshot_data_ready(G, now);
    c.ticks(50);
    assert_eq!(c.sm(f).data, c.sm(l).data);
    c.propose(l, b"z").unwrap();
    c.ticks(10);
    assert!(c.sm(f).data.ends_with(b"z"));
}

#[test]
fn a_restarted_replica_whose_data_is_missing_waits_for_it() {
    // 005 FR-012: a replica that crashed while its range was being replaced finds its own
    // snapshot without the data: the group keeps loading and asks for it instead of stopping.
    let small = |i| CoreConfig { snapshot_entries: 3, ..cfg(i) };
    let mut c = Cluster::with(3, small);
    let l = c.elect();
    for i in 0..10u8 {
        c.propose(l, &[b'a' + i]).unwrap();
        c.ticks(1);
    }
    c.ticks(10);
    let f = c.others(l)[0];
    let want = c.sm(f).data.clone();
    c.restart(f);
    let missing = Appender::default();
    missing.missing_data.set(true);
    let desc = GroupDesc { group: G, voters: vec![], learners: vec![] };
    let node = c.nodes.get_mut(&f).unwrap();
    node.core.create_group(desc, missing).unwrap();
    c.ticks(20);
    assert!(c.nodes[&f].snapshot_data.iter().any(|(g, i)| *g == G && *i > 0));
    assert!(!c.nodes[&f].core.has_stopped_group(), "it waits instead of stopping");
    assert_ne!(c.sm(f).data, want, "nothing restored yet");
    // Still missing: asked again. Then the data is there.
    let now = c.now;
    let asked = c.nodes[&f].snapshot_data.len();
    c.nodes.get_mut(&f).unwrap().core.snapshot_data_ready(G, now);
    c.ticks(1);
    assert!(c.nodes[&f].snapshot_data.len() > asked);
    c.sm(f).missing_data.set(false);
    c.nodes.get_mut(&f).unwrap().core.snapshot_data_ready(G, now);
    c.ticks(20);
    assert_eq!(c.sm(f).data, c.sm(l).data);
    c.propose(l, b"z").unwrap();
    c.ticks(10);
    assert!(c.sm(f).data.ends_with(b"z"));
}

#[test]
fn a_leader_whose_clock_jumps_400_ms_steps_aside_within_a_second() {
    // 005 FR-006: the jump against its monotonic clock and its peers' heartbeats set it aside at
    // once; while its clock stays out of margin it does not lead again.
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.propose(l, b"a").unwrap();
    c.ticks(5);
    c.skew.insert(l, 400);
    let mut aside = None;
    for t in 1..=100u32 {
        c.ticks(1);
        if c.leader().is_some_and(|n| n != l) {
            aside = Some(t);
            break;
        }
    }
    let t = aside.expect("another node leads within a second");
    assert!(t * TICK.as_millis() as u32 <= 1_000, "{t} ticks");
    assert_eq!(c.propose(l, b"b"), Err(OpError::Overloaded), "no marks out of margin");
    for _ in 0..300 {
        c.ticks(1);
        assert_ne!(c.leader(), Some(l), "it does not lead while its clock is out");
    }
    let l2 = c.leader().expect("a leader");
    c.propose(l2, b"c").unwrap();
    c.ticks(10);
    assert!(c.sm(l2).data.ends_with(b"c"));
}

#[test]
fn a_report_of_group_0_out_of_margin_hands_over_the_leaderships() {
    // 005 FR-006: Group 0 measures a node 300 ms from the voters' median.
    let mut c = Cluster::new(3);
    let l = c.elect();
    let now = c.now;
    c.nodes.get_mut(&l).unwrap().core.on_clock_report(300, now);
    assert_eq!(c.propose(l, b"x"), Err(OpError::Overloaded), "no marks out of margin");
    c.settle();
    c.ticks(20);
    let l2 = c.leader().expect("another voter leads");
    assert_ne!(l2, l);
    // While Group 0 keeps reporting it out, it takes no leadership back.
    for _ in 0..10 {
        let now = c.now;
        c.nodes.get_mut(&l).unwrap().core.on_clock_report(300, now);
        c.ticks(50);
        assert_ne!(c.leader(), Some(l));
    }
    // Back in margin, it can lead again.
    let now = c.now;
    c.nodes.get_mut(&l).unwrap().core.on_clock_report(10, now);
    c.propose(l2, b"y").unwrap();
    c.ticks(10);
    assert!(c.sm(l2).data.ends_with(b"y"));
}
