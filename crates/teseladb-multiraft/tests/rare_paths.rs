//! Paths of `MultiRaftCore` that only rare timings take (005 T031): a snapshot whose data arrives
//! before its request when its group closes, a retried read whose leader steps down, frames another
//! core hands over for a group this one does not run either, state machines that halt in the middle
//! of a batch or report a state outside their log, configuration changes another leader's log
//! replaces, and snapshots a full disk refuses.

mod common;

use common::*;
use teseladb_multiraft::{
    ApplyCtx, ApplyError, CoreConfig, Durable, GroupDesc, GroupImport, MultiRaftCore, OpError,
    PeerFrame, ReadMode, RestoreError, StateMachine,
};
use teseladb_raft::{ConfChange, ConfChangeKind, Message, MessageType, Role};
use teseladb_raftlog::{CoreStore, Disk, IoErrno, MemDisk, StorageConfig};
use teseladb_types::{GroupId, Nanos, NodeId};

/// A frame from a peer whose HLC says nothing yet.
const NO_HLC: teseladb_hlc::Hlc = teseladb_hlc::Hlc { physical: 0, logical: 0 };
/// A group of one voter next to `G` on the same core.
const H: GroupId = GroupId(8);

fn small(i: u64) -> CoreConfig {
    CoreConfig { snapshot_entries: 3, ..cfg(i) }
}

fn has_append_resp(_: NodeId, _: NodeId, f: &PeerFrame) -> bool {
    matches!(f, PeerFrame::Raft { msgs, .. } if msgs.iter().any(|(_, m)| m.ty == MessageType::AppendResp))
}

fn has_snapshot_req(_: NodeId, _: NodeId, f: &PeerFrame) -> bool {
    matches!(f, PeerFrame::Raft { msgs, .. } if msgs.iter().any(|(_, m)| m.ty == MessageType::SnapshotReq))
}

/// A cluster of three whose follower missed entries past the leader's snapshots, cut off.
fn behind() -> (Cluster, NodeId, NodeId) {
    let mut c = Cluster::with(3, small);
    let l = c.elect();
    let f = c.others(l)[0];
    c.isolate(f);
    for i in 0..8u8 {
        c.propose(l, &[i]).unwrap();
        c.ticks(1);
    }
    c.ticks(5);
    (c, l, f)
}

/// Runs a core's disk until it has nothing more to do now.
fn run_disk(core: &mut MultiRaftCore<impl StateMachine>, disk: &mut MemDisk, now: Nanos) {
    for _ in 0..10_000 {
        let Some(op) = core.poll_io(now) else { return };
        let d = disk.execute(op);
        core.on_io_done(d, now);
    }
    panic!("the disk never empties");
}

/// Fills the disk of node `n`: its store's next file reservation fails with no space, and a group
/// of one voter created there is the write that reserves it.
fn fill_disk(c: &mut Cluster, n: NodeId) {
    let now = c.now;
    let node = c.nodes.get_mut(&n).unwrap();
    node.disk.inject_error("create", IoErrno::NoSpace);
    let h = GroupDesc { group: H, voters: vec![n], learners: vec![] };
    node.core.create_group(h, Appender::default()).unwrap();
    run_disk(&mut node.core, &mut node.disk, now);
    assert!(node.core.store().is_disk_full(), "the reservation failed");
}

#[test]
fn a_group_whose_snapshot_arrived_before_its_request_does_not_close() {
    let (mut c, l, f) = behind();
    // The data of the snapshot arrives whole; the request that installs it is lost.
    c.drop = Some(has_snapshot_req);
    c.heal();
    for _ in 0..200 {
        c.tick();
        if c.sent.iter().any(|(_, to, fr)| *to == f && matches!(fr, PeerFrame::SnapshotChunk(_))) {
            break;
        }
    }
    let core = &mut c.nodes.get_mut(&f).unwrap().core;
    assert_eq!(core.close_group(G), Err(OpError::Busy), "its snapshot waits for its request");
    // The leader sends it again after its timeout, and the follower catches up.
    c.drop = None;
    c.ticks(1_100);
    assert_eq!(c.sm(f).data, c.sm(l).data);
}

#[test]
fn a_retried_read_whose_leader_stepped_down_ends_with_not_leader() {
    // The followers' acknowledgements are lost: the new leader's reads wait for its `Noop`.
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
    // A heartbeat of a newer term: the leader steps down before its next retry.
    let f = c.others(l)[0];
    let term = c.nodes[&l].core.status(G).unwrap().hard_state.term;
    let beat = Message::new(MessageType::Heartbeat, f, l, term + 1);
    let frame = PeerFrame::Raft { wall_ms: 0, hlc: NO_HLC, msgs: vec![(G, beat)] };
    c.nodes.get_mut(&l).unwrap().core.on_peer_message(f, frame, now);
    assert_ne!(c.nodes[&l].core.status(G).unwrap().role, Role::Leader);
    assert!(c.results(l, op).is_empty(), "a read not registered stays until the retry");
    c.tick();
    let r = c.results(l, op);
    assert!(matches!(&r[..], [Err(OpError::NotLeader { hint })] if *hint == f), "{r:?}");
}

#[test]
fn a_frame_another_core_hands_over_for_a_group_this_one_does_not_run_is_dropped() {
    let mut c = Cluster::new(1);
    let l = c.elect();
    let now = c.now;
    let core = &mut c.nodes.get_mut(&l).unwrap().core;
    let beat = Message::new(MessageType::Heartbeat, NodeId(2), l, 9);
    let frame = PeerFrame::Raft { wall_ms: 0, hlc: NO_HLC, msgs: vec![(GroupId(99), beat)] };
    core.on_stray(NodeId(2), frame, now);
    assert!(core.take_strays().is_empty(), "not kept for another core either");
    assert_eq!(core.group_ids(), vec![G]);
}

#[test]
fn a_state_machine_that_halts_in_the_middle_of_a_batch_is_handed_none_of_the_rest() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    let f = c.others(l)[0];
    let at = c.sm(f).applied + 1;
    c.sm(f).halt_at.set(at);
    // Cut off, the follower gets the three entries and their commit in one batch when it is back.
    c.isolate(f);
    for i in 0..3u8 {
        c.propose(l, &[b'x' + i]).unwrap();
    }
    c.ticks(3);
    c.heal();
    c.ticks(10);
    assert_eq!(c.sm(f).applied, at, "it stops at the entry that halted it");
    assert_eq!(c.sm(l).applied, at + 2);
    let log = c.nodes[&f].core.store().group(G).unwrap();
    assert!(log.hard.commit >= at + 2, "the rest stays in its log, committed");
}

/// The cut-off leader `l` of a cluster of three proposes a configuration change and a write that
/// the others never see, then the others elect a leader that writes over them; `l` is back.
fn replaced_change(mk: fn(u64) -> CoreConfig, writes: u8) -> (Cluster, NodeId) {
    let mut c = Cluster::with(3, mk);
    let l = c.elect();
    c.isolate(l);
    let now = c.now;
    let cc = ConfChange { kind: ConfChangeKind::AddLearner, node: NodeId(9) };
    c.nodes.get_mut(&l).unwrap().core.propose_conf_change(G, cc, now).unwrap();
    c.propose(l, b"lost").unwrap();
    c.settle();
    let others = c.others(l);
    let mut m = None;
    for _ in 0..1_000 {
        c.tick();
        m = others
            .iter()
            .copied()
            .find(|&o| c.nodes[&o].core.status(G).unwrap().role == Role::Leader);
        if m.is_some() {
            break;
        }
    }
    let m = m.expect("the others elect a leader");
    for i in 0..writes {
        c.propose(m, &[b'k', i]).unwrap();
        c.ticks(1);
    }
    c.heal();
    c.ticks(60);
    assert_eq!(c.sm(l).data, c.sm(m).data, "the old leader follows the new one");
    (c, l)
}

/// Whether the entries `n` rolled back are the one write `lost`, in its envelope.
fn rolled_only_the_write(c: &Cluster, n: NodeId) -> bool {
    let rolled = &c.nodes[&n].rolled;
    rolled.len() == 1 && rolled[0].1.data.ends_with(b"lost")
}

#[test]
fn a_configuration_change_another_leader_replaces_is_not_rolled_back_as_a_write() {
    let (c, l) = replaced_change(cfg, 2);
    assert!(rolled_only_the_write(&c, l), "{:?}", c.nodes[&l].rolled);
}

#[test]
fn a_configuration_change_a_snapshot_supersedes_is_not_rolled_back_as_a_write() {
    // The new leader compacts its log past the old one's entries: they go with a snapshot.
    let (c, l) = replaced_change(small, 8);
    assert!(c.nodes[&l].core.snapshot_index(G).unwrap() > 0);
    assert!(rolled_only_the_write(&c, l), "{:?}", c.nodes[&l].rolled);
}

#[test]
fn a_snapshot_the_disk_has_no_room_for_waits_for_the_next_one() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    for i in 0..8u8 {
        c.propose(l, &[i]).unwrap();
        c.ticks(1);
    }
    c.ticks(3);
    let f = c.others(l)[0];
    // Restarted with a snapshot every three entries over a disk without room.
    let node = c.nodes.get_mut(&f).unwrap();
    let (store, _) = CoreStore::recover(&StorageConfig::default(), 0, &mut node.disk).unwrap();
    node.core = MultiRaftCore::new(small(f.0), store, 77);
    fill_disk(&mut c, f);
    // It applies its committed entries again; the snapshot they call for is refused.
    let core = &mut c.nodes.get_mut(&f).unwrap().core;
    let recovered = GroupDesc { group: G, voters: vec![], learners: vec![] };
    core.create_group(recovered, Appender::default()).unwrap();
    assert!(core.state_machine(G).unwrap().applied >= 8, "replayed");
    assert_eq!(core.snapshot_index(G), Some(0), "refused for lack of room");
    // Once the disk has room again, the next entries take one.
    c.ticks(150);
    for i in 0..4u8 {
        c.propose(l, &[b'n', i]).unwrap();
        c.ticks(1);
    }
    c.ticks(5);
    assert!(c.nodes[&f].core.snapshot_index(G).unwrap() > 0);
    assert_eq!(c.sm(f).data, c.sm(l).data);
}

#[test]
fn a_leader_snapshot_the_disk_has_no_room_for_stops_the_group() {
    let (mut c, _, f) = behind();
    // Restarted: the first write after a restart reserves a new log segment.
    c.restart(f);
    let recovered = GroupDesc { group: G, voters: vec![], learners: vec![] };
    c.nodes.get_mut(&f).unwrap().core.create_group(recovered, Appender::default()).unwrap();
    c.settle();
    fill_disk(&mut c, f);
    c.heal();
    for _ in 0..50 {
        c.tick();
        if c.sent.iter().any(|(_, to, fr)| *to == f && matches!(fr, PeerFrame::SnapshotChunk(_))) {
            break;
        }
    }
    c.ticks(2);
    let now = c.now;
    let core = &mut c.nodes.get_mut(&f).unwrap().core;
    assert_eq!(core.snapshot_index(G), Some(0), "not installed");
    assert_eq!(core.read(G, ReadMode::Lease, now), Err(OpError::Unavailable), "stopped");
}

/// A state machine that reports the state it is told to: the index it holds, whether it halted
/// and whether the data of its snapshots is on this node.
#[derive(Default)]
struct Fixed {
    state: u64,
    halted: bool,
    missing: bool,
}

impl StateMachine for Fixed {
    type Output = u64;

    fn apply(&mut self, _: &[u8], ctx: &ApplyCtx) -> Result<u64, ApplyError> {
        Ok(ctx.index)
    }

    fn skip(&mut self, _: u64) {}

    fn snapshot(&self) -> Vec<u8> {
        Vec::new()
    }

    fn restore(&mut self, _: &[u8]) -> Result<(), RestoreError> {
        Ok(())
    }

    fn has_data_for(&self, _: &[u8]) -> bool {
        !self.missing
    }

    fn halted(&self) -> bool {
        self.halted
    }

    fn applied_index(&self) -> u64 {
        self.state
    }
}

/// What a follower of a settled cluster hands over of `G` after four entries.
fn export() -> GroupImport {
    let mut c = Cluster::new(3);
    let l = c.elect();
    for i in 0..4u8 {
        c.propose(l, &[b'a' + i]).unwrap();
        c.ticks(1);
    }
    c.ticks(3);
    let f = c.others(l)[0];
    c.nodes.get_mut(&f).unwrap().core.export_group(G).unwrap()
}

/// A core of node 1 with `G` imported and opened again over `sm`; its disk is run only if `run`.
fn imported<S: StateMachine>(sm: S, run: bool) -> (MultiRaftCore<S>, MemDisk, GroupImport) {
    let imp = export();
    let mut disk = MemDisk::new();
    let (store, _) = CoreStore::recover(&StorageConfig::default(), 1, &mut disk).unwrap();
    let mut core = MultiRaftCore::new(CoreConfig { core: 1, ..cfg(1) }, store, 9);
    core.import_group(G, imp.clone()).unwrap();
    run_disk(&mut core, &mut disk, Nanos::ZERO);
    assert_eq!(core.take_durable(), vec![Durable::Imported(G)]);
    let recovered = GroupDesc { group: G, voters: vec![], learners: vec![] };
    core.create_group(recovered, sm).unwrap();
    if run {
        run_disk(&mut core, &mut disk, Nanos::ZERO);
    }
    (core, disk, imp)
}

#[test]
fn a_halted_group_still_reading_its_snapshot_is_not_exported() {
    let sm = Fixed { halted: true, ..Fixed::default() };
    let (mut core, ..) = imported(sm, false);
    assert_eq!(core.export_group(G), Err(OpError::Busy));
}

#[test]
fn a_halted_group_waiting_for_the_data_of_its_snapshot_is_not_exported() {
    let sm = Fixed { halted: true, missing: true, ..Fixed::default() };
    let (mut core, ..) = imported(sm, true);
    assert!(core.awaited_snapshot(G).is_some());
    assert_eq!(core.export_group(G), Err(OpError::Busy));
}

#[test]
fn a_group_whose_state_is_outside_its_log_is_not_exported() {
    // Below the snapshot that starts its log.
    let (mut core, ..) = imported(Fixed { state: 1, ..Fixed::default() }, true);
    assert_eq!(core.export_group(G), Err(OpError::Unavailable));
    assert!(core.status(G).is_some(), "still open");
    // Past its last entry.
    let (mut core, ..) = imported(Fixed { state: 1_000, ..Fixed::default() }, true);
    assert_eq!(core.export_group(G), Err(OpError::Unavailable));
}

#[test]
fn no_snapshot_is_taken_now_of_a_state_waiting_for_its_data_or_past_the_log() {
    // A range whose LSM holds more than its log replayed: its state is past its snapshot.
    let (mut core, ..) = imported(Appender { applied: 1_000, ..Appender::default() }, false);
    assert!(!core.snapshot_now(G), "its own snapshot is still being read");
    let sm = Appender { applied: 1_000, ..Appender::default() };
    sm.missing_data.set(true);
    let (mut core, ..) = imported(sm, true);
    assert!(core.awaited_snapshot(G).is_some());
    assert!(!core.snapshot_now(G), "its own snapshot waits for its data");
    let (mut core, _, imp) = imported(Fixed { state: 1_000, ..Fixed::default() }, true);
    assert!(!core.snapshot_now(G), "the log has no entry at its state");
    assert_eq!(core.snapshot_index(G), Some(imp.index));
}

#[test]
fn an_open_group_is_retired_with_its_log() {
    let (mut core, mut disk, _) = imported(Fixed::default(), true);
    core.retire_group(G).unwrap();
    run_disk(&mut core, &mut disk, Nanos::ZERO);
    assert_eq!(core.take_durable(), vec![Durable::Removed(G)]);
    assert!(core.status(G).is_none() && core.store().group(G).is_none());
}
