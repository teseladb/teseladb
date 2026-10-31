//! Damage to stored bytes (002 T006, research R3c): `SimDisk::corrupt` changes a byte of a
//! durable file, and a damaged WAL segment header stops the core's recovery with `Corrupt`.

mod common;

use common::*;
use teseladb_raftlog::format::BLOCK;
use teseladb_raftlog::paths::wal_dir;
use teseladb_raftlog::{CoreStore, Disk, SimDisk, SimDiskConfig, StorageError};
use teseladb_types::{Bytes, Durability, GroupId, Nanos, Rng};

/// A disk whose core 0 wrote and synced a group's creation, then crashed.
fn written_and_crashed() -> SimDisk {
    let mut disk = SimDisk::new(SimDiskConfig::default());
    let mut s = recover(&small_cfg(), &mut disk);
    s.create_group(GroupId(1), Bytes::new(), Durability::Strict).unwrap();
    let mut now = Nanos::ZERO;
    let done = settle(&mut s, &mut disk, &mut now);
    assert_eq!(ok_tickets(&done).len(), 1);
    disk.crash(&mut Rng::new(1));
    disk
}

#[test]
fn a_damaged_wal_header_stops_the_recovery() {
    let mut disk = written_and_crashed();
    let wal = disk.list(&wal_dir(0)).unwrap();
    let segment = wal.first().expect("a WAL segment").clone();
    for offset in [0, 17, BLOCK as u64 / 2, BLOCK as u64 - 1] {
        let mut d = written_and_crashed();
        assert!(d.corrupt(&segment, offset), "byte {offset} of {}", segment.display());
        match CoreStore::recover(&small_cfg(), 0, &mut d) {
            Err(StorageError::Corrupt { .. }) => {}
            other => panic!("byte {offset}: {:?}", other.map(|_| ())),
        }
    }
    // The damage is in the durable contents: another crash does not repair it.
    assert!(disk.corrupt(&segment, 5));
    disk.crash(&mut Rng::new(2));
    assert!(matches!(
        CoreStore::recover(&small_cfg(), 0, &mut disk),
        Err(StorageError::Corrupt { .. })
    ));
}

#[test]
fn nothing_is_damaged_outside_a_durable_file() {
    let mut disk = written_and_crashed();
    let segment = disk.list(&wal_dir(0)).unwrap()[0].clone();
    let len = disk.read_all(&segment).unwrap().len() as u64;
    assert!(!disk.corrupt(&segment, len), "past the end");
    assert!(!disk.corrupt(std::path::Path::new("nope"), 0), "no such file");
    // An undamaged disk still recovers.
    let mut s = recover(&small_cfg(), &mut disk);
    let mut now = Nanos::ZERO;
    settle(&mut s, &mut disk, &mut now);
}
