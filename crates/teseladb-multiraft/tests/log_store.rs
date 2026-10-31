//! A group's log in the core's store as Raft sees it: what the snapshot compacted and what does
//! not exist yet are told apart.

use teseladb_multiraft::log_store::{GroupStore, encode_conf};
use teseladb_raft::{ConfState, LogGap, LogStore};
use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::{CoreStore, LogEntry, MemDisk, SnapshotMeta, StorageConfig};
use teseladb_types::{Bytes, Durability, GroupId, Nanos, NodeId};

#[test]
fn compacted_and_missing_entries_are_different_gaps() {
    let g = GroupId(3);
    let mut disk = MemDisk::new();
    let mut s = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap().0;
    let conf = encode_conf(&ConfState::voters(&[NodeId(1)]));
    s.create_group(g, conf.clone(), Durability::Strict).unwrap();
    let entries: Vec<LogEntry> = (1..=6)
        .map(|i| LogEntry { index: i, term: 1, kind: EntryKind::Normal, data: Bytes::from("e") })
        .collect();
    s.append_raft(g, &entries, None, Durability::Strict).unwrap();
    s.run_io(&mut disk, Nanos::ZERO);
    s.save_snapshot(g, SnapshotMeta { index: 4, term: 1, conf }, Bytes::from("state")).unwrap();
    s.run_io(&mut disk, Nanos::ZERO);
    let view = GroupStore::new(&s, g);
    assert_eq!(view.entries(1, 3, u64::MAX), Err(LogGap::Compacted));
    assert_eq!(view.entries(5, 9, u64::MAX), Err(LogGap::Unavailable));
    assert_eq!(view.entries(5, 7, u64::MAX).unwrap().len(), 2);
    assert_eq!(view.term(2), Err(LogGap::Compacted));
    assert_eq!(view.term(4), Ok(1));
    assert_eq!(view.term(9), Err(LogGap::Unavailable));
    assert_eq!((view.first_index(), view.last_index()), (5, 6));
    assert_eq!(view.snapshot_meta().index, 4);
}
