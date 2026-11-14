//! Atomic snapshots per group and WAL release.
mod common;

use common::*;
use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::{
    CoreStore, Disk, HardState, LogEntry, LogGap, MemDisk, ReleaseProvider, SimDisk, SimDiskConfig,
    SnapshotMeta, StorageConfig, StorageError, paths,
};
use teseladb_types::{Bytes, Durability, GroupId, Nanos, Rng};

fn e(index: u64, len: usize) -> LogEntry {
    LogEntry {
        index,
        term: 1,
        kind: EntryKind::Normal,
        data: Bytes::from_vec(vec![(index % 251) as u8; len]),
    }
}

fn snap(index: u64) -> SnapshotMeta {
    SnapshotMeta { index, term: 1, conf: Bytes::from(format!("conf-{index}")) }
}

fn wal_segments(disk: &mut dyn Disk) -> usize {
    disk.list(&paths::wal_dir(0)).unwrap_or_default().len()
}

#[test]
#[expect(
    clippy::many_single_char_names,
    reason = "one letter per store, disk and group of the case"
)]
fn snapshot_compacts_log_and_survives_restart() {
    let cfg = small_cfg();
    let mut disk = MemDisk::new();
    let mut s = recover(&cfg, &mut disk);
    let mut now = Nanos::ZERO;
    let g = GroupId(1);
    s.create_group(g, Bytes::from("c0"), Durability::Strict).expect("create_group succeeds");
    s.append_raft(
        g,
        &(1..=10).map(|i| e(i, 50)).collect::<Vec<_>>(),
        Some(HardState { term: 1, vote: 2, commit: 8 }),
        Durability::Strict,
    )
    .expect("append_raft succeeds");
    settle(&mut s, &mut disk, &mut now);
    let t = s.save_snapshot(g, snap(8), Bytes::from("estado-8")).expect("save_snapshot succeeds");
    assert!(matches!(
        s.save_snapshot(GroupId(9), snap(1), Bytes::new()),
        Err(StorageError::Invalid(_))
    ));
    let c = settle(&mut s, &mut disk, &mut now);
    assert_eq!(ok_tickets(&c), vec![t]);
    assert_eq!(s.entries(g, 1, 3, u64::MAX), Err(LogGap::Compacted { first: 9 }));
    assert_eq!(s.entries(g, 9, 11, u64::MAX).expect("entries succeeds").len(), 2);
    assert!(matches!(s.save_snapshot(g, snap(8), Bytes::new()), Err(StorageError::Invalid(_))));
    // Reading the snapshot.
    let (meta, tok) = s.load_snapshot(g).expect("load_snapshot succeeds");
    assert_eq!((meta.index, meta.conf.clone()), (8, Bytes::from("conf-8")));
    let c = s.run_io(&mut disk, now);
    assert_eq!(c.snapshots, vec![(tok, Ok((snap(8), Bytes::from("estado-8"))))]);
    assert!(s.load_snapshot(GroupId(9)).is_none());

    let (mut s2, rec) = CoreStore::recover(&cfg, 0, &mut disk).expect("recover succeeds");
    let r = &rec.groups[0];
    assert_eq!(r.snapshot, Some(snap(8)));
    assert_eq!(r.last_index, 10);
    assert_eq!(r.hard_state, HardState { term: 1, vote: 2, commit: 8 });
    assert_eq!(s2.entries(g, 9, 11, u64::MAX).expect("entries succeeds"), &[e(9, 50), e(10, 50)]);
    let (_, tok) = s2.load_snapshot(g).expect("load_snapshot succeeds");
    let c = s2.run_io(&mut disk, now);
    assert_eq!(c.snapshots[0], (tok, Ok((snap(8), Bytes::from("estado-8")))));
}

#[test]
fn keeps_two_snapshots_and_removes_older_ones() {
    let cfg = small_cfg();
    let mut disk = MemDisk::new();
    let mut s = recover(&cfg, &mut disk);
    let mut now = Nanos::ZERO;
    let g = GroupId(3);
    s.create_group(g, Bytes::new(), Durability::Strict).expect("create_group succeeds");
    for round in 1..=5u64 {
        s.append_raft(g, &[e(round, 10)], None, Durability::Strict).expect("append_raft succeeds");
        s.save_snapshot(g, snap(round), Bytes::from(format!("s{round}")))
            .expect("save_snapshot succeeds");
        settle(&mut s, &mut disk, &mut now);
    }
    let snaps = disk.list(&paths::snap_dir(0)).expect("list succeeds");
    assert_eq!(snaps.len(), 2, "{snaps:?}");
    // Deleting the group deletes its snapshots.
    s.delete_group(g).expect("delete_group succeeds");
    settle(&mut s, &mut disk, &mut now);
    assert!(disk.list(&paths::snap_dir(0)).unwrap_or_default().is_empty());
}

#[test]
fn wal_stays_bounded_with_periodic_snapshots() {
    let cfg = small_cfg(); // 1 MiB WAL segments
    let mut disk = MemDisk::new();
    let mut s = recover(&cfg, &mut disk);
    let mut now = Nanos::ZERO;
    let groups: Vec<GroupId> = (0..8).map(GroupId).collect();
    for &g in &groups {
        s.create_group(g, Bytes::new(), Durability::Strict).expect("create_group succeeds");
    }
    let mut max_segments = 0;
    // 40 MiB of entries in total, with a snapshot of each group every ~1 MiB.
    for round in 1..=400u64 {
        for &g in &groups {
            s.append_raft(g, &[e(round, 12_000)], None, Durability::Strict)
                .expect("append_raft succeeds");
            if round % 10 == 0 {
                s.save_snapshot(g, snap(round), Bytes::from("x")).expect("save_snapshot succeeds");
            }
        }
        settle(&mut s, &mut disk, &mut now);
        max_segments = max_segments.max(wal_segments(&mut disk));
    }
    assert!(max_segments <= 4, "the WAL reached {max_segments} segments");
    let (s2, rec) = CoreStore::recover(&cfg, 0, &mut disk).expect("recover succeeds");
    for r in &rec.groups {
        assert_eq!(r.last_index, 400);
        assert_eq!(r.snapshot.as_ref().expect("as_ref succeeds").index, 400);
    }
    drop(s2);
}

#[test]
fn wal_stays_bounded_across_restarts() {
    // Regression: segments of earlier lives were not registered on recovery, so they were never
    // released and release stopped being prefix-based (a segment after other retained ones was
    // deleted and the batch chain was broken).
    let cfg = small_cfg();
    let mut disk = MemDisk::new();
    let groups: Vec<GroupId> = (0..4).map(GroupId).collect();
    let mut index = 0u64;
    for life in 0..12 {
        let mut s = recover(&cfg, &mut disk);
        let mut now = Nanos::ZERO;
        for &g in &groups {
            if s.group(g).is_none() {
                s.create_group(g, Bytes::new(), Durability::Strict).expect("create_group succeeds");
            }
        }
        for _ in 0..60 {
            index += 1;
            for &g in &groups {
                s.append_raft(g, &[e(index, 12_000)], None, Durability::Strict)
                    .expect("append_raft succeeds");
                if index.is_multiple_of(10) {
                    s.save_snapshot(g, snap(index), Bytes::from("x"))
                        .expect("save_snapshot succeeds");
                }
            }
            settle(&mut s, &mut disk, &mut now);
        }
        let n = wal_segments(&mut disk);
        assert!(n <= 5, "life {life}: {n} WAL segments");
    }
    let (_, rec) = CoreStore::recover(&cfg, 0, &mut disk).expect("recover succeeds");
    for r in &rec.groups {
        assert_eq!(r.last_index, index);
    }
}

#[test]
fn idle_group_state_survives_wal_release() {
    let cfg = small_cfg();
    let mut disk = MemDisk::new();
    let mut s = recover(&cfg, &mut disk);
    let mut now = Nanos::ZERO;
    let (idle, busy) = (GroupId(1), GroupId(2));
    s.create_group(idle, Bytes::from("idle"), Durability::Strict).expect("create_group succeeds");
    s.create_group(busy, Bytes::new(), Durability::Strict).expect("create_group succeeds");
    // The idle group votes in term 7 and writes nothing else.
    s.append_raft(
        idle,
        &[e(1, 10)],
        Some(HardState { term: 7, vote: 3, commit: 1 }),
        Durability::Strict,
    )
    .expect("append_raft succeeds");
    s.save_snapshot(idle, snap(1), Bytes::from("i")).expect("save_snapshot succeeds");
    settle(&mut s, &mut disk, &mut now);
    for round in 1..=300u64 {
        s.append_raft(busy, &[e(round, 20_000)], None, Durability::Strict)
            .expect("append_raft succeeds");
        if round % 20 == 0 {
            s.save_snapshot(busy, snap(round), Bytes::new()).expect("save_snapshot succeeds");
        }
        settle(&mut s, &mut disk, &mut now);
    }
    assert!(wal_segments(&mut disk) <= 4);
    // After restarting the vote is still there (it cannot vote again in term 7).
    let (s2, rec) = CoreStore::recover(&cfg, 0, &mut disk).expect("recover succeeds");
    let r = rec.groups.iter().find(|g| g.id == idle).expect("find succeeds");
    assert_eq!(r.hard_state, HardState { term: 7, vote: 3, commit: 1 });
    assert_eq!(r.conf, Bytes::from("conf-1"));
    assert_eq!(s2.hard_state(idle).expect("hard_state succeeds").vote, 3);
}

#[test]
fn release_floor_holds_back_the_wal() {
    let cfg = small_cfg();
    let mut disk = MemDisk::new();
    let mut s = recover(&cfg, &mut disk);
    let mut now = Nanos::ZERO;
    let g = GroupId(1);
    s.create_group(g, Bytes::new(), Durability::Strict).expect("create_group succeeds");
    // The LSM still needs everything: snapshots would compact what it has not flushed.
    s.set_release_floor(g, ReleaseProvider::Lsm, Some(0));
    for round in 1..=200u64 {
        s.append_raft(g, &[e(round, 40_000)], None, Durability::Strict)
            .expect("append_raft succeeds");
        if round % 20 == 0 {
            let refused = s.save_snapshot(g, snap(round), Bytes::new());
            assert!(matches!(refused, Err(StorageError::Invalid(_))), "{refused:?}");
        }
        settle(&mut s, &mut disk, &mut now);
    }
    let held = wal_segments(&mut disk);
    assert!(held > 4, "the floor retains the WAL ({held} segments)");
    // The store says what the WAL takes and who holds its oldest segment back.
    assert_eq!(s.wal_disk_bytes(), held as u64 * cfg.wal_segment_size);
    assert!(s.entries_bytes() >= 200 * 40_000);
    let holder =
        teseladb_raftlog::WalHolder { group: g, behind_snapshot: true, behind_floor: true };
    assert_eq!(s.wal_holders(), vec![holder]);
    assert_eq!(
        s.entries(g, 1, 2, u64::MAX).expect("entries succeeds")[0].index,
        1,
        "every entry is still there"
    );
    // A second provider further back keeps holding it; the lowest floor counts.
    s.set_release_floor(g, ReleaseProvider::Search, Some(10));
    s.set_release_floor(g, ReleaseProvider::Lsm, Some(200));
    assert_eq!(s.release_floor_of(g), Some(10));
    assert!(s.save_snapshot(g, snap(150), Bytes::new()).is_err());
    s.save_snapshot(g, snap(10), Bytes::new()).expect("save_snapshot succeeds");
    settle(&mut s, &mut disk, &mut now);
    assert!(wal_segments(&mut disk) >= held - 1, "still held by search");
    // Once it holds nothing back, only the snapshot counts.
    s.set_release_floor(g, ReleaseProvider::Search, None);
    assert_eq!(s.release_floor_of(g), Some(200));
    s.append_raft(g, &[e(201, 10)], None, Durability::Strict).expect("append_raft succeeds");
    s.save_snapshot(g, snap(200), Bytes::new()).expect("save_snapshot succeeds");
    settle(&mut s, &mut disk, &mut now);
    s.append_raft(g, &[e(202, 10)], None, Durability::Strict).expect("append_raft succeeds");
    settle(&mut s, &mut disk, &mut now);
    assert!(wal_segments(&mut disk) <= 2, "{}", wal_segments(&mut disk));
    assert!(s.wal_holders().iter().all(|h| !h.behind_floor));
    assert!(s.entries_bytes() < 40_000);
}

#[test]
fn crash_during_snapshot_write_keeps_old_or_new_complete() {
    let cfg: StorageConfig = small_cfg();
    for seed in 0..200 {
        let mut disk =
            SimDisk::new(SimDiskConfig { torn_write_probability: 0.7, ..Default::default() });
        let mut rng = Rng::new(seed);
        let mut s = CoreStore::recover(&cfg, 0, &mut disk).expect("recover succeeds").0;
        let mut now = Nanos::ZERO;
        let g = GroupId(1);
        s.create_group(g, Bytes::new(), Durability::Strict).expect("create_group succeeds");
        s.append_raft(g, &(1..=4).map(|i| e(i, 100)).collect::<Vec<_>>(), None, Durability::Strict)
            .expect("append_raft succeeds");
        s.save_snapshot(g, snap(2), Bytes::from_vec(vec![2; 9000]))
            .expect("save_snapshot succeeds");
        settle(&mut s, &mut disk, &mut now);
        s.save_snapshot(g, snap(4), Bytes::from_vec(vec![4; 9000]))
            .expect("save_snapshot succeeds");
        let stop = rng.range(0, 8);
        for _ in 0..stop {
            match s.poll_io(now) {
                Some(op) => {
                    let d = disk.execute(op);
                    s.on_io_done(d, now);
                }
                None => break,
            }
        }
        disk.crash(&mut rng);
        let mut s2 = CoreStore::recover(&cfg, 0, &mut disk)
            .unwrap_or_else(|e| panic!("seed {seed}: {e:?}"))
            .0;
        let (meta, tok) = s2.load_snapshot(g).expect("load_snapshot succeeds");
        let c = s2.run_io(&mut disk, now);
        let (got_meta, data) = c.snapshots[0].1.clone().expect("clone succeeds");
        assert_eq!(got_meta.index, meta.index);
        let expected = vec![meta.index.to_le_bytes()[0]; 9000];
        assert!(meta.index == 2 || meta.index == 4);
        assert_eq!(data.to_vec(), expected, "seed {seed}");
        let _ = tok;
    }
}
