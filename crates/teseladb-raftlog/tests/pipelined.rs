//! Several batches of a core in flight (`StorageConfig::max_inflight_batches`): the next batch is
//! written while the one before waits for its sync, and the batches become durable in order.

mod common;

use common::*;
use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::{
    CoreStore, Disk, IoDone, IoErrno, IoOp, LogEntry, MemDisk, StorageConfig, Ticket,
};
use teseladb_types::{Bytes, Durability, GroupId, Nanos};

const G: GroupId = GroupId(1);

fn entry(index: u64, len: usize) -> LogEntry {
    LogEntry { index, term: 1, kind: EntryKind::Normal, data: Bytes::from_vec(vec![7; len]) }
}

fn pipelined(depth: usize) -> StorageConfig {
    StorageConfig { max_inflight_batches: depth, ..small_cfg() }
}

/// A store with group `G` created and on disk.
fn ready(cfg: &StorageConfig, disk: &mut MemDisk) -> CoreStore {
    let mut s = recover(cfg, disk);
    s.create_group(G, Bytes::new(), Durability::Strict).expect("create_group succeeds");
    let mut now = Nanos::ZERO;
    settle(&mut s, disk, &mut now);
    s
}

/// Runs the operations `poll_io` gives until it gives a sync, which it returns without running.
fn up_to_sync(s: &mut CoreStore, disk: &mut MemDisk) -> (IoOp, Vec<Ticket>) {
    let mut acked = Vec::new();
    loop {
        let op = s.poll_io(Nanos::ZERO).expect("an operation of the batch");
        if matches!(op, IoOp::Sync { .. }) {
            return (op, acked);
        }
        let c = s.on_io_done(disk.execute(op), Nanos::ZERO);
        acked.extend(c.tickets.into_iter().map(|(t, _)| t));
    }
}

fn append(s: &mut CoreStore, index: u64) -> Ticket {
    s.append_raft(G, &[entry(index, 100)], None, Durability::Strict).expect("append_raft succeeds")
}

#[test]
fn the_next_batch_is_written_while_the_one_before_waits_for_its_sync() {
    let cfg = pipelined(2);
    let mut disk = MemDisk::new();
    let mut s = ready(&cfg, &mut disk);
    let a = append(&mut s, 1);
    let (sync_a, _) = up_to_sync(&mut s, &mut disk);
    let b = append(&mut s, 2);
    let (sync_b, acked) = up_to_sync(&mut s, &mut disk);
    assert!(acked.is_empty(), "nothing is durable yet");
    assert!(s.poll_io(Nanos::ZERO).is_none(), "two batches in flight, no more");
    // The later sync ends first: its batch waits for the one before.
    let c = s.on_io_done(disk.execute(sync_b), Nanos::ZERO);
    assert!(c.tickets.is_empty(), "{c:?}");
    let c = s.on_io_done(disk.execute(sync_a), Nanos::ZERO);
    assert_eq!(c.tickets, vec![(a, Ok(())), (b, Ok(()))]);
    assert!(s.writes_settled());
    let s2 = recover(&cfg, &mut disk);
    assert_eq!(
        s2.entries(G, 1, 3, u64::MAX).expect("entries succeeds"),
        &[entry(1, 100), entry(2, 100)]
    );
}

#[test]
fn with_one_batch_in_flight_the_next_waits_for_the_sync() {
    let cfg = pipelined(1);
    let mut disk = MemDisk::new();
    let mut s = ready(&cfg, &mut disk);
    append(&mut s, 1);
    let (sync_a, _) = up_to_sync(&mut s, &mut disk);
    append(&mut s, 2);
    assert!(s.poll_io(Nanos::ZERO).is_none(), "the next batch waits");
    s.on_io_done(disk.execute(sync_a), Nanos::ZERO);
    assert!(matches!(s.poll_io(Nanos::ZERO), Some(IoOp::WriteAt { .. })));
}

#[test]
fn a_failed_sync_fails_its_batch_and_the_later_ones_in_flight() {
    let cfg = pipelined(2);
    let mut disk = MemDisk::new();
    let mut s = ready(&cfg, &mut disk);
    let a = append(&mut s, 1);
    let (sync_a, _) = up_to_sync(&mut s, &mut disk);
    let b = append(&mut s, 2);
    let (sync_b, _) = up_to_sync(&mut s, &mut disk);
    let c = s.on_io_done(disk.execute(sync_b), Nanos::ZERO);
    assert!(c.tickets.is_empty());
    let failed = IoDone::Err { op: sync_a.without_data(), errno: IoErrno::Io };
    let c = s.on_io_done(failed, Nanos::ZERO);
    let tickets: Vec<Ticket> = c.tickets.iter().map(|(t, _)| *t).collect();
    assert_eq!(tickets, vec![a, b]);
    assert!(c.tickets.iter().all(|(_, r)| r.is_err()), "{c:?}");
    assert!(s.is_failed() && s.writes_settled());
    assert!(s.poll_io(Nanos::from_secs(10)).is_none());
}

#[test]
fn a_batch_that_opens_a_segment_waits_for_every_batch_before_it() {
    let cfg = pipelined(4);
    let mut disk = MemDisk::new();
    let mut s = ready(&cfg, &mut disk);
    // Fill the first segment to near its end, settled.
    let mut index = 1;
    let mut now = Nanos::ZERO;
    while s.metrics().wal_bytes_written + 40_000 < cfg.wal_segment_size {
        s.append_raft(G, &[entry(index, 30_000)], None, Durability::Strict)
            .expect("append_raft succeeds");
        index += 1;
        settle(&mut s, &mut disk, &mut now);
    }
    append(&mut s, index);
    let (sync_a, _) = up_to_sync(&mut s, &mut disk);
    let IoOp::Sync { file: old, .. } = sync_a else { panic!("a sync: {sync_a:?}") };
    // This one does not fit in the segment: it opens the next one only after the batch before.
    s.append_raft(G, &[entry(index + 1, 30_000)], None, Durability::Strict)
        .expect("append_raft succeeds");
    assert!(s.poll_io(Nanos::ZERO).is_none(), "it waits for the batch in flight");
    s.on_io_done(disk.execute(sync_a), Nanos::ZERO);
    // The segment was made ahead, so the batch only writes to it.
    match s.poll_io(Nanos::ZERO) {
        Some(IoOp::WriteAt { file, .. }) => assert_ne!(file, old, "the next segment"),
        other => panic!("a write to the next segment: {other:?}"),
    }
}

/// Appends entries of 30 KB, settled, until the active segment holds at least `bytes`; the
/// operations each settle ran.
fn fill_to(s: &mut CoreStore, disk: &mut MemDisk, index: &mut u64, bytes: u64) -> Vec<IoOp> {
    let mut ran = Vec::new();
    while s.metrics().wal_bytes_written < bytes {
        s.append_raft(G, &[entry(*index, 30_000)], None, Durability::Strict)
            .expect("append_raft succeeds");
        *index += 1;
        while let Some(op) = s.poll_io(Nanos::ZERO) {
            ran.push(op.clone());
            s.on_io_done(disk.execute(op), Nanos::ZERO);
        }
    }
    ran
}

/// Once the active segment is half full the next one is made ahead and its directory synced
/// apart; the batch that opens it then neither creates a file nor syncs the directory, whose
/// sync took as long as the drive's flush while every batch behind it waited (H-049).
#[test]
fn the_next_segment_is_made_ahead_and_opening_it_syncs_no_directory() {
    let cfg = pipelined(4);
    let mut disk = MemDisk::new();
    let mut s = ready(&cfg, &mut disk);
    let mut index = 1;
    let half = fill_to(&mut s, &mut disk, &mut index, cfg.wal_segment_size / 2 + 40_000);
    let creates = half.iter().filter(|op| matches!(op, IoOp::Create { .. })).count();
    assert_eq!(creates, 1, "the spare segment, once: {half:?}");
    assert!(half.iter().any(|op| matches!(op, IoOp::SyncDir { .. })), "synced apart");
    let rest = fill_to(&mut s, &mut disk, &mut index, cfg.wal_segment_size + 40_000);
    let opening: Vec<&IoOp> =
        rest.iter().filter(|op| matches!(op, IoOp::Create { .. } | IoOp::SyncDir { .. })).collect();
    // Past the end of the first segment only the next spare is made, ahead again.
    assert!(opening.len() <= 2, "no file or directory sync to open the segment: {opening:?}");
    let s2 = recover(&cfg, &mut disk);
    assert_eq!(s2.group(G).map(teseladb_raftlog::GroupLog::last_index), Some(index - 1));
}

/// A spare segment that no batch opened is dropped by recovery like any newest segment without
/// batches, and the log is whole.
#[test]
fn a_spare_segment_left_unused_by_a_crash_is_dropped_on_recovery() {
    let cfg = pipelined(4);
    let mut disk = MemDisk::new();
    let mut s = ready(&cfg, &mut disk);
    let mut index = 1;
    fill_to(&mut s, &mut disk, &mut index, cfg.wal_segment_size / 2 + 40_000);
    let wal = std::path::Path::new("core-00/wal");
    let listed = disk.list(wal).expect("the WAL directory");
    assert_eq!(listed.len(), 2, "the active segment and the spare");
    drop(s);
    let s2 = recover(&cfg, &mut disk);
    assert_eq!(s2.group(G).map(teseladb_raftlog::GroupLog::last_index), Some(index - 1));
    assert_eq!(disk.list(wal).expect("the WAL directory").len(), 1, "the spare is gone");
}

#[test]
fn many_batches_in_flight_all_reach_the_disk_in_order() {
    let cfg = pipelined(3);
    let mut disk = MemDisk::new();
    let mut s = ready(&cfg, &mut disk);
    let mut syncs = Vec::new();
    let mut tickets = Vec::new();
    for i in 1..=3 {
        tickets.push(append(&mut s, i));
        syncs.push(up_to_sync(&mut s, &mut disk).0);
    }
    let mut acked = Vec::new();
    for op in syncs.into_iter().rev() {
        let c = s.on_io_done(disk.execute(op), Nanos::ZERO);
        acked.extend(c.tickets.into_iter().map(|(t, _)| t));
    }
    assert_eq!(acked, tickets);
    let s2 = recover(&cfg, &mut disk);
    assert_eq!(s2.group(G).map(teseladb_raftlog::GroupLog::last_index), Some(3));
}

/// Maintenance (deleting an old segment, snapshot or marker and syncing its directory) waits for
/// the batch open when it was asked for, but no batch waits for it: a directory sync takes as
/// long as the drive's flush, and every acknowledgement of the core waited behind it.
#[test]
fn maintenance_goes_out_after_its_batch_and_no_batch_waits_for_it() {
    let cfg = pipelined(2);
    let mut disk = MemDisk::new();
    let mut s = ready(&cfg, &mut disk);
    let marker = teseladb_raftlog::rebuild_marker(s.core());
    let create =
        IoOp::Create { file: teseladb_raftlog::FileId(9_000), path: marker.clone(), prealloc: 0 };
    assert!(matches!(disk.execute(create), IoDone::Ok(_)));
    s.clear_rebuild_marker();
    let a = append(&mut s, 1);
    let (sync_a, _) = up_to_sync(&mut s, &mut disk);
    let c = s.on_io_done(disk.execute(sync_a), Nanos::ZERO);
    assert_eq!(c.tickets, vec![(a, Ok(()))], "acknowledged once synced");
    // Now the maintenance of that batch goes out, one operation at a time.
    let remove = s.poll_io(Nanos::ZERO).expect("the maintenance");
    assert_eq!(remove, IoOp::Remove { path: marker });
    // A later batch is acknowledged while the maintenance is still running.
    let b = append(&mut s, 2);
    let (sync_b, _) = up_to_sync(&mut s, &mut disk);
    let c = s.on_io_done(disk.execute(sync_b), Nanos::ZERO);
    assert_eq!(c.tickets, vec![(b, Ok(()))]);
    assert!(s.poll_io(Nanos::ZERO).is_none(), "one maintenance operation at a time");
    let c = s.on_io_done(disk.execute(remove), Nanos::ZERO);
    assert!(c.tickets.is_empty());
    let sync_dir = s.poll_io(Nanos::ZERO).expect("then its directory");
    assert!(matches!(sync_dir, IoOp::SyncDir { .. }), "{sync_dir:?}");
    assert!(!s.writes_settled(), "maintenance in flight");
    s.on_io_done(disk.execute(sync_dir), Nanos::ZERO);
    assert!(s.writes_settled() && s.poll_io(Nanos::ZERO).is_none());
}
