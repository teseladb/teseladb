//! Compression of the WAL's regions (data format 11): a region whose records shrink goes as one LZ4
//! block, alone or sealed with encryption at rest; recovery gives back the same entries and hard
//! state, whether the regions went compressed, as they were, or both in one log.

mod common;

use std::sync::Arc;

use common::*;
use teseladb_crypto::at_rest::{CipherMode, Encryption, KeyRing};
use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::{
    CoreStore, Disk, HardState, LogEntry, MemDisk, SimDisk, SimDiskConfig, StorageConfig,
    StorageError, paths,
};
use teseladb_types::{Bytes, Durability, GroupId, Nanos};

/// A command as the ranges write them: a document whose field names and padding repeat.
fn doc(index: u64) -> Bytes {
    let mut d =
        format!("{{_id: \"c003-{index:08}\", c: 3, n: {index}, g: {}, v: 0, s: \"\", ", index % 16)
            .into_bytes();
    d.extend(std::iter::repeat_n(b'x', 900));
    Bytes::from_vec(d)
}

fn entry(index: u64, data: Bytes) -> LogEntry {
    LogEntry { index, term: 1, kind: EntryKind::Normal, data }
}

/// Bytes that no LZ4 block shrinks.
fn noise(index: u64) -> Bytes {
    let mut x = index.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    Bytes::from_vec(
        (0..600)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x.to_le_bytes()[0]
            })
            .collect(),
    )
}

/// Appends entries 1..=n of `data` to group 1 in batches of 10, then a hard state, and returns
/// the WAL bytes written.
fn write(cfg: &StorageConfig, disk: &mut dyn Disk, n: u64, data: fn(u64) -> Bytes) -> u64 {
    let mut s = recover(cfg, disk);
    let mut now = Nanos::ZERO;
    let g = GroupId(1);
    s.create_group(g, Bytes::from("c0"), Durability::Strict).expect("create_group succeeds");
    for first in (1..=n).step_by(10) {
        let batch: Vec<LogEntry> = (first..first + 10).map(|i| entry(i, data(i))).collect();
        s.append_raft(g, &batch, None, Durability::Strict).expect("append_raft succeeds");
        settle(&mut s, disk, &mut now);
    }
    let hs = Some(HardState { term: 1, vote: 1, commit: n });
    s.append_raft(g, &[], hs, Durability::Strict).expect("append_raft succeeds");
    settle(&mut s, disk, &mut now);
    s.metrics().wal_bytes_written
}

fn recovered(cfg: &StorageConfig, disk: &mut dyn Disk, n: u64) -> (Vec<LogEntry>, u64) {
    let (s, _) = CoreStore::recover(cfg, 0, disk).expect("recover succeeds");
    let g = GroupId(1);
    let got = s.entries(g, 1, n + 1, u64::MAX).expect("entries succeeds");
    (got.to_vec(), s.hard_state(g).expect("hard_state succeeds").commit)
}

fn plain_cfg() -> StorageConfig {
    StorageConfig { compress_wal: false, ..small_cfg() }
}

#[test]
fn compressed_regions_are_smaller_and_recover_the_same_entries() {
    let mut packed = MemDisk::new();
    let mut plain = MemDisk::new();
    let packed_bytes = write(&small_cfg(), &mut packed, 200, doc);
    let plain_bytes = write(&plain_cfg(), &mut plain, 200, doc);
    // Each batch of ten documents takes three blocks as it is and one compressed; the
    // segment's header block is the same in both.
    assert!(packed_bytes * 5 < plain_bytes * 2, "{packed_bytes} vs {plain_bytes}");
    let want: Vec<LogEntry> = (1..=200).map(|i| entry(i, doc(i))).collect();
    assert_eq!(recovered(&small_cfg(), &mut packed, 200), (want.clone(), 200));
    assert_eq!(recovered(&plain_cfg(), &mut plain, 200), (want.clone(), 200));
    // A node that does not compress reads a compressed log, and one that does reads a plain one.
    assert_eq!(recovered(&plain_cfg(), &mut packed, 200), (want.clone(), 200));
    assert_eq!(recovered(&small_cfg(), &mut plain, 200), (want, 200));
}

#[test]
fn regions_that_do_not_shrink_go_as_they_are() {
    let mut a = MemDisk::new();
    let mut b = MemDisk::new();
    assert_eq!(write(&small_cfg(), &mut a, 50, noise), write(&plain_cfg(), &mut b, 50, noise));
    let want: Vec<LogEntry> = (1..=50).map(|i| entry(i, noise(i))).collect();
    assert_eq!(recovered(&small_cfg(), &mut a, 50), (want, 50));
}

#[test]
fn a_log_with_both_kinds_of_regions_recovers_whole() {
    let mut disk = MemDisk::new();
    write(&plain_cfg(), &mut disk, 30, doc);
    // The same log goes on compressed after a restart: its first batches stay as they were.
    let mut s = recover(&small_cfg(), &mut disk);
    let mut now = Nanos::ZERO;
    let g = GroupId(1);
    let more: Vec<LogEntry> = (31..=60).map(|i| entry(i, doc(i))).collect();
    s.append_raft(g, &more, Some(HardState { term: 1, vote: 1, commit: 60 }), Durability::Strict)
        .expect("append_raft succeeds");
    settle(&mut s, &mut disk, &mut now);
    drop(s);
    let want: Vec<LogEntry> = (1..=60).map(|i| entry(i, doc(i))).collect();
    assert_eq!(recovered(&small_cfg(), &mut disk, 60), (want.clone(), 60));
    assert_eq!(recovered(&plain_cfg(), &mut disk, 60), (want, 60));
}

#[test]
fn sealed_regions_are_compressed_first_and_recover() {
    for mode in [CipherMode::Cbc, CipherMode::Gcm] {
        let ring = KeyRing::new(
            &[3; 32],
            mode,
            |b: &mut [u8]| {
                b.fill(5);
                Ok(())
            },
            "teseladb-raftlog test",
        );
        let sealed = StorageConfig { encryption: Some(Encryption(Arc::new(ring))), ..small_cfg() };
        let sealed_plain = StorageConfig { compress_wal: false, ..sealed.clone() };
        let mut a = MemDisk::new();
        let mut b = MemDisk::new();
        let packed = write(&sealed, &mut a, 100, doc);
        let plain = write(&sealed_plain, &mut b, 100, doc);
        assert!(packed * 5 < plain * 2, "{mode:?}: {packed} vs {plain}");
        // Nothing of the documents is in clear in the segments.
        for p in a.list(&paths::wal_dir(0)).expect("list succeeds") {
            let bytes = a.read_all(&p).expect("read_all succeeds");
            assert!(!bytes.windows(12).any(|w| w == b"\"c003-000000"), "{mode:?}: {}", p.display());
        }
        let want: Vec<LogEntry> = (1..=100).map(|i| entry(i, doc(i))).collect();
        assert_eq!(recovered(&sealed, &mut a, 100), (want, 100), "{mode:?}");
    }
}

#[test]
fn a_damaged_compressed_region_stops_the_recovery() {
    let mut disk = SimDisk::new(SimDiskConfig::default());
    write(&small_cfg(), &mut disk, 40, doc);
    let seg = disk.list(&paths::wal_dir(0)).expect("list succeeds")[0].clone();
    let bytes = disk.read_all(&seg).expect("read_all succeeds");
    // A byte of the LZ4 data of the first compressed region, which batches follow.
    let at =
        bytes.windows(2).position(|w| w == 0x5A4C_u16.to_le_bytes()).expect("a compressed region");
    assert!(disk.corrupt(&seg, at as u64 + 20));
    assert!(matches!(
        CoreStore::recover(&small_cfg(), 0, &mut disk),
        Err(StorageError::Corrupt { .. })
    ));
}
