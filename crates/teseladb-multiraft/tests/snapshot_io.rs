//! Snapshot files and the disk: reads of a snapshot that fail or arrive after their group left,
//! transfers whose peer drops while the file is read, snapshot data that arrives broken or that
//! the state machine refuses once it is here, and reads that wait across groups.

mod common;

use common::*;
use teseladb_multiraft::StateMachine;
use teseladb_multiraft::chunks::split;
use teseladb_multiraft::{CoreConfig, Done, GroupDesc, OpError, PeerFrame, ReadMode};
use teseladb_raft::{Message, MessageType, Snapshot, SnapshotMeta};
use teseladb_raftlog::{Disk, IoErrno};
use teseladb_types::{Bytes, Encoder, GroupId, NodeId};

/// A frame from a peer whose HLC says nothing yet.
const NO_HLC: teseladb_hlc::Hlc = teseladb_hlc::Hlc { physical: 0, logical: 0 };

/// Name of the disk operation that reads a snapshot file (`IoOp::name`).
const READ_OP: &str = "read";
/// Name of the disk operation that makes a write durable.
const SYNC_OP: &str = "sync";

fn small(i: u64) -> CoreConfig {
    CoreConfig { snapshot_entries: 3, ..cfg(i) }
}

/// A cluster of three whose leader compacted its log while follower `f` was cut off.
fn compacted_while_one_is_away() -> (Cluster, NodeId, NodeId) {
    let mut c = Cluster::with(3, small);
    let l = c.elect();
    let f = c.others(l)[0];
    c.isolate(f);
    for i in 0..8u8 {
        c.propose(l, &[b'a' + i]).unwrap();
        c.ticks(1);
    }
    c.ticks(5);
    assert!(c.nodes[&l].core.store().group(G).unwrap().snapshot_index > 0, "compacted");
    (c, l, f)
}

fn sends_snapshot_req(from: NodeId, to: NodeId, sent: &[(NodeId, NodeId, PeerFrame)]) -> bool {
    sent.iter().any(|(a, b, f)| {
        *a == from
            && *b == to
            && matches!(f, PeerFrame::Raft { msgs, .. }
                if msgs.iter().any(|(_, m)| m.ty == MessageType::SnapshotReq))
    })
}

#[test]
fn a_group_removed_while_its_snapshot_is_read_forgets_the_read() {
    let mut c = Cluster::with(3, small);
    let l = c.elect();
    for i in 0..8u8 {
        c.propose(l, &[i]).unwrap();
        c.ticks(1);
    }
    c.ticks(5);
    let f = c.others(l)[0];
    c.restart(f);
    // The snapshot is being read when the group is removed.
    c.slow.insert(f);
    let desc = GroupDesc { group: G, voters: vec![], learners: vec![] };
    let node = c.nodes.get_mut(&f).unwrap();
    node.core.create_group(desc, Appender::default()).unwrap();
    node.core.remove_group(G).unwrap();
    c.slow.clear();
    c.settle();
    let node = &c.nodes[&f];
    assert!(node.core.group_ids().is_empty());
    assert!(node.core.store().group(G).is_none(), "the log forgot the group");
    assert!(!node.core.has_stopped_group());
    assert_eq!(c.propose(f, b"x"), Err(OpError::GroupNotFound));
}

#[test]
fn a_snapshot_that_cannot_be_read_at_start_stops_its_group() {
    let mut c = Cluster::with(3, small);
    let l = c.elect();
    for i in 0..8u8 {
        c.propose(l, &[i]).unwrap();
        c.ticks(1);
    }
    c.ticks(5);
    let f = c.others(l)[0];
    c.restart(f);
    let node = c.nodes.get_mut(&f).unwrap();
    node.disk.inject_error(READ_OP, IoErrno::Io);
    let desc = GroupDesc { group: G, voters: vec![], learners: vec![] };
    node.core.create_group(desc, Appender::default()).unwrap();
    c.settle();
    assert!(c.nodes[&f].core.has_stopped_group());
    assert_eq!(c.sm(f).applied, 0, "nothing restored");
    assert_eq!(c.propose(f, b"x"), Err(OpError::Unavailable));
    // The other replicas go on without it.
    let op = c.propose(l, b"y").unwrap();
    c.ticks(3);
    assert!(matches!(c.results(l, op)[..], [Ok(Done::Applied(_))]));
}

#[test]
fn a_snapshot_that_cannot_be_read_for_a_transfer_is_sent_again() {
    let (mut c, l, f) = compacted_while_one_is_away();
    c.nodes.get_mut(&l).unwrap().disk.inject_error(READ_OP, IoErrno::Io);
    c.heal();
    c.sent.clear();
    c.ticks(300);
    assert!(sends_snapshot_req(l, f, &c.sent));
    assert_eq!(c.sm(f).data, c.sm(l).data, "the follower caught up with the second read");
    assert!(!c.nodes[&l].core.has_stopped_group(), "a failed read does not stop the leader");
}

#[test]
fn a_transfer_whose_peer_drops_while_the_file_is_read_still_completes() {
    let (mut c, l, f) = compacted_while_one_is_away();
    // The leader's disk holds the snapshot read until the connection to the follower dropped.
    c.slow.insert(l);
    c.heal();
    c.sent.clear();
    for _ in 0..100 {
        c.tick();
        if sends_snapshot_req(l, f, &c.sent) {
            break;
        }
    }
    assert!(sends_snapshot_req(l, f, &c.sent));
    c.nodes.get_mut(&l).unwrap().core.report_unreachable(f);
    c.slow.clear();
    c.ticks(300);
    assert_eq!(c.sm(f).data, c.sm(l).data);
}

#[test]
fn snapshot_data_announced_for_an_unknown_group_is_ignored() {
    let mut c = Cluster::new(1);
    let now = c.now;
    let core = &mut c.nodes.get_mut(&id(1)).unwrap().core;
    core.snapshot_data_ready(GroupId(99), now);
    let a = core.drain_actions();
    assert!(a.send.is_empty() && a.done.is_empty() && a.snapshot_data.is_empty());
    assert_eq!(core.group_ids(), vec![G]);
}

#[test]
fn an_own_snapshot_refused_once_its_data_arrives_stops_the_group() {
    let mut c = Cluster::with(3, small);
    let l = c.elect();
    for i in 0..8u8 {
        c.propose(l, &[i]).unwrap();
        c.ticks(1);
    }
    c.ticks(5);
    let f = c.others(l)[0];
    c.restart(f);
    let refuse = Appender { fail_restore: true, ..Appender::default() };
    refuse.missing_data.set(true);
    let missing = refuse.missing_data.clone();
    let desc = GroupDesc { group: G, voters: vec![], learners: vec![] };
    c.nodes.get_mut(&f).unwrap().core.create_group(desc, refuse).unwrap();
    c.ticks(5);
    assert!(c.nodes[&f].snapshot_data.iter().any(|(g, _)| *g == G), "asks for the data");
    assert!(c.nodes[&f].core.awaited_snapshot(G).is_some(), "its own snapshot waits");
    assert!(!c.nodes[&f].core.has_stopped_group(), "waits while the data is missing");
    // The data is here, but this version cannot restore the snapshot.
    missing.set(false);
    let now = c.now;
    c.nodes.get_mut(&f).unwrap().core.snapshot_data_ready(G, now);
    assert!(c.nodes[&f].core.has_stopped_group());
    assert_eq!(c.propose(f, b"x"), Err(OpError::Unavailable));
}

/// Hands follower `f` a snapshot from leader `l` (current term, far ahead of its log) whose file
/// is `file`, as a `SnapshotReq` and its single chunk; returns the follower's actions.
fn deliver_snapshot(
    c: &mut Cluster,
    l: NodeId,
    f: NodeId,
    file: &[u8],
) -> teseladb_multiraft::Actions<u64> {
    let st = c.nodes[&f].core.status(G).unwrap();
    let index = st.hard_state.commit + 100;
    let term = st.hard_state.term;
    let conf = c.nodes[&f].core.conf(G).unwrap().clone();
    let mut m = Message::new(MessageType::SnapshotReq, l, f, term);
    m.index = index;
    m.snapshot = Some(Snapshot { meta: SnapshotMeta { index, term, conf }, data: Bytes::new() });
    let now = c.now;
    let node = c.nodes.get_mut(&f).unwrap();
    node.core.on_peer_message(
        l,
        PeerFrame::Raft { wall_ms: 0, hlc: NO_HLC, msgs: vec![(G, m)] },
        now,
    );
    let chunk = split(G, index, term, file)[0].clone();
    node.core.on_peer_message(l, PeerFrame::SnapshotChunk(chunk), now);
    node.core.drain_actions()
}

fn refused_to(l: NodeId, a: &teseladb_multiraft::Actions<u64>) -> bool {
    a.send.iter().any(|(to, fr)| {
        *to == l && matches!(fr, PeerFrame::SnapshotAck { group, ok: false, .. } if *group == G)
    })
}

#[test]
fn a_received_snapshot_whose_file_cannot_be_decoded_is_refused_and_stops_the_group() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.ticks(5);
    let f = c.others(l)[0];
    let applied = c.sm(f).applied;
    // One byte: too short to hold the file's header.
    let a = deliver_snapshot(&mut c, l, f, b"x");
    assert!(refused_to(l, &a));
    assert!(c.nodes[&f].core.has_stopped_group());
    assert_eq!(c.sm(f).applied, applied, "nothing applied from it");
}

#[test]
fn a_received_snapshot_the_stopped_log_cannot_save_stops_the_group() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.ticks(5);
    let f = c.others(l)[0];
    // The follower's log fails a write of another group and stops (fail-stop); group `G` has
    // nothing to write until the snapshot arrives.
    c.isolate(f);
    let node = c.nodes.get_mut(&f).unwrap();
    node.disk.inject_error(SYNC_OP, IoErrno::Io);
    let other = GroupDesc { group: GroupId(8), voters: vec![f], learners: vec![] };
    node.core.create_group(other, Appender::default()).unwrap();
    c.settle();
    let now = c.now;
    assert!(
        matches!(
            c.nodes.get_mut(&f).unwrap().core.read(G, ReadMode::ReadIndex, now),
            Err(OpError::NotLeader { .. })
        ),
        "group G still takes operations"
    );
    // A valid file: the cluster time, then the state machine's snapshot.
    let sm = Appender { data: b"abc".to_vec(), applied: 200, entries: 3, ..Appender::default() };
    let mut file = Encoder::new();
    file.u64(WALL0).raw(&sm.snapshot());
    let a = deliver_snapshot(&mut c, l, f, &file.finish());
    assert!(
        !a.send.iter().any(|(_, fr)| matches!(fr, PeerFrame::SnapshotAck { ok: true, .. })),
        "a snapshot that is not durable is not acknowledged"
    );
    let now = c.now;
    assert_eq!(
        c.nodes.get_mut(&f).unwrap().core.read(G, ReadMode::ReadIndex, now),
        Err(OpError::Unavailable)
    );
}

#[test]
fn a_read_waits_through_entries_below_its_index_and_other_groups_applying() {
    // Group `H` has the leader of `G` as its only voter.
    const H: GroupId = GroupId(3);
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.ticks(5);
    let desc = GroupDesc { group: H, voters: vec![l], learners: vec![] };
    c.nodes.get_mut(&l).unwrap().core.create_group(desc, Appender::default()).unwrap();
    c.ticks(2);
    // The leader's disk stops answering: the followers commit `a` and then `b`, while the
    // leader's first write is held on the way to its disk.
    c.slow.insert(l);
    c.propose(l, b"a").unwrap();
    c.settle();
    let now = c.now;
    let held = c.nodes.get_mut(&l).unwrap().core.poll_io(now).expect("the write of a");
    c.propose(l, b"b").unwrap();
    c.settle();
    let node = c.nodes.get_mut(&l).unwrap();
    let h_op = node.core.propose(H, b"h", now).unwrap();
    let st = node.core.status(G).unwrap();
    assert!(st.hard_state.commit >= st.applied + 2, "{st:?}");
    let read = node.core.read(G, ReadMode::Lease, now).unwrap();
    // The disk answers: `H` and the first entry of `G` apply before the read's index.
    let done = node.disk.execute(held);
    node.core.on_io_done(done, now);
    c.slow.clear();
    c.settle();
    assert_eq!(c.results(l, h_op), vec![Ok(Done::Applied(1))]);
    assert_eq!(c.results(l, read), vec![Ok(Done::Read { index: st.hard_state.commit })]);
    assert_eq!(c.sm(l).data, b"ab");
}

/// A leader's snapshot parked for its data moves Raft's commit index to the snapshot's index in
/// memory only. Until the snapshot is durable, no hard state on disk may carry that commit: after a
/// restart the replica would take the old entries the snapshot replaces, never committed, for
/// committed and apply them (seed 4058 of `txn-bank`, heavy plan). Here a newer snapshot from a
/// deposed term takes the parked one's place and Raft ignores it, so nothing is parked any more,
/// and the follower, cut off, campaigns and persists its new term.
#[test]
fn a_parked_snapshot_puts_no_commit_on_disk_until_it_is_durable() {
    let mut c = Cluster::new(3);
    // A second term, so that a term below the follower's exists.
    let first = c.elect();
    c.isolate(first);
    let mut l = first;
    for _ in 0..1_000 {
        c.tick();
        if let Some(n) = c.leader().filter(|n| *n != first) {
            l = n;
            break;
        }
    }
    assert_ne!(l, first, "a new leader");
    c.heal();
    c.ticks(5);
    let f = c.others(l)[0];
    c.isolate(f);
    let on_disk = |c: &Cluster| c.nodes[&f].core.store().hard_state(G).unwrap().commit;
    let before = on_disk(&c);
    c.sm(f).missing_data.set(true);
    let sm = Appender { data: b"abc".to_vec(), applied: 200, entries: 3, ..Appender::default() };
    // The file as a leader writes it: its cluster time, no term runs, the state machine's part.
    let mut file = Encoder::new();
    file.u64(WALL0).var(0).var(0).raw(&sm.snapshot());
    let file = file.finish();
    deliver_snapshot(&mut c, l, f, &file);
    assert!(c.nodes[&f].core.awaited_snapshot(G).is_some(), "parked for its data");
    let st = c.nodes[&f].core.status(G).unwrap();
    assert!(st.hard_state.commit > before, "Raft took the snapshot's commit in memory");
    assert!(st.hard_state.term >= 2, "a term below it exists");
    // A newer snapshot from an earlier term: it replaces the parked one, and Raft ignores it.
    let (index, term) = (st.hard_state.commit + 100, st.hard_state.term - 1);
    let conf = c.nodes[&f].core.conf(G).unwrap().clone();
    let mut m = Message::new(MessageType::SnapshotReq, l, f, term);
    m.index = index;
    m.snapshot = Some(Snapshot { meta: SnapshotMeta { index, term, conf }, data: Bytes::new() });
    let now = c.now;
    let node = c.nodes.get_mut(&f).unwrap();
    node.core.on_peer_message(
        l,
        PeerFrame::Raft { wall_ms: 0, hlc: NO_HLC, msgs: vec![(G, m)] },
        now,
    );
    let chunk = split(G, index, term, &file)[0].clone();
    node.core.on_peer_message(l, PeerFrame::SnapshotChunk(chunk), now);
    node.core.drain_actions();
    // Cut off for longer than an election, it votes for the third node in a higher term, and
    // persists the vote.
    c.ticks(50);
    let st = c.nodes[&f].core.status(G).unwrap();
    let other = c.others(f).into_iter().find(|n| *n != l).unwrap();
    let mut vote = Message::new(MessageType::VoteReq, other, f, st.hard_state.term + 1);
    (vote.index, vote.log_term) = (st.last_index + 1_000, st.hard_state.term);
    let now = c.now;
    let node = c.nodes.get_mut(&f).unwrap();
    node.core.on_peer_message(
        other,
        PeerFrame::Raft { wall_ms: 0, hlc: NO_HLC, msgs: vec![(G, vote)] },
        now,
    );
    c.ticks(5);
    assert_eq!(
        c.nodes[&f].core.store().hard_state(G).unwrap().vote,
        other.0,
        "it voted and persisted its vote"
    );
    assert!(
        on_disk(&c) <= before,
        "commit {} on disk without its snapshot (it was {before})",
        on_disk(&c)
    );
}
