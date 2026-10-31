//! Edge cases of `MultiRaftCore`: a slow disk, a leader without its first entry committed, a log
//! that fails, snapshots that cannot be restored, transfers without an answer and frames that do
//! not belong to anything.

mod common;

use common::*;
use teseladb_multiraft::chunks::split;
use teseladb_multiraft::{
    ApplyError, CoreConfig, Done, GroupDesc, OpError, PeerFrame, ReadMode, RestoreError, coalesce,
};
use teseladb_raft::{Message, MessageType, Role, Snapshot, SnapshotMeta};
use teseladb_raftlog::IoErrno;
use teseladb_types::{Bytes, GroupId, Nanos, NodeId};

/// A frame from a peer whose HLC says nothing yet.
const NO_HLC: teseladb_hlc::Hlc = teseladb_hlc::Hlc { physical: 0, logical: 0 };

fn has_append_resp(_: NodeId, _: NodeId, f: &PeerFrame) -> bool {
    matches!(f, PeerFrame::Raft { msgs, .. } if msgs.iter().any(|(_, m)| m.ty == MessageType::AppendResp))
}

fn is_chunk(_: NodeId, _: NodeId, f: &PeerFrame) -> bool {
    matches!(f, PeerFrame::SnapshotChunk(_))
}

#[test]
fn a_read_waits_until_the_leader_applied_its_index() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.ticks(5);
    // The leader's disk stops answering: the followers acknowledge the entry and it is committed,
    // but the leader cannot apply it before persisting it.
    c.slow.insert(l);
    let op = c.propose(l, b"x").unwrap();
    c.settle();
    let st = c.nodes[&l].core.status(G).unwrap();
    assert!(st.hard_state.commit > st.applied, "{st:?}");
    let now = c.now;
    let read = c.nodes.get_mut(&l).unwrap().core.read(G, ReadMode::Lease, now).unwrap();
    c.settle();
    assert!(c.results(l, read).is_empty(), "waits for its index");
    c.slow.clear();
    c.settle();
    assert_eq!(c.results(l, op), vec![Ok(Done::Applied(1))]);
    assert_eq!(c.results(l, read), vec![Ok(Done::Read { index: st.hard_state.commit })]);
}

#[test]
fn a_read_index_waits_for_the_first_entry_of_the_term_or_ends_with_not_leader() {
    // The followers' acknowledgements are lost: the new leader cannot commit its `Noop`.
    let mut c = Cluster::new(3);
    c.drop = Some(has_append_resp);
    let l = loop {
        c.tick();
        if let Some(l) = c.leader() {
            break l;
        }
    };
    let now = c.now;
    let op = c.nodes.get_mut(&l).unwrap().core.read(G, ReadMode::ReadIndex, now).unwrap();
    c.ticks(5);
    assert!(c.results(l, op).is_empty(), "retried every tick");
    c.drop = None;
    c.ticks(30);
    let commit = c.nodes[&l].core.status(G).unwrap().hard_state.commit;
    assert_eq!(c.results(l, op), vec![Ok(Done::Read { index: commit })]);
    assert_eq!(c.nodes[&l].core.read_counts(G), Some((0, 1)), "one read with ReadIndex");
    assert_eq!(c.nodes[&l].core.read_counts(GroupId(77)), None);
    // Again without acknowledgements, and the leader loses its quorum while it retries.
    let mut c = Cluster::new(3);
    c.drop = Some(has_append_resp);
    let l = loop {
        c.tick();
        if let Some(l) = c.leader() {
            break l;
        }
    };
    let now = c.now;
    let op = c.nodes.get_mut(&l).unwrap().core.read(G, ReadMode::ReadIndex, now).unwrap();
    c.isolate(l);
    c.ticks(60);
    // Cut off, it still leads (it takes writes until `step_down_ticks`) but confirms no read.
    assert_eq!(c.nodes[&l].core.status(G).unwrap().role, Role::Leader);
    let r = c.results(l, op);
    assert!(matches!(&r[..], [Err(OpError::NotLeader { .. })]), "{r:?}");
}

#[test]
fn a_registered_read_ends_as_soon_as_its_leader_steps_down() {
    // A long deadline: the read must end because of the step down, not because of the timeout.
    let slow = |i| CoreConfig { op_timeout: Nanos::from_secs(60), ..cfg(i) };
    let mut c = Cluster::with(3, slow);
    let l = c.elect();
    c.ticks(10);
    // The leader registers a read (its term has committed) and is cut off before confirming it.
    c.isolate(l);
    let now = c.now;
    let op = c.nodes.get_mut(&l).unwrap().core.read(G, ReadMode::ReadIndex, now).unwrap();
    c.ticks(2);
    assert!(c.results(l, op).is_empty(), "confirmed without a quorum");
    // The others elect another leader; back in touch, the old one learns its term and steps down.
    let others = c.others(l);
    for _ in 0..200 {
        c.tick();
        if others.iter().any(|&o| c.nodes[&o].core.status(G).unwrap().role == Role::Leader) {
            break;
        }
    }
    c.heal();
    for _ in 0..200 {
        c.tick();
        if c.nodes[&l].core.status(G).unwrap().role != Role::Leader {
            break;
        }
    }
    assert_ne!(c.nodes[&l].core.status(G).unwrap().role, Role::Leader);
    c.ticks(2);
    let r = c.results(l, op);
    assert!(matches!(&r[..], [Err(OpError::NotLeader { .. })]), "{r:?}");
}

#[test]
fn a_late_read_state_is_dropped() {
    let quick = |i| CoreConfig { op_timeout: Nanos::from_millis(20), ..cfg(i) };
    let mut c = Cluster::with(3, quick);
    let l = c.elect();
    for f in c.others(l) {
        c.cut.insert((f, l));
    }
    let now = c.now;
    let op = c.nodes.get_mut(&l).unwrap().core.read(G, ReadMode::ReadIndex, now).unwrap();
    c.ticks(3);
    assert_eq!(c.results(l, op), vec![Err(OpError::Timeout)]);
    // The answers arrive now: the read is no longer awaited and nothing else ends.
    c.heal();
    c.ticks(10);
    assert_eq!(c.nodes[&l].done.len(), 1);
}

#[test]
fn a_log_that_fails_stops_its_groups() {
    let mut c = Cluster::new(1);
    let l = c.elect();
    c.nodes.get_mut(&l).unwrap().disk.inject_error("sync", IoErrno::Io);
    let first = c.propose(l, b"a").unwrap();
    c.settle();
    // The failed write is never acknowledged; the next one is refused and the group stops.
    assert!(c.results(l, first).is_empty());
    let second = c.propose(l, b"b");
    c.settle();
    assert_eq!(c.results(l, first), vec![Err(OpError::Unavailable)]);
    assert!(second.is_err() || c.results(l, second.unwrap()) == vec![Err(OpError::Unavailable)]);
    assert_eq!(c.propose(l, b"c"), Err(OpError::Unavailable));
    let now = c.now;
    let node = c.nodes.get_mut(&l).unwrap();
    assert_eq!(node.core.read(G, ReadMode::ReadIndex, now), Err(OpError::Unavailable));
    let cc = teseladb_raft::ConfChange { kind: teseladb_raft::ConfChangeKind::Remove, node: l };
    assert_eq!(node.core.propose_conf_change(G, cc, now), Err(OpError::Unavailable));
    // The log refuses the deletion too; the group leaves the core anyway.
    node.core.remove_group(G).unwrap();
    assert!(node.core.group_ids().is_empty());
    // And no new group fits in it.
    let desc = GroupDesc { group: GroupId(8), voters: vec![l], learners: vec![] };
    assert_eq!(node.core.create_group(desc, Appender::default()), Err(OpError::Unavailable));
}

#[test]
fn a_snapshot_the_state_machine_cannot_restore_stops_the_group() {
    // On restart.
    let small = |i| CoreConfig { snapshot_entries: 3, ..cfg(i) };
    let mut c = Cluster::with(3, small);
    let l = c.elect();
    for i in 0..8u8 {
        c.propose(l, &[i]).unwrap();
        c.ticks(1);
    }
    c.ticks(5);
    let f = c.others(l)[0];
    c.restart(f);
    let now = c.now;
    let node = c.nodes.get_mut(&f).unwrap();
    let desc = GroupDesc { group: G, voters: vec![], learners: vec![] };
    node.core.create_group(desc, Appender { fail_restore: true, ..Appender::default() }).unwrap();
    // Still reading its snapshot: no operations.
    assert_eq!(node.core.read(G, ReadMode::Lease, now), Err(OpError::Unavailable));
    c.settle();
    assert_eq!(c.propose(f, b"x"), Err(OpError::Unavailable));
    assert_eq!(c.sm(f).applied, 0);

    // Received from the leader: it is refused and the leader hears it.
    let mut c = Cluster::with(3, small);
    let l = c.elect();
    let f = c.others(l)[0];
    c.isolate(f);
    for i in 0..8u8 {
        c.propose(l, &[i]).unwrap();
        c.ticks(1);
    }
    c.ticks(5);
    c.restart(f);
    let desc = GroupDesc { group: G, voters: vec![], learners: vec![] };
    let refuse = Appender { fail_restore: true, ..Appender::default() };
    c.nodes.get_mut(&f).unwrap().core.create_group(desc, refuse).unwrap();
    c.heal();
    c.ticks(30);
    assert!(c.sent.iter().any(|(from, to, fr)| *from == f
        && *to == l
        && matches!(fr, PeerFrame::SnapshotAck { ok: false, .. })));
    assert_eq!(c.propose(f, b"x"), Err(OpError::Unavailable));
}

#[test]
fn an_unanswered_snapshot_transfer_is_sent_again() {
    let small = |i| CoreConfig { snapshot_entries: 3, ..cfg(i) };
    let mut c = Cluster::with(3, small);
    let l = c.elect();
    let f = c.others(l)[0];
    c.isolate(f);
    for i in 0..8u8 {
        c.propose(l, &[i]).unwrap();
        c.ticks(1);
    }
    c.ticks(5);
    // The snapshot's data is lost on the way: the leader waits for an answer that never comes.
    c.heal();
    c.drop = Some(is_chunk);
    c.sent.clear();
    c.ticks(1_100);
    let starts = c
        .sent
        .iter()
        .filter(|(from, to, fr)| {
            *from == l && *to == f && matches!(fr, PeerFrame::SnapshotChunk(ch) if ch.offset == 0)
        })
        .count();
    assert!(starts >= 2, "sent again after the timeout: {starts}");
    c.drop = None;
    c.ticks(1_100);
    assert_eq!(c.sm(f).data, c.sm(l).data);
}

#[test]
fn a_small_snapshot_lost_on_the_way_is_sent_again_within_seconds() {
    // The deadline grows with the snapshot's size: a small one is not waited for ten seconds.
    let small = |i| CoreConfig { snapshot_entries: 3, ..cfg(i) };
    let mut c = Cluster::with(3, small);
    let l = c.elect();
    let f = c.others(l)[0];
    c.isolate(f);
    for i in 0..8u8 {
        c.propose(l, &[i]).unwrap();
        c.ticks(1);
    }
    c.ticks(5);
    c.heal();
    c.drop = Some(is_chunk);
    c.sent.clear();
    c.ticks(250);
    let starts = c
        .sent
        .iter()
        .filter(|(from, to, fr)| {
            *from == l && *to == f && matches!(fr, PeerFrame::SnapshotChunk(ch) if ch.offset == 0)
        })
        .count();
    assert!(starts >= 2, "sent again within 2.5 s: {starts}");
    c.drop = None;
    c.ticks(300);
    assert_eq!(c.sm(f).data, c.sm(l).data);
}

#[test]
fn an_entry_the_state_machine_cannot_apply_stops_every_replica() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.propose(l, b"poison").unwrap();
    c.ticks(30);
    for n in (1..=3).map(id) {
        assert_eq!(c.propose(n, b"x"), Err(OpError::Unavailable), "{n:?}");
        assert!(c.nodes[&n].core.has_stopped_group());
    }
    let to = c.others(l)[0];
    assert!(!c.nodes.get_mut(&l).unwrap().core.transfer_leader(G, to), "a stopped group");
    assert!(!Cluster::new(1).nodes[&id(1)].core.has_stopped_group());
}

#[test]
fn frames_that_belong_to_nothing_are_dropped() {
    let mut c = Cluster::new(2);
    let l = c.elect();
    let f = c.others(l)[0];
    let now = c.now;
    let node = c.nodes.get_mut(&f).unwrap();
    // A group this core does not have.
    let m = Message::new(MessageType::Heartbeat, l, f, 99);
    node.core.on_peer_message(
        l,
        PeerFrame::Raft { wall_ms: 0, hlc: NO_HLC, msgs: vec![(GroupId(99), m)] },
        now,
    );
    let ack = PeerFrame::SnapshotAck { group: GroupId(99), index: 1, offset: 0, ok: true };
    node.core.on_peer_message(l, ack, now);
    // A snapshot request and data of different indexes: the older one is discarded.
    let snapshot_req = |index| {
        let mut m = Message::new(MessageType::SnapshotReq, l, f, 1);
        m.index = index;
        m.snapshot = Some(Snapshot {
            meta: SnapshotMeta { index, term: 1, ..Default::default() },
            data: Bytes::new(),
        });
        PeerFrame::Raft { wall_ms: 0, hlc: NO_HLC, msgs: vec![(GroupId(55), m)] }
    };
    let chunk = |index| PeerFrame::SnapshotChunk(split(GroupId(55), index, 1, b"x")[0].clone());
    node.core.on_peer_message(l, chunk(5), now);
    node.core.on_peer_message(l, snapshot_req(7), now);
    node.core.on_peer_message(l, snapshot_req(4), now);
    node.core.on_peer_message(l, chunk(9), now);
    // Every frame of a group this core does not run goes to the layer above, which hands it to
    // the core of the node that runs the group, or drops it (007 FR-011).
    let strays = node.core.take_strays();
    assert_eq!(strays.len(), 6);
    assert!(strays.iter().all(|(from, _)| *from == l));
    // The same in a group it runs: the older of a request and data that do not match goes.
    let own = |f: PeerFrame| match f {
        PeerFrame::Raft { wall_ms, hlc, msgs } => {
            PeerFrame::Raft { wall_ms, hlc, msgs: msgs.into_iter().map(|(_, m)| (G, m)).collect() }
        }
        PeerFrame::SnapshotChunk(mut ch) => {
            ch.group = G;
            PeerFrame::SnapshotChunk(ch)
        }
        other => other,
    };
    node.core.on_peer_message(l, own(chunk(5)), now);
    node.core.on_peer_message(l, own(snapshot_req(7)), now);
    node.core.on_peer_message(l, own(snapshot_req(4)), now);
    node.core.on_peer_message(l, own(chunk(9)), now);
    assert!(node.core.take_strays().is_empty());
    // A chunk of its own group that does not continue anything is refused.
    let mut late = split(G, 3, 1, &vec![0; (1 << 20) + 1])[1].clone();
    late.index = 3;
    node.core.on_peer_message(l, PeerFrame::SnapshotChunk(late), now);
    let a = node.core.drain_actions();
    assert!(a.send.iter().any(
        |(to, fr)| *to == l && matches!(fr, PeerFrame::SnapshotAck { group: G, ok: false, .. })
    ));
    assert!(a.done.is_empty());
    c.settle();
    // The group keeps working.
    let op = c.propose(l, b"ok").unwrap();
    c.ticks(3);
    assert_eq!(c.results(l, op), vec![Ok(Done::Applied(2))]);
}

#[test]
fn answers_to_forwarded_requests_find_their_request() {
    let mut c = Cluster::new(2);
    let now = c.now;
    let n1 = c.nodes.get_mut(&id(1)).unwrap();
    let a = n1.core.forward(id(2), Bytes::from("a"), now);
    let b = n1.core.forward(id(2), Bytes::from("b"), now);
    c.settle();
    let fw = std::mem::take(&mut c.nodes.get_mut(&id(2)).unwrap().forwarded);
    assert_eq!(fw.len(), 2);
    // Answered in the reverse order.
    for f in fw.iter().rev() {
        let body = Bytes::from_vec([b"re:".as_slice(), &f.body].concat());
        c.nodes.get_mut(&id(2)).unwrap().core.answer_forward(f.from, f.req, body);
    }
    c.settle();
    assert_eq!(c.results(id(1), a), vec![Ok(Done::Forwarded(Bytes::from("re:a")))]);
    assert_eq!(c.results(id(1), b), vec![Ok(Done::Forwarded(Bytes::from("re:b")))]);
}

#[test]
fn messages_to_this_node_are_not_sent_and_the_state_is_visible() {
    let me = id(1);
    let mut to_me = Message::new(MessageType::AppendResp, id(2), me, 1);
    to_me.index = 3;
    let to_two = Message::new(MessageType::AppendReq, me, id(2), 1);
    let frames = coalesce(me, vec![(G, to_me), (G, to_two.clone())], 0, NO_HLC, &|_, _| None);
    assert_eq!(
        frames,
        vec![(id(2), PeerFrame::Raft { wall_ms: 0, hlc: NO_HLC, msgs: vec![(G, to_two)] })]
    );
    let c = Cluster::new(1);
    let core = &c.nodes[&me].core;
    assert_eq!(core.group_ids(), vec![G]);
    assert_eq!(core.conf(G).unwrap().voters, vec![me]);
    assert!(core.conf(GroupId(1)).is_none());
    assert_eq!(core.store().group_ids(), vec![G]);
    assert_eq!(core.next_deadline(), None);
    assert!(!ApplyError("x".into()).to_string().is_empty());
    assert!(!RestoreError("x".into()).to_string().is_empty());
}

#[test]
fn a_new_group_with_an_invalid_configuration_is_refused_before_it_is_stored() {
    let mut disk = teseladb_raftlog::MemDisk::new();
    let storage = teseladb_raftlog::StorageConfig::default();
    let (store, _) = teseladb_raftlog::CoreStore::recover(&storage, 0, &mut disk).unwrap();
    let mut core = teseladb_multiraft::MultiRaftCore::new(cfg(1), store, 1);
    let g = GroupId(7);
    // A node that is both voter and learner cannot be read back.
    let both = GroupDesc { group: g, voters: vec![id(1)], learners: vec![id(1)] };
    assert_eq!(core.create_group(both, Appender::default()), Err(OpError::InvalidConfChange));
    assert!(core.group_ids().is_empty());
    assert!(core.store().group(g).is_none(), "nothing reaches the log");
    let ok = GroupDesc { group: g, voters: vec![id(1)], learners: vec![id(2)] };
    core.create_group(ok, Appender::default()).unwrap();
    assert_eq!(core.group_ids(), vec![g]);
}

#[test]
fn removing_a_group_leaves_forwarded_requests_running() {
    let mut c = Cluster::new(2);
    let now = c.now;
    let n1 = c.nodes.get_mut(&id(1)).unwrap();
    let op = n1.core.forward(id(2), Bytes::from("q"), now);
    // Forwarded requests belong to no group: removing one does not end them.
    n1.core.remove_group(G).unwrap();
    c.settle();
    assert!(c.results(id(1), op).is_empty());
    let fw = std::mem::take(&mut c.nodes.get_mut(&id(2)).unwrap().forwarded);
    let n2 = c.nodes.get_mut(&id(2)).unwrap();
    n2.core.answer_forward(fw[0].from, fw[0].req, Bytes::from("a"));
    c.settle();
    assert_eq!(c.results(id(1), op), vec![Ok(Done::Forwarded(Bytes::from("a")))]);
}

#[test]
fn a_configuration_change_waits_for_the_term_entry_and_for_the_previous_change() {
    let learner =
        |n| teseladb_raft::ConfChange { kind: teseladb_raft::ConfChangeKind::AddLearner, node: n };
    // The followers' acknowledgements are lost: the new leader has not committed its `Noop`.
    let mut c = Cluster::new(3);
    c.drop = Some(has_append_resp);
    let l = loop {
        c.tick();
        if let Some(l) = c.leader() {
            break l;
        }
    };
    let now = c.now;
    let core = &mut c.nodes.get_mut(&l).unwrap().core;
    assert_eq!(core.propose_conf_change(G, learner(id(4)), now), Err(OpError::NotReady));
    c.drop = None;
    let l = c.elect();
    // One change at a time: the second waits until the first is applied.
    c.isolate(l);
    let now = c.now;
    let core = &mut c.nodes.get_mut(&l).unwrap().core;
    core.propose_conf_change(G, learner(id(4)), now).unwrap();
    assert_eq!(core.propose_conf_change(G, learner(id(5)), now), Err(OpError::ConfChangePending));
}
