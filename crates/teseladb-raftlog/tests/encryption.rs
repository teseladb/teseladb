//! Encryption at rest of the log: with a key ring, no entry written
//! appears in clear in the WAL segments nor in the group snapshots, in both cipher modes; the log
//! recovers with the same master key and refuses to start without it or with another one.

mod common;

use std::sync::Arc;

use common::*;
use teseladb_crypto::at_rest::{CipherMode, Encryption, KeyRing};
use teseladb_raftlog::format::snapshot::GroupSnapshot;
use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::{
    CoreStore, Disk, HardState, LogEntry, MemDisk, SnapshotMeta, StorageConfig, StorageError, paths,
};
use teseladb_types::{Bytes, Durability, GroupId, Nanos};

const SECRET: &[u8] = b"entry with a credit card 4111-1111";
const STATE: &[u8] = b"state machine with a secret value";

fn encrypted(mode: CipherMode, master: u8) -> StorageConfig {
    let ring = KeyRing::new(
        &[master; 32],
        mode,
        |b: &mut [u8]| {
            b.fill(9);
            Ok(())
        },
        "teseladb-raftlog test",
    );
    StorageConfig { encryption: Some(Encryption(Arc::new(ring))), ..small_cfg() }
}

fn entry(index: u64) -> LogEntry {
    LogEntry { index, term: 1, kind: EntryKind::Normal, data: Bytes::from(SECRET) }
}

fn all_files(disk: &mut MemDisk) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for dir in [paths::wal_dir(0), paths::snap_dir(0)] {
        for p in disk.list(&dir).unwrap_or_default() {
            let bytes = disk.read_all(&p).expect("read_all succeeds");
            out.push((p.display().to_string(), bytes));
        }
    }
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn the_log_and_snapshots_are_sealed_and_recover_with_the_master_key() {
    for mode in [CipherMode::Cbc, CipherMode::Gcm] {
        let cfg = encrypted(mode, 7);
        let mut disk = MemDisk::new();
        let mut s = recover(&cfg, &mut disk);
        let mut now = Nanos::ZERO;
        let g = GroupId(1);
        s.create_group(g, Bytes::from("c0"), Durability::Strict).expect("create_group succeeds");
        let entries: Vec<LogEntry> = (1..=20).map(entry).collect();
        let hs = Some(HardState { term: 1, vote: 2, commit: 18 });
        s.append_raft(g, &entries, hs, Durability::Strict).expect("append_raft succeeds");
        settle(&mut s, &mut disk, &mut now);
        let meta = SnapshotMeta { index: 10, term: 1, conf: Bytes::from("conf") };
        s.save_snapshot(g, meta, Bytes::from(STATE)).expect("save_snapshot succeeds");
        settle(&mut s, &mut disk, &mut now);
        drop(s);
        let files = all_files(&mut disk);
        assert!(files.iter().any(|(n, _)| n.contains("snap")), "{mode:?}: {files:?}");
        for (name, bytes) in &files {
            assert!(!contains(bytes, SECRET), "{mode:?}: an entry in clear in {name}");
            assert!(!contains(bytes, STATE), "{mode:?}: the snapshot in clear in {name}");
        }
        // The snapshot file is whole without opening it, and opens only with the ring.
        let (_, snap) = files.iter().find(|(n, _)| n.contains("snap")).expect("find succeeds");
        assert!(GroupSnapshot::verify(snap).is_ok());
        assert!(GroupSnapshot::decode(snap).is_err());
        let ring = KeyRing::new(
            &[7; 32],
            mode,
            |b: &mut [u8]| {
                b.fill(1);
                Ok(())
            },
            "teseladb-raftlog test",
        );
        assert_eq!(
            &*GroupSnapshot::decode_with(snap, Some(&ring)).expect("decode_with succeeds").1.data,
            STATE
        );
        // Recovery with the same master key gives back the entries and the snapshot.
        let (mut s, _) = CoreStore::recover(&cfg, 0, &mut disk).expect("recover succeeds");
        let got = s.entries(g, 11, 21, u64::MAX).expect("entries succeeds");
        assert_eq!(got.len(), 10);
        assert!(got.iter().all(|e| &*e.data == SECRET));
        let (m, _) = s.load_snapshot(g).expect("load_snapshot succeeds");
        assert_eq!(m.index, 10);
        let c = s.run_io(&mut disk, now);
        assert_eq!(c.snapshots[0].1.as_ref().expect("as_ref succeeds").1, Bytes::from(STATE));
        drop(s);
        // Without the ring, or with another master key, it does not start.
        let clear = small_cfg();
        assert!(matches!(
            CoreStore::recover(&clear, 0, &mut disk),
            Err(StorageError::Invalid(_) | StorageError::Corrupt { .. })
        ));
        assert!(CoreStore::recover(&encrypted(mode, 8), 0, &mut disk).is_err(), "{mode:?}");
    }
}
