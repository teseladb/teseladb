//! Encryption at rest of the LSM (013 T026, FR-015, SC-005): with a key ring, no value or key
//! written appears in clear in the SST files nor in the manifest, in both cipher modes, through
//! flushes, compactions and a manifest rotation; the files read back after a restart with the
//! same master key, and not without it or with another one.

mod common;

use std::sync::Arc;

use common::{get, open, small, ts};
use teseladb_crypto::at_rest::{CipherMode, KeyRing};
use teseladb_lsm::{Encryption, Lsm, LsmConfig};
use teseladb_raftlog::{Disk, MemDisk};
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);
const SECRET_KEY: &[u8] = b"customer-4711";
const SECRET_VALUE: &[u8] = b"card 4111-1111-1111-1111";

fn encrypted(mode: CipherMode, master: [u8; 32]) -> LsmConfig {
    let ring = KeyRing::new(&master, mode, |b: &mut [u8]| {
        b.fill(3);
        Ok(())
    });
    LsmConfig { encryption: Some(Encryption(Arc::new(ring))), ..small() }
}

/// Every byte of every file of the LSM's directory.
fn files(cfg: &LsmConfig, disk: &mut MemDisk) -> Vec<(String, Vec<u8>)> {
    disk.list(&cfg.dir)
        .unwrap()
        .into_iter()
        .map(|p| {
            let bytes = disk.read_all(&p).unwrap();
            (p.display().to_string(), bytes)
        })
        .collect()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn fill(lsm: &mut Lsm, disk: &mut MemDisk) {
    for i in 0..200u64 {
        let key = [SECRET_KEY, format!("-{i:04}").as_bytes()].concat();
        lsm.put(G, i + 1, &key, ts(10 + i, 0), SECRET_VALUE).unwrap();
        if i % 50 == 49 {
            lsm.flush();
            lsm.run_io(disk, Nanos::ZERO);
        }
    }
    lsm.compact_all();
    lsm.run_io(disk, Nanos::ZERO);
}

#[test]
fn nothing_written_shows_in_clear_and_it_reads_back_after_a_restart() {
    for mode in [CipherMode::Cbc, CipherMode::Gcm] {
        let cfg = encrypted(mode, [7; 32]);
        let mut disk = MemDisk::new();
        let mut lsm = open(&cfg, &mut disk);
        fill(&mut lsm, &mut disk);
        drop(lsm);
        let all = files(&cfg, &mut disk);
        assert!(all.iter().any(|(n, _)| n.ends_with(".sst")), "{mode:?}");
        assert!(all.iter().any(|(n, _)| n.contains("MANIFEST")), "{mode:?}");
        for (name, bytes) in &all {
            assert!(!contains(bytes, SECRET_KEY), "{mode:?}: a key in clear in {name}");
            assert!(!contains(bytes, SECRET_VALUE), "{mode:?}: a value in clear in {name}");
        }
        // The same master key reads everything back; the ring of the files' mode does not matter.
        let other_mode = if mode == CipherMode::Cbc { CipherMode::Gcm } else { CipherMode::Cbc };
        let again = encrypted(other_mode, [7; 32]);
        let mut lsm = open(&again, &mut disk);
        let v = lsm.view();
        let key = [SECRET_KEY, b"-0123"].concat();
        let row = get(&mut lsm, &mut disk, &v, &key, ts(1_000, 0)).unwrap();
        assert_eq!(row.3, SECRET_VALUE);
        drop(lsm);
        // Without the key ring, or with another master key, the files do not open.
        assert!(Lsm::recover(&small(), &mut disk).is_err(), "{mode:?}");
        assert!(Lsm::recover(&encrypted(mode, [8; 32]), &mut disk).is_err(), "{mode:?}");
    }
}

#[test]
fn a_clear_lsm_keeps_its_files_readable_by_a_node_with_a_ring() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    fill(&mut lsm, &mut disk);
    drop(lsm);
    assert!(files(&small(), &mut disk).iter().any(|(_, b)| contains(b, SECRET_VALUE)));
    // Clear files written before encryption was turned on still read; new ones are sealed.
    let cfg = encrypted(CipherMode::Gcm, [7; 32]);
    let mut lsm = open(&cfg, &mut disk);
    let v = lsm.view();
    let key = [SECRET_KEY, b"-0001"].concat();
    assert_eq!(get(&mut lsm, &mut disk, &v, &key, ts(1_000, 0)).unwrap().3, SECRET_VALUE);
    lsm.put(G, 500, b"later", ts(5_000, 0), b"sealed value").unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    lsm.compact_all();
    lsm.run_io(&mut disk, Nanos::ZERO);
    drop(lsm);
    for (name, bytes) in files(&cfg, &mut disk) {
        assert!(!contains(&bytes, b"sealed value"), "{name}");
    }
}
