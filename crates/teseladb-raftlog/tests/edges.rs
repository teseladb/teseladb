//! Edge cases of `CoreStore`: races between batches and group changes, results nobody asked for,
//! and the errors of the log API.
mod common;

use common::*;
use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::{
    Completions, CoreStore, Disk, FileId, HardState, IoDone, IoOp, LogEntry, LogGap, MemDisk,
    SimDisk, SimDiskConfig, SnapshotMeta, StorageError, Ticket,
};
use teseladb_types::{Bytes, Durability, GroupId, Nanos};

/// Runs only the first I/O operation (leaves the batch in flight).
fn start_batch(s: &mut CoreStore, disk: &mut dyn Disk, now: Nanos) {
    let op = s.poll_io(now).expect("operation");
    let d = disk.execute(op);
    s.on_io_done(d, now);
}

fn entry(index: u64) -> LogEntry {
    LogEntry { index, term: 1, kind: EntryKind::Normal, data: Bytes::from_vec(vec![1; 10]) }
}

#[test]
fn completions_is_empty_checks_every_list() {
    assert!(Completions::default().is_empty());
    let t = Completions { tickets: vec![(Ticket(1), Ok(()))], ..Default::default() };
    assert!(!t.is_empty());
    let sn = Completions {
        snapshots: vec![(teseladb_raftlog::ReadToken(1), Err(StorageError::DiskFull))],
        ..Default::default()
    };
    assert!(!sn.is_empty());
}

#[test]
fn unknown_completions_are_ignored() {
    let cfg = small_cfg();
    let mut disk = MemDisk::new();
    let mut s = recover(&cfg, &mut disk);
    let mut now = Nanos::ZERO;
    s.create_group(GroupId(1), Bytes::new(), Durability::Strict).unwrap();
    let _ = s.poll_io(now).unwrap();
    // A result for something that was not requested changes nothing.
    let c = s.on_io_done(IoDone::Ok(IoOp::Close { file: FileId(4242) }), now);
    assert!(c.is_empty());
    let c = s.on_io_done(IoDone::Read { token: 999_999, data: Ok(Bytes::new()) }, now);
    assert!(c.is_empty());
    // The handed-out operation never got its answer: a clean restart recovers the state.
    drop(s);
    let mut s = recover(&cfg, &mut disk);
    let t = s.create_group(GroupId(2), Bytes::new(), Durability::Strict).unwrap();
    let c = settle(&mut s, &mut disk, &mut now);
    assert_eq!(ok_tickets(&c), vec![t]);
}

#[test]
fn first_batch_without_space_retries_every_create() {
    let cfg = small_cfg();
    let mut disk = SimDisk::new(SimDiskConfig { capacity: 1000, ..Default::default() });
    let mut s = CoreStore::recover(&cfg, 0, &mut disk).unwrap().0;
    let mut now = Nanos::ZERO;
    // The first WAL segment does not fit.
    let t = s.create_group(GroupId(1), Bytes::new(), Durability::Strict).unwrap();
    let c = s.run_io(&mut disk, now);
    assert!(c.tickets.is_empty());
    assert!(s.is_disk_full());
    disk.set_capacity(u64::MAX);
    now = now + teseladb_raftlog::DISK_FULL_RETRY;
    let c = settle(&mut s, &mut disk, &mut now);
    assert_eq!(ok_tickets(&c), vec![t]);
}

#[test]
fn groups_hard_state_only_and_snapshot_of_a_deleted_group() {
    let cfg = small_cfg();
    let mut disk = MemDisk::new();
    let mut s = recover(&cfg, &mut disk);
    let mut now = Nanos::ZERO;
    let g = GroupId(1);
    s.create_group(g, Bytes::new(), Durability::Strict).unwrap();
    let e = LogEntry { index: 1, term: 1, kind: EntryKind::Normal, data: Bytes::from("x") };
    s.append_raft(g, &[e], None, Durability::Strict).unwrap();
    // Only hard state, no entries.
    let hs = HardState { term: 2, vote: 3, commit: 1 };
    s.append_raft(g, &[], Some(hs), Durability::Strict).unwrap();
    settle(&mut s, &mut disk, &mut now);
    let s2 = recover(&cfg, &mut disk);
    assert_eq!(s2.hard_state(g), Some(hs));
    // Snapshot requested and group deleted before it is durable: the ticket completes anyway.
    let t = s
        .save_snapshot(g, SnapshotMeta { index: 1, term: 1, conf: Bytes::new() }, Bytes::from("s"))
        .unwrap();
    start_batch(&mut s, &mut disk, now);
    s.delete_group(g).unwrap();
    let c = settle(&mut s, &mut disk, &mut now);
    assert!(ok_tickets(&c).contains(&t));
    assert!(s.group(g).is_none());
    let s3 = recover(&cfg, &mut disk);
    assert!(s3.group(g).is_none());
}

#[test]
fn log_api_errors() {
    let cfg = small_cfg();
    let mut disk = MemDisk::new();
    let mut s = recover(&cfg, &mut disk);
    let mut now = Nanos::ZERO;
    let g = GroupId(1);
    let missing = GroupId(99);
    let invalid = |r: Result<Ticket, StorageError>| matches!(r, Err(StorageError::Invalid(_)));
    // Groups that do not exist.
    assert!(invalid(s.delete_group(missing)));
    assert!(invalid(s.append_raft(missing, &[entry(1)], None, Durability::Strict)));
    assert!(invalid(s.truncate_raft(missing, 1)));
    let meta = |index| SnapshotMeta { index, term: 1, conf: Bytes::new() };
    assert!(invalid(s.save_snapshot(missing, meta(1), Bytes::new())));
    assert_eq!(s.entries(missing, 1, 2, u64::MAX), Err(LogGap::NoGroup));
    assert_eq!(s.hard_state(missing), None);
    // A group created twice.
    s.create_group(g, Bytes::new(), Durability::Strict).unwrap();
    assert!(invalid(s.create_group(g, Bytes::new(), Durability::Strict)));
    // Entries that do not follow the log.
    assert!(invalid(s.append_raft(g, &[entry(5)], None, Durability::Strict)));
    let batch: Vec<_> = (1..=10).map(entry).collect();
    s.append_raft(g, &batch, None, Durability::Strict).unwrap();
    settle(&mut s, &mut disk, &mut now);
    // Snapshots must be newer than the latest one, durable or in progress.
    s.save_snapshot(g, meta(5), Bytes::new()).unwrap();
    assert!(invalid(s.save_snapshot(g, meta(5), Bytes::new())));
    assert!(invalid(s.save_snapshot(g, meta(3), Bytes::new())));
    settle(&mut s, &mut disk, &mut now);
    // Truncation cannot reach entries covered by the snapshot.
    assert!(invalid(s.truncate_raft(g, 5)));
    let t = s.truncate_raft(g, 8).unwrap();
    let c = settle(&mut s, &mut disk, &mut now);
    assert!(ok_tickets(&c).contains(&t));
    assert_eq!(s.group(g).unwrap().last_index(), 7);
    assert_eq!(s.group_ids(), vec![g]);
    // The truncation survives a restart.
    let s2 = recover(&cfg, &mut disk);
    assert_eq!(s2.group(g).unwrap().last_index(), 7);
}
