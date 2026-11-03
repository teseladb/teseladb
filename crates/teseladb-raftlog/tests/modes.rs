//! Durability modes, disk full and fail-stop.
mod common;

use common::*;
use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::{
    CoreStore, DISK_FULL_RETRY, Disk, IoDone, IoErrno, IoOp, LogEntry, MemDisk, SimDisk,
    SimDiskConfig, SnapshotMeta, StorageError,
};
use teseladb_types::{Bytes, Durability, GroupId, Nanos, Rng};

fn entry(index: u64) -> LogEntry {
    LogEntry {
        index,
        term: 1,
        kind: EntryKind::Normal,
        data: Bytes::from_vec(vec![index as u8; 50]),
    }
}

fn entry_of(index: u64, len: usize) -> LogEntry {
    LogEntry { index, term: 1, kind: EntryKind::Normal, data: Bytes::from_vec(vec![3; len]) }
}

#[test]
fn memory_and_none_modes() {
    let cfg = small_cfg();
    let mut disk = MemDisk::new();
    let mut s = recover(&cfg, &mut disk);
    let (gm, gn) = (GroupId(1), GroupId(2));
    s.create_group(gm, Bytes::new(), Durability::Strict).unwrap();
    s.create_group(gn, Bytes::new(), Durability::None).unwrap();
    let mut now = Nanos::ZERO;
    settle(&mut s, &mut disk, &mut now);
    let tm = s.append_raft(gm, &[entry(1)], None, Durability::Memory).unwrap();
    let tn = s.append_raft(gn, &[entry(1)], None, Durability::None).unwrap();
    let c = s.take_completions();
    assert_eq!(ok_tickets(&c), vec![tm, tn], "memory and none are acknowledged when encoded");
    // The `memory` flush is scheduled within ≤ 1 ms.
    assert_eq!(s.next_deadline(), Some(now + Nanos::from_millis(1)));
    assert!(s.poll_io(now).is_none(), "not yet due");
    let start = now;
    settle(&mut s, &mut disk, &mut now);
    assert!(now.saturating_since(start) <= Nanos::from_millis(1));
    assert_eq!(s.entries(gn, 1, 2, u64::MAX).unwrap()[0], entry(1), "kept in memory");
    let s2 = recover(&cfg, &mut disk);
    assert_eq!(s2.entries(gm, 1, 2, u64::MAX).unwrap()[0], entry(1));
    assert!(s2.group(gn).is_none(), "none does not survive a restart");
}

#[test]
fn memory_is_persisted_within_one_ms_without_more_traffic() {
    // With only `next_deadline` + `poll_io` (what the core loop does), without more requests,
    // what was written in `memory` survives a crash after 1 ms.
    let cfg = small_cfg();
    let mut rng = Rng::new(1);
    for round in 0..50u64 {
        let mut disk =
            SimDisk::new(SimDiskConfig { torn_write_probability: 0.5, ..Default::default() });
        let mut s = CoreStore::recover(&cfg, 0, &mut disk).unwrap().0;
        let start = Nanos::from_micros(round * 37);
        s.on_tick(start);
        s.create_group(GroupId(1), Bytes::new(), Durability::Memory).unwrap();
        let e = entry_of(1, 1 + round as usize * 50);
        s.append_raft(GroupId(1), std::slice::from_ref(&e), None, Durability::Memory).unwrap();
        let deadline = s.next_deadline().expect("scheduled flush");
        assert!(deadline.saturating_since(start) <= Nanos::from_millis(1));
        let mut now = start;
        while let Some(d) = s.next_deadline() {
            now = d.max(now);
            s.on_tick(now);
            s.run_io(&mut disk, now);
        }
        assert!(now.saturating_since(start) <= Nanos::from_millis(1), "{now:?}");
        disk.crash(&mut rng);
        let s = CoreStore::recover(&cfg, 0, &mut disk).unwrap().0;
        assert_eq!(s.entries(GroupId(1), 1, 2, u64::MAX).unwrap()[0], e);
    }
}

#[test]
fn none_starts_empty() {
    let cfg = small_cfg();
    let mut disk = MemDisk::new();
    let mut s = recover(&cfg, &mut disk);
    let mut now = Nanos::ZERO;
    let g = GroupId(3);
    s.create_group(g, Bytes::new(), Durability::None).unwrap();
    let entries: Vec<_> = (1..=50).map(|i| entry_of(i, 1000)).collect();
    s.append_raft(g, &entries, None, Durability::None).unwrap();
    settle(&mut s, &mut disk, &mut now);
    assert_eq!(s.entries(g, 1, 51, u64::MAX).unwrap().len(), 50);
    assert!(disk.list(&teseladb_raftlog::paths::wal_dir(0)).unwrap_or_default().is_empty());
    let s = recover(&cfg, &mut disk);
    assert!(s.group_ids().is_empty());
}

#[test]
fn disk_full_writes_nothing_and_retries() {
    let cfg = small_cfg();
    let mut disk = SimDisk::new(SimDiskConfig { capacity: 4 << 20, ..Default::default() });
    let mut s = CoreStore::recover(&cfg, 0, &mut disk).unwrap().0;
    let mut now = Nanos::ZERO;
    let g = GroupId(1);
    s.create_group(g, Bytes::new(), Durability::Strict).unwrap();
    s.append_raft(g, &[entry(1)], None, Durability::Strict).unwrap();
    settle(&mut s, &mut disk, &mut now);
    // Fill almost everything: the snapshot file of the next batch does not fit.
    let used = disk.used();
    disk.set_capacity(used + 1000);
    let meta = SnapshotMeta { index: 1, term: 1, conf: Bytes::new() };
    let t = s.save_snapshot(g, meta, Bytes::from_vec(vec![9; 64 << 10])).unwrap();
    // Without `settle`, which would advance the clock to every retry.
    let c = s.run_io(&mut disk, now);
    assert!(c.tickets.is_empty(), "nothing is acknowledged: {c:?}");
    assert!(s.is_disk_full());
    assert_eq!(disk.used(), used, "nothing has been written");
    // A stop does not wait for space that may never come.
    assert!(s.writes_settled());
    // Read-only: new writes fail and reads go on.
    assert_eq!(
        s.append_raft(g, &[entry(2)], None, Durability::Strict),
        Err(StorageError::DiskFull)
    );
    assert_eq!(s.entries(g, 1, 2, u64::MAX).unwrap()[0], entry(1));
    // The retry is scheduled at 1 s and, still without space, waits again.
    let retry = s.next_deadline().expect("retry");
    assert_eq!(retry, now + DISK_FULL_RETRY);
    assert!(s.poll_io(retry.saturating_since(Nanos::from_millis(1))).is_none());
    s.run_io(&mut disk, retry);
    assert!(s.is_disk_full());
    assert_eq!(s.next_deadline(), Some(retry + DISK_FULL_RETRY));
    // Space is freed: the retry finishes the batch.
    disk.set_capacity(u64::MAX);
    now = retry + DISK_FULL_RETRY;
    let c = settle(&mut s, &mut disk, &mut now);
    assert!(ok_tickets(&c).contains(&t));
    assert!(!s.is_disk_full());
    assert!(s.writes_settled());
    s.append_raft(g, &[entry(2)], None, Durability::Strict).unwrap();
}

/// Runs the I/O injecting `errno` into the first operation matching `hit`.
fn fail_op(
    s: &mut CoreStore,
    disk: &mut MemDisk,
    now: Nanos,
    hit: impl Fn(&IoOp) -> bool,
    errno: IoErrno,
) -> teseladb_raftlog::Completions {
    let mut all = teseladb_raftlog::Completions::default();
    let mut injected = false;
    while let Some(op) = s.poll_io(now) {
        let done = if !injected && hit(&op) {
            injected = true;
            IoDone::Err { op: op.without_data(), errno }
        } else {
            disk.execute(op)
        };
        let c = s.on_io_done(done, now);
        all.tickets.extend(c.tickets);
    }
    assert!(injected, "the operation was never reached");
    all
}

#[test]
fn sync_error_is_fail_stop_for_this_and_later_batches() {
    let cfg = small_cfg();
    let mut disk = MemDisk::new();
    let mut s = recover(&cfg, &mut disk);
    let now = Nanos::ZERO;
    let g = GroupId(1);
    s.create_group(g, Bytes::new(), Durability::Strict).unwrap();
    let mut now2 = now;
    settle(&mut s, &mut disk, &mut now2);
    // Several batches: the first fails to sync; neither it nor the queued ones are acknowledged.
    let mut tickets = Vec::new();
    for i in 1..=30 {
        tickets.push(s.append_raft(g, &[entry_of(i, 3000)], None, Durability::Strict).unwrap());
    }
    let c = fail_op(&mut s, &mut disk, now, |op| matches!(op, IoOp::Sync { .. }), IoErrno::Io);
    assert_eq!(c.tickets.len(), tickets.len());
    assert!(c.tickets.iter().all(|(_, r)| r.is_err()), "{c:?}");
    assert!(s.is_failed());
    // Nothing else is accepted or emitted; no `fsync` retry.
    assert_eq!(
        s.append_raft(g, &[entry(31)], None, Durability::Strict),
        Err(StorageError::CoreFailed)
    );
    assert!(s.poll_io(now + Nanos::from_secs(10)).is_none());
    assert_eq!(s.next_deadline(), None);
    // A stop has nothing to wait for: none of it will reach the disk.
    assert!(s.writes_settled());
}

#[test]
fn writes_settle_once_none_waits_behind_the_batch_in_flight() {
    let cfg = small_cfg();
    let mut disk = MemDisk::new();
    let mut s = recover(&cfg, &mut disk);
    let mut now = Nanos::ZERO;
    let g = GroupId(1);
    s.create_group(g, Bytes::new(), Durability::Strict).unwrap();
    settle(&mut s, &mut disk, &mut now);
    assert!(s.writes_settled());
    // A batch goes out and is in flight; the next entry waits for it in the open batch.
    s.append_raft(g, &[entry(1)], None, Durability::Strict).unwrap();
    assert!(!s.writes_settled(), "an open batch");
    let first = s.poll_io(now).expect("the first batch goes out");
    s.append_raft(g, &[entry(2)], None, Durability::Strict).unwrap();
    assert!(!s.writes_settled(), "an entry behind the batch in flight");
    s.on_io_done(disk.execute(first), now);
    settle(&mut s, &mut disk, &mut now);
    assert!(s.writes_settled());
    // Both are on disk.
    let s2 = recover(&cfg, &mut disk);
    assert_eq!(s2.entries(g, 1, 3, u64::MAX).unwrap(), &[entry(1), entry(2)]);
    // `memory` writes count too: they are acknowledged before they reach the disk.
    s.append_raft(g, &[entry(3)], None, Durability::Memory).unwrap();
    assert!(!s.writes_settled());
    settle(&mut s, &mut disk, &mut now);
    assert!(s.writes_settled());
}

#[test]
fn write_error_and_late_no_space_are_fail_stop_too() {
    // ENOSPC outside the reservation (in `WriteAt` or `Sync`) is not "disk full": the core stops.
    for (name, errno) in
        [("write", IoErrno::Io), ("sync", IoErrno::NoSpace), ("write", IoErrno::NoSpace)]
    {
        let cfg = small_cfg();
        let mut disk = MemDisk::new();
        let mut s = recover(&cfg, &mut disk);
        let now = Nanos::ZERO;
        let t = s.create_group(GroupId(1), Bytes::new(), Durability::Strict).unwrap();
        let c = fail_op(
            &mut s,
            &mut disk,
            now,
            |op| match name {
                "write" => matches!(op, IoOp::WriteAt { .. }),
                _ => matches!(op, IoOp::Sync { .. }),
            },
            errno,
        );
        assert_eq!(c.tickets.len(), 1);
        assert_eq!(c.tickets[0].0, t);
        assert!(c.tickets[0].1.is_err());
        assert!(s.is_failed(), "{name} {errno:?}");
        assert!(!s.is_disk_full());
    }
}
