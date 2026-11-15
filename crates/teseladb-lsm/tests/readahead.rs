//! Reading ahead in a scan (FR-044): a file read in order asks the disk for few reads, each of
//! the blocks after the one it needs; a point read asks for its block alone; a bad block read
//! ahead stops the scan as a bad block read on its own does (FR-014).

mod common;

use std::path::PathBuf;

use common::{get, open, scan, small, ts};
use teseladb_lsm::{LsmConfig, LsmError, Step};
use teseladb_raftlog::{Disk, SimDisk};
use teseladb_types::{GroupId, Nanos, Rng};

const G: GroupId = GroupId(1);
const KEYS: u64 = 600;

fn key(i: u64) -> Vec<u8> {
    format!("k{i:05}").into_bytes()
}

/// Bytes that do not compress, so that the file has many 4 KiB blocks.
fn value(i: u64) -> Vec<u8> {
    let mut v = vec![0; 700];
    Rng::new(i + 1).fill(&mut v);
    v
}

/// One memtable and one file hold every key, with no compactions: the flush writes one file.
fn config() -> LsmConfig {
    LsmConfig {
        memtable_bytes: 4 << 20,
        target_file_bytes: 4 << 20,
        cache_bytes: 4 << 20,
        l0_trigger: 100,
        l0_slow: 100,
        l0_reject: 100,
        ..small()
    }
}

/// A disk with every key flushed into one file.
fn written(cfg: &LsmConfig) -> SimDisk {
    let mut disk = SimDisk::default();
    let mut lsm = open(cfg, &mut disk);
    for i in 0..KEYS {
        lsm.put(G, i + 1, &key(i), ts(10, 0), &value(i)).unwrap();
    }
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    disk
}

fn sst(disk: &mut SimDisk, cfg: &LsmConfig) -> PathBuf {
    let mut v: Vec<PathBuf> = disk
        .list(&cfg.dir)
        .unwrap()
        .into_iter()
        .filter(|p| p.to_string_lossy().ends_with(".sst"))
        .collect();
    assert_eq!(v.len(), 1, "{v:?}");
    v.remove(0)
}

#[test]
fn a_scan_in_order_reads_the_blocks_ahead_in_few_disk_reads() {
    let cfg = config();
    let mut disk = written(&cfg);
    // Reopened, the cache holds only the file's pinned meta.
    let mut lsm = open(&cfg, &mut disk);
    let blocks = lsm.space().data.blocks;
    assert!(blocks > 100, "{blocks} blocks");
    let v = lsm.view();
    let rows = scan(&mut lsm, &mut disk, &v, b"k", b"l", ts(20, 0));
    assert_eq!(rows.len() as u64, KEYS);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(
            (row.0.as_slice(), row.3.as_slice()),
            (&key(i as u64)[..], &value(i as u64)[..])
        );
    }
    // Two reads of one block, then 8 KiB doubling up to 256 KiB: a handful of reads where a
    // read per block would take as many as the file has blocks.
    let reads = lsm.metrics().cache_misses_total;
    assert!(reads >= 3 && reads * 8 < blocks, "{reads} reads for {blocks} blocks");
    // Every block of the file came in: a second scan reads nothing.
    let again = scan(&mut lsm, &mut disk, &v, b"k", b"l", ts(20, 0));
    assert_eq!(again, rows);
    assert_eq!(lsm.metrics().cache_misses_total, reads);
}

#[test]
fn a_point_read_reads_its_block_alone() {
    let cfg = config();
    let mut disk = written(&cfg);
    let mut lsm = open(&cfg, &mut disk);
    let v = lsm.view();
    assert_eq!(get(&mut lsm, &mut disk, &v, &key(0), ts(20, 0)).unwrap().3, value(0));
    assert_eq!(lsm.metrics().cache_misses_total, 1);
    // The key of the next block was not read along: its read is a disk read of its own.
    let next = (1..KEYS).find(|&i| {
        get(&mut lsm, &mut disk, &v, &key(i), ts(20, 0)).unwrap();
        lsm.metrics().cache_misses_total > 1
    });
    assert!(next.is_some_and(|i| i < 10), "{next:?}");
    assert_eq!(lsm.metrics().cache_misses_total, 2);
}

#[test]
fn a_bad_block_read_ahead_stops_the_scan() {
    let cfg = config();
    let mut probe = written(&cfg);
    let path = sst(&mut probe, &cfg);
    // A byte in the data blocks past the first reads, which only a read ahead brings in.
    let mut disk = written(&cfg);
    assert!(disk.corrupt(&path, 64 << 10));
    let mut lsm = open(&cfg, &mut disk);
    let v = lsm.view();
    let mut s = v.scan(b"k", b"l", ts(20, 0));
    let mut seen = 0u64;
    let failed = loop {
        match s.next(&mut lsm) {
            Step::Item(_) => seen += 1,
            Step::Miss(_) => {
                lsm.run_io(&mut disk, Nanos::ZERO);
            }
            Step::Done => break None,
            Step::Failed(e) => break Some(e),
        }
    };
    assert!(matches!(failed, Some(LsmError::Corrupt { .. })), "{failed:?} after {seen} rows");
    assert!(seen < KEYS, "{seen}");
}
