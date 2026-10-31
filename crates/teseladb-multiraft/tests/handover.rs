//! 007 T020: a group handed over to another core of its node (FR-011, `CoreRelocate.tla`). A
//! halted state machine is handed no more committed entries and takes no more; the old core closes the group and
//! exports its Raft state, its state and the entries past it; the target core writes the group
//! whole to its log and, once the old copy's removal is durable, opens it and goes on with the
//! same term and vote, the entries the old core had not applied included.

mod common;

use common::*;
use teseladb_multiraft::{CoreConfig, Durable, GroupDesc, MultiRaftCore, OpError, PeerFrame};
use teseladb_raftlog::{CoreStore, Disk, IoErrno, MemDisk, StorageConfig};
use teseladb_types::{GroupId, Nanos, NodeId};

/// The follower of a settled cluster of three, with `n` entries applied everywhere.
fn cluster_with(n: u8) -> (Cluster, NodeId, NodeId) {
    let mut c = Cluster::new(3);
    let l = c.elect();
    for i in 0..n {
        c.propose(l, &[b'a' + i]).unwrap();
        c.ticks(1);
    }
    c.ticks(3);
    let f = c.others(l)[0];
    (c, l, f)
}

/// Runs a core's disk until nothing is left.
fn run_disk(core: &mut MultiRaftCore<Appender>, disk: &mut MemDisk, c: &Cluster) {
    for _ in 0..10_000 {
        let Some(op) = core.poll_io(c.now) else { return };
        let d = disk.execute(op);
        core.on_io_done(d, c.now);
    }
    panic!("the disk never empties");
}

#[test]
fn a_halted_state_machine_is_handed_no_more_entries() {
    let (mut c, l, f) = cluster_with(2);
    let at = c.sm(f).applied + 1;
    c.sm(f).halt_at.set(at);
    for i in 0..4u8 {
        c.propose(l, &[b'x' + i]).unwrap();
        c.ticks(1);
    }
    c.ticks(3);
    assert_eq!(c.sm(f).applied, at, "it stops at the entry that halted it");
    assert!(c.sm(l).applied > at);
    // What it appended before it halted stays in its log, which goes with the group.
    let log = c.nodes[&f].core.store().group(G).unwrap();
    assert!(log.last_index() >= at);
}

#[test]
fn a_group_handed_to_another_core_goes_on_there_with_its_raft_state() {
    let (mut c, l, f) = cluster_with(2);
    let at = c.sm(f).applied + 1;
    c.sm(f).halt_at.set(at);
    for i in 0..3u8 {
        c.propose(l, &[b'x' + i]).unwrap();
        c.ticks(1);
    }
    c.ticks(3);
    // The old core stops the group and exports it.
    let old = c.nodes.get_mut(&f).unwrap();
    let hard = old.core.store().hard_state(G).unwrap();
    let imp = old.core.export_group(G).unwrap();
    assert!(old.core.status(G).is_none(), "closed");
    assert_eq!((imp.index, imp.hard), (at, hard));
    assert!(imp.entries.first().is_none_or(|e| e.index == at + 1), "the tail past `at`");
    // The target core writes it whole and says so once it is durable.
    let mut disk = MemDisk::new();
    let (store, _) = CoreStore::recover(&StorageConfig::default(), 1, &mut disk).unwrap();
    let mut target = MultiRaftCore::new(CoreConfig { core: 1, ..cfg(f.0) }, store, 9);
    target.import_group(G, imp.clone()).unwrap();
    assert_eq!(target.import_group(G, imp), Err(OpError::GroupExists));
    run_disk(&mut target, &mut disk, &c);
    assert_eq!(target.take_durable(), vec![Durable::Imported(G)]);
    // The old copy goes, and says so once that is durable.
    let old = c.nodes.get_mut(&f).unwrap();
    old.core.retire_group(G).unwrap();
    for _ in 0..1_000 {
        let Some(op) = old.core.poll_io(c.now) else { break };
        let d = old.disk.execute(op);
        old.core.on_io_done(d, c.now);
    }
    assert_eq!(old.core.take_durable(), vec![Durable::Removed(G)]);
    assert!(old.core.store().group(G).is_none());
    // The target core, restarted over its disk, opens the group and takes the node's place.
    let (store, _) = CoreStore::recover(&StorageConfig::default(), 1, &mut disk).unwrap();
    let mut target = MultiRaftCore::new(CoreConfig { core: 1, ..cfg(f.0) }, store, 10);
    assert_eq!(target.recovered_groups(), vec![G]);
    let recovered = GroupDesc { group: G, voters: vec![], learners: vec![] };
    target.create_group(recovered, Appender::default()).unwrap();
    let node = c.nodes.get_mut(&f).unwrap();
    node.core = target;
    node.disk = disk;
    c.settle();
    assert_eq!(c.nodes[&f].core.status(G).unwrap().hard_state.term, hard.term);
    for i in 0..3u8 {
        let leader = c.leader().unwrap();
        c.propose(leader, &[b'p' + i]).unwrap();
        c.ticks(1);
    }
    c.ticks(5);
    let leader = c.leader().unwrap();
    assert_eq!(c.sm(f).data, c.sm(leader).data, "it applied the tail and what came after");
    let _ = l;
}

#[test]
fn a_group_imported_and_not_started_can_be_retired() {
    let (mut c, _, f) = cluster_with(2);
    let old = c.nodes.get_mut(&f).unwrap();
    assert_eq!(old.core.export_group(GroupId(99)), Err(OpError::GroupNotFound));
    let imp = old.core.export_group(G).unwrap();
    assert_eq!(old.core.export_group(G), Err(OpError::GroupNotFound), "closed by the export");
    let mut disk = MemDisk::new();
    let (store, _) = CoreStore::recover(&StorageConfig::default(), 1, &mut disk).unwrap();
    let mut target = MultiRaftCore::new(CoreConfig { core: 1, ..cfg(f.0) }, store, 9);
    target.import_group(G, imp).unwrap();
    run_disk(&mut target, &mut disk, &c);
    assert_eq!(target.take_durable(), vec![Durable::Imported(G)]);
    target.retire_group(G).unwrap();
    run_disk(&mut target, &mut disk, &c);
    assert_eq!(target.take_durable(), vec![Durable::Removed(G)]);
    assert!(target.store().group(G).is_none());
    assert_eq!(target.retire_group(G), Err(OpError::GroupNotFound));
}

#[test]
fn a_group_waiting_for_the_data_of_its_snapshot_is_not_exported() {
    let (mut c, _, f) = cluster_with(2);
    let imp = c.nodes.get_mut(&f).unwrap().core.export_group(G).unwrap();
    let mut disk = MemDisk::new();
    let (store, _) = CoreStore::recover(&StorageConfig::default(), 1, &mut disk).unwrap();
    let mut target = MultiRaftCore::new(CoreConfig { core: 1, ..cfg(f.0) }, store, 9);
    target.import_group(G, imp).unwrap();
    run_disk(&mut target, &mut disk, &c);
    // Opened in the same run, its state machine has not the data of the snapshot yet.
    let sm = Appender::default();
    sm.missing_data.set(true);
    let recovered = GroupDesc { group: G, voters: vec![], learners: vec![] };
    target.create_group(recovered, sm).unwrap();
    run_disk(&mut target, &mut disk, &c);
    assert_eq!(target.export_group(G), Err(OpError::Busy));
    assert!(target.status(G).is_some(), "still open");
}

#[test]
fn frames_for_a_group_another_core_holds_are_handed_to_it() {
    let (mut c, l, f) = cluster_with(2);
    // The follower's group goes to core 1 of its node, which opens it.
    let imp = c.nodes.get_mut(&f).unwrap().core.export_group(G).unwrap();
    let mut disk = MemDisk::new();
    let (store, _) = CoreStore::recover(&StorageConfig::default(), 1, &mut disk).unwrap();
    let mut target = MultiRaftCore::new(CoreConfig { core: 1, ..cfg(f.0) }, store, 9);
    target.import_group(G, imp).unwrap();
    run_disk(&mut target, &mut disk, &c);
    assert_eq!(target.take_durable(), vec![Durable::Imported(G)]);
    let recovered = GroupDesc { group: G, voters: vec![], learners: vec![] };
    target.create_group(recovered, Appender::default()).unwrap();
    run_disk(&mut target, &mut disk, &c);
    let before = target.store().group(G).unwrap().last_index();
    // The leader's frames still reach core 0 of the follower's node: they are strays there.
    for i in 0..3u8 {
        c.propose(l, &[b'x' + i]).unwrap();
        c.ticks(1);
    }
    let strays = c.nodes.get_mut(&f).unwrap().core.take_strays();
    assert!(!strays.is_empty(), "the frames of a group the core does not have are kept");
    for (from, frame) in strays {
        assert_eq!(from, l);
        target.on_stray(from, frame, c.now);
    }
    run_disk(&mut target, &mut disk, &c);
    assert!(target.store().group(G).unwrap().last_index() > before, "core 1 took the entries");
    // A stray of a group the second core does not have either goes nowhere more.
    let lost = teseladb_multiraft::PeerFrame::SnapshotAck {
        group: GroupId(99),
        index: 1,
        offset: 0,
        ok: true,
    };
    target.on_stray(l, lost.clone(), c.now);
    assert!(target.take_strays().is_empty());
    target.on_peer_message(l, lost, c.now);
    assert_eq!(target.take_strays().len(), 1);
}

/// A single-voter core whose group takes a snapshot every two entries.
fn single(disk: &mut MemDisk) -> MultiRaftCore<Appender> {
    let (store, _) = CoreStore::recover(&StorageConfig::default(), 0, disk).unwrap();
    let mut core = MultiRaftCore::new(CoreConfig { snapshot_entries: 2, ..cfg(1) }, store, 1);
    let desc = GroupDesc { group: G, voters: vec![NodeId(1)], learners: vec![] };
    core.create_group(desc, Appender::default()).unwrap();
    core
}

/// Proposes `n` entries on a single-voter core, its disk run whole after each.
fn fill(core: &mut MultiRaftCore<Appender>, disk: &mut MemDisk, n: u8, now: &mut Nanos) {
    for i in 0..n {
        *now = *now + Nanos::from_millis(10);
        core.on_tick(*now, now.0 / 1_000_000);
        let _ = core.propose(G, &[b'a' + i], *now);
        while let Some(op) = core.poll_io(*now) {
            let d = disk.execute(op);
            core.on_io_done(d, *now);
        }
        let _ = core.drain_actions();
    }
}

#[test]
fn a_group_closed_while_its_snapshot_is_written_reopens_with_it() {
    // Every point at which the group may be closed while the core writes a snapshot of it.
    for cut in 0..40 {
        let mut disk = MemDisk::new();
        let mut core = single(&mut disk);
        let mut now = Nanos::ZERO;
        fill(&mut core, &mut disk, 3, &mut now);
        assert!(core.store().group(G).unwrap().snapshot_index > 0, "snapshots are taken");
        // Two more entries: the core takes a snapshot; only `cut` disk operations finish.
        for i in 0..2u8 {
            let _ = core.propose(G, &[b'x' + i], now);
        }
        for _ in 0..cut {
            let Some(op) = core.poll_io(now) else { break };
            let d = disk.execute(op);
            core.on_io_done(d, now);
            let _ = core.drain_actions();
        }
        // One more operation is on its way to the disk while the group closes and reopens.
        let held = core.poll_io(now);
        if core.close_group(G).is_ok() {
            let desc = GroupDesc { group: G, voters: vec![], learners: vec![] };
            core.create_group(desc, Appender::default()).unwrap();
        }
        if let Some(op) = held {
            let d = disk.execute(op);
            core.on_io_done(d, now);
        }
        fill(&mut core, &mut disk, 6, &mut now);
        assert!(core.export_group(G).is_ok(), "cut {cut}");
    }
}

#[test]
fn a_halted_replica_takes_no_snapshot() {
    let mut c = Cluster::with(3, |i| CoreConfig { snapshot_entries: 4, ..cfg(i) });
    let l = c.elect();
    c.propose(l, b"a").unwrap();
    c.ticks(3);
    let f = c.others(l)[0];
    let at = c.sm(f).applied + 1;
    c.sm(f).halt_at.set(at);
    c.propose(l, b"b").unwrap();
    c.ticks(3);
    assert_eq!(c.sm(f).applied, at);
    // Cut off, it falls behind the leader's snapshots; back, it is offered one.
    c.isolate(f);
    for i in 0..12u8 {
        c.propose(l, &[b'c' + i]).unwrap();
        c.ticks(1);
    }
    c.heal();
    c.ticks(20);
    assert_eq!(c.sm(f).applied, at, "its state stays the one it hands over");
    // What it received of the snapshot does not hold its hand-over back.
    let imp = c.nodes.get_mut(&f).unwrap().core.export_group(G).unwrap();
    assert_eq!(imp.index, at);
}

#[test]
fn a_halted_follower_takes_no_more_entries_and_casts_no_vote() {
    let (mut c, l, f) = cluster_with(2);
    let at = c.sm(f).applied + 1;
    c.sm(f).halt_at.set(at);
    c.propose(l, b"h").unwrap();
    c.ticks(3);
    let last = c.nodes[&f].core.store().group(G).unwrap().last_index();
    for i in 0..4u8 {
        c.propose(l, &[b'x' + i]).unwrap();
        c.ticks(1);
    }
    c.ticks(3);
    assert_eq!(c.nodes[&f].core.store().group(G).unwrap().last_index(), last);
    // With the leader cut off, the halted follower does not help elect another one.
    let term = c.nodes[&f].core.status(G).unwrap().hard_state.term;
    c.isolate(l);
    c.ticks(40);
    assert_eq!(c.nodes[&f].core.status(G).unwrap().hard_state.term, term);
    let core = &mut c.nodes.get_mut(&f).unwrap().core;
    assert!(core.export_group(G).is_ok(), "nothing of it is in flight");
}

#[test]
fn a_core_that_has_the_group_takes_no_import_of_it() {
    let (mut c, _, f) = cluster_with(2);
    let imp = c.nodes.get_mut(&f).unwrap().core.export_group(G).unwrap();
    let mut disk = MemDisk::new();
    let mut core = single(&mut disk);
    assert_eq!(core.import_group(G, imp), Err(OpError::GroupExists));
}

#[test]
fn strays_past_the_limit_are_dropped() {
    let (mut c, l, f) = cluster_with(1);
    let core = &mut c.nodes.get_mut(&f).unwrap().core;
    let ack = PeerFrame::SnapshotAck { group: GroupId(99), index: 1, offset: 0, ok: true };
    for _ in 0..300 {
        core.on_peer_message(l, ack.clone(), c.now);
    }
    assert_eq!(core.take_strays().len(), 256, "the rest is dropped, as the network may");
}

#[test]
fn a_group_whose_entries_are_being_written_does_not_close() {
    let mut disk = MemDisk::new();
    let mut core = single(&mut disk);
    let mut now = Nanos::ZERO;
    fill(&mut core, &mut disk, 1, &mut now);
    core.propose(G, b"w", now).unwrap();
    assert_eq!(core.close_group(G), Err(OpError::Busy), "its entry is on its way to the disk");
    assert_eq!(core.export_group(G), Err(OpError::Busy));
    while let Some(op) = core.poll_io(now) {
        let d = disk.execute(op);
        core.on_io_done(d, now);
    }
    let _ = core.drain_actions();
    assert!(core.close_group(G).is_ok());
}

/// A cluster of three whose follower `f` missed entries past the leader's snapshots.
fn behind() -> (Cluster, NodeId, NodeId) {
    let mut c = Cluster::with(3, |i| CoreConfig { snapshot_entries: 3, ..cfg(i) });
    let l = c.elect();
    c.propose(l, b"a").unwrap();
    c.ticks(3);
    let f = c.others(l)[0];
    c.isolate(f);
    for i in 0..10u8 {
        c.propose(l, &[b'b' + i]).unwrap();
        c.ticks(1);
    }
    c.ticks(3);
    (c, l, f)
}

#[test]
fn a_leader_reading_a_snapshot_to_send_does_not_close_the_group() {
    let (mut c, l, _) = behind();
    c.slow.insert(l);
    c.heal();
    c.ticks(5);
    assert_eq!(c.nodes.get_mut(&l).unwrap().core.close_group(G), Err(OpError::Busy));
    c.slow.clear();
    c.ticks(20);
}

#[test]
fn a_follower_installing_a_snapshot_does_not_close_the_group() {
    let (mut c, _, f) = behind();
    c.slow.insert(f);
    c.heal();
    c.ticks(20);
    let core = &mut c.nodes.get_mut(&f).unwrap().core;
    assert_eq!(core.close_group(G), Err(OpError::Busy), "its snapshot is being made durable");
    c.slow.clear();
    c.ticks(20);
}

#[test]
fn a_halted_group_without_the_data_of_its_own_snapshot_is_not_exported() {
    let (mut c, _, f) = cluster_with(2);
    let imp = c.nodes.get_mut(&f).unwrap().core.export_group(G).unwrap();
    let mut disk = MemDisk::new();
    let (store, _) = CoreStore::recover(&StorageConfig::default(), 1, &mut disk).unwrap();
    let mut target = MultiRaftCore::new(CoreConfig { core: 1, ..cfg(f.0) }, store, 9);
    target.import_group(G, imp).unwrap();
    run_disk(&mut target, &mut disk, &c);
    let sm = Appender::default();
    sm.missing_data.set(true);
    sm.halt_at.set(1);
    let recovered = GroupDesc { group: G, voters: vec![], learners: vec![] };
    target.create_group(recovered, sm).unwrap();
    run_disk(&mut target, &mut disk, &c);
    assert_eq!(target.export_group(G), Err(OpError::Busy));
}

#[test]
fn a_store_that_stopped_takes_no_import_nor_removal() {
    let (mut c, _, f) = cluster_with(2);
    let imp = c.nodes.get_mut(&f).unwrap().core.export_group(G).unwrap();
    let mut disk = MemDisk::new();
    let (store, _) = CoreStore::recover(&StorageConfig::default(), 1, &mut disk).unwrap();
    let mut target = MultiRaftCore::new(CoreConfig { core: 1, ..cfg(f.0) }, store, 9);
    target.import_group(G, imp).unwrap();
    // The first write of the import fails: the store stops and the import never becomes durable.
    disk.inject_error("sync", IoErrno::Io);
    run_disk(&mut target, &mut disk, &c);
    assert!(target.take_durable().is_empty());
    assert_eq!(target.retire_group(G), Err(OpError::Unavailable));
}
