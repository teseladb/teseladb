//! WAL shared by Raft groups.
mod common;

use common::*;
use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::{CoreStore, HardState, LogEntry, LogGap, MemDisk, SimDisk, StorageError};
use teseladb_types::{Bytes, Durability, GroupId, Nanos};

fn e(index: u64, term: u64, tag: &str) -> LogEntry {
    LogEntry { index, term, kind: EntryKind::Normal, data: Bytes::from(tag) }
}

fn hs(term: u64, vote: u64, commit: u64) -> HardState {
    HardState { term, vote, commit }
}

#[test]
fn append_hardstate_truncate_and_recover_per_group() {
    let cfg = small_cfg();
    let mut disk = MemDisk::new();
    let mut s = recover(&cfg, &mut disk);
    let mut now = Nanos::ZERO;
    let (g1, g2, g3) = (GroupId(1), GroupId(2), GroupId(3));
    s.create_group(g1, Bytes::from("c1"), Durability::Strict).expect("create_group succeeds");
    s.create_group(g2, Bytes::from("c2"), Durability::Strict).expect("create_group succeeds");
    s.create_group(g3, Bytes::from("c3"), Durability::Strict).expect("create_group succeeds");
    s.append_raft(
        g1,
        &[e(1, 1, "a"), e(2, 1, "b"), e(3, 1, "c")],
        Some(hs(1, 7, 2)),
        Durability::Strict,
    )
    .expect("append_raft succeeds");
    s.append_raft(g2, &[e(1, 5, "x")], None, Durability::Strict).expect("append_raft succeeds");
    settle(&mut s, &mut disk, &mut now);
    // Overwrite g1's suffix, truncate g2 and delete g3.
    s.append_raft(g1, &[e(3, 2, "C"), e(4, 2, "D")], Some(hs(2, 7, 3)), Durability::Strict)
        .expect("append_raft succeeds");
    s.truncate_raft(g2, 1).expect("truncate_raft succeeds");
    s.delete_group(g3).expect("delete_group succeeds");
    settle(&mut s, &mut disk, &mut now);
    assert_eq!(s.entries(g1, 1, 5, u64::MAX).expect("entries succeeds").len(), 4);

    let (mut s2, rec) = CoreStore::recover(&cfg, 0, &mut disk).expect("recover succeeds");
    assert_eq!(rec.groups.len(), 2);
    let r1 = rec.groups.iter().find(|g| g.id == g1).expect("find succeeds");
    assert_eq!(
        (r1.last_index, r1.hard_state, r1.conf.clone()),
        (4, hs(2, 7, 3), Bytes::from("c1"))
    );
    assert!(r1.snapshot.is_none());
    let got = s2.entries(g1, 1, 5, u64::MAX).expect("entries succeeds");
    assert_eq!(got, &[e(1, 1, "a"), e(2, 1, "b"), e(3, 2, "C"), e(4, 2, "D")]);
    assert_eq!(s2.hard_state(g2), Some(HardState::default()));
    assert_eq!(s2.group(g2).expect("group succeeds").last_index(), 0);
    assert!(s2.group(g3).is_none());
    assert_eq!(s2.entries(g3, 1, 2, 10), Err(LogGap::NoGroup));
    // Keep writing after recovering.
    s2.append_raft(g2, &[e(1, 6, "y")], None, Durability::Strict).expect("append_raft succeeds");
    settle(&mut s2, &mut disk, &mut now);
    let s3 = recover(&cfg, &mut disk);
    assert_eq!(s3.entries(g2, 1, 2, 10).expect("entries succeeds"), &[e(1, 6, "y")]);
}

#[test]
fn invalid_group_operations() {
    let cfg = small_cfg();
    let mut disk = MemDisk::new();
    let mut s = recover(&cfg, &mut disk);
    let g = GroupId(1);
    assert!(matches!(
        s.append_raft(g, &[e(1, 1, "a")], None, Durability::Strict),
        Err(StorageError::Invalid(_))
    ));
    assert!(matches!(s.truncate_raft(g, 1), Err(StorageError::Invalid(_))));
    assert!(matches!(s.delete_group(g), Err(StorageError::Invalid(_))));
    s.create_group(g, Bytes::new(), Durability::Strict).expect("create_group succeeds");
    assert!(matches!(
        s.create_group(g, Bytes::new(), Durability::Strict),
        Err(StorageError::Invalid(_))
    ));
    assert!(matches!(
        s.append_raft(g, &[e(3, 1, "a")], None, Durability::Strict),
        Err(StorageError::Invalid(_))
    ));
    assert!(matches!(s.truncate_raft(g, 0), Err(StorageError::Invalid(_))));
    assert!(s.hard_state(GroupId(9)).is_none());
    // `memory` and `none` modes: immediate acknowledgement.
    let t =
        s.append_raft(g, &[e(1, 1, "a")], None, Durability::Memory).expect("append_raft succeeds");
    let t2 = s
        .append_raft(g, &[e(2, 1, "b")], Some(hs(1, 0, 0)), Durability::None)
        .expect("append_raft succeeds");
    let c = s.take_completions();
    assert_eq!(ok_tickets(&c), vec![t, t2]);
    let t3 =
        s.create_group(GroupId(2), Bytes::new(), Durability::None).expect("create_group succeeds");
    assert_eq!(ok_tickets(&s.take_completions()), vec![t3]);
}

#[test]
fn group_commit_batches_many_groups_per_sync() {
    let cfg = small_cfg();
    let mut disk = SimDisk::default();
    let mut s = CoreStore::recover(&cfg, 0, &mut disk).expect("recover succeeds").0;
    let mut now = Nanos::ZERO;
    for g in 0..1000 {
        s.create_group(GroupId(g), Bytes::new(), Durability::Strict)
            .expect("create_group succeeds");
    }
    settle(&mut s, &mut disk, &mut now);
    let syncs_before = disk.stats().syncs;
    let batches_before = s.metrics().batches;
    let mut done = 0;
    for round in 1..=5u64 {
        for g in 0..1000 {
            s.append_raft(
                GroupId(g),
                &[e(round, 1, "payload-of-about-a-hundred-bytes")],
                None,
                Durability::Strict,
            )
            .expect("append_raft succeeds");
        }
        done += ok_tickets(&settle(&mut s, &mut disk, &mut now)).len();
    }
    assert_eq!(done, 5000);
    let batches = s.metrics().batches - batches_before;
    let per_batch = 5000.0 / f64::from(u32::try_from(batches).expect("a test count"));
    assert!(per_batch >= 20.0, "{per_batch} requests per batch ({batches} batches)");
    // Each batch syncs the WAL once (plus the new segments and their directories).
    assert!(disk.stats().syncs - syncs_before <= 2 * batches + 4);
    assert!(s.metrics().mean_batch_size() >= 20.0);
}
