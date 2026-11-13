//! Adversarial cases (004 T030, third pass; FR-014): every byte of a file and of the manifest
//! flipped, files cut short, `CURRENT` damaged. Recovery and reads either give the right data or
//! report the core corrupt; they never give a wrong value and never panic. Also ingestions that
//! must be refused.

mod common;

use std::path::{Path, PathBuf};

use common::{open, small, ts};
use teseladb_lsm::{FileRef, Lookup, Lsm, LsmConfig, LsmError};
use teseladb_raftlog::{Disk, FileId, IoOp, SimDisk};
use teseladb_types::{GroupId, Nanos, Rng};

const G: GroupId = GroupId(1);
const KEYS: u64 = 120;

fn key(i: u64) -> Vec<u8> {
    format!("k{i:04}").into_bytes()
}

fn value(i: u64) -> Vec<u8> {
    format!("value-{i}-{}", "v".repeat((i % 50) as usize)).into_bytes()
}

/// A disk with every key flushed into one file, plus a range tombstone and a second file.
fn written(cfg: &LsmConfig) -> SimDisk {
    let mut disk = SimDisk::default();
    let mut lsm = open(cfg, &mut disk);
    for i in 0..KEYS {
        lsm.put(G, i + 1, &key(i), ts(10, 0), &value(i)).unwrap();
    }
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    lsm.delete_range(G, KEYS + 1, &key(KEYS), &key(KEYS + 10), ts(11, 0)).unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    disk
}

fn files(disk: &mut SimDisk, dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = disk
        .list(dir)
        .unwrap()
        .into_iter()
        .filter(|p| p.to_string_lossy().ends_with(suffix))
        .collect();
    v.sort();
    v
}

/// Recovers and reads every key: right values or a corrupt core, nothing else.
fn right_or_corrupt(cfg: &LsmConfig, disk: &mut SimDisk, what: &str) -> bool {
    let mut lsm = match Lsm::recover(cfg, disk) {
        Ok((lsm, _)) => lsm,
        Err(LsmError::Corrupt { .. }) => return false,
        Err(e) => panic!("{what}: {e}"),
    };
    let v = lsm.view();
    let mut intact = true;
    for i in 0..KEYS {
        let got = loop {
            match v.get(&mut lsm, &key(i), ts(20, 0)) {
                Lookup::Miss(_) => {
                    lsm.run_io(disk, Nanos::ZERO);
                }
                other => break other,
            }
        };
        match got {
            Lookup::Hit(f) => assert_eq!(&*f.value, value(i).as_slice(), "{what}: key {i}"),
            Lookup::Failed(LsmError::Corrupt { .. }) => intact = false,
            // A torn manifest tail can drop the last flush; the log would replay it.
            Lookup::NotFound => intact = false,
            other => panic!("{what}: key {i}: {other:?}"),
        }
    }
    intact
}

#[test]
fn every_flipped_byte_of_a_file_is_caught_or_harmless() {
    let cfg = small();
    let mut probe = written(&cfg);
    let sst = files(&mut probe, &cfg.dir, ".sst")[0].clone();
    let size = probe.read_all(&sst).unwrap().len() as u64;
    let (mut caught, mut harmless) = (0, 0);
    for offset in (0..size).step_by(11) {
        let mut disk = written(&cfg);
        assert!(disk.corrupt(&sst, offset));
        if right_or_corrupt(&cfg, &mut disk, &format!("byte {offset}")) {
            harmless += 1;
        } else {
            caught += 1;
        }
    }
    // Each block sits in its own 4 KiB pages; the padding after it is never read, so flipping it
    // is harmless. Every other byte is under a checksum.
    assert!(caught > 0 && harmless > 0, "{caught} caught, {harmless} harmless");
}

#[test]
fn a_file_cut_short_is_caught() {
    let cfg = small();
    let mut probe = written(&cfg);
    let sst = files(&mut probe, &cfg.dir, ".sst")[0].clone();
    let size = probe.read_all(&sst).unwrap().len() as u64;
    for len in [0, 1, 4096, size / 2, size - 4096, size - 1] {
        let mut disk = written(&cfg);
        let f = FileId(500);
        disk.execute(IoOp::Open { file: f, path: sst.clone() });
        disk.execute(IoOp::Truncate { file: f, len });
        disk.execute(IoOp::Sync { file: f, batch: 0 });
        disk.execute(IoOp::Close { file: f });
        assert!(!right_or_corrupt(&cfg, &mut disk, &format!("cut at {len}")), "cut at {len}");
    }
}

#[test]
fn every_flipped_byte_of_the_manifest_is_caught_or_a_cut_tail() {
    let cfg = small();
    let mut probe = written(&cfg);
    let manifest = files(&mut probe, &cfg.dir, "")
        .into_iter()
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("MANIFEST-"))
        .unwrap();
    let size = probe.read_all(&manifest).unwrap().len() as u64;
    for offset in (0..size).step_by(5) {
        let mut disk = written(&cfg);
        assert!(disk.corrupt(&manifest, offset));
        right_or_corrupt(&cfg, &mut disk, &format!("manifest byte {offset}"));
    }
}

#[test]
fn a_damaged_current_is_caught() {
    let cfg = small();
    for offset in [0, 3, 9, 15, 4095] {
        let mut disk = written(&cfg);
        assert!(disk.corrupt(&cfg.dir.join("CURRENT"), offset));
        // Its name, its newline or the zeros after them: any change is refused.
        let result = Lsm::recover(&cfg, &mut disk);
        assert!(matches!(result, Err(LsmError::Corrupt { .. })), "CURRENT byte {offset}");
    }
}

fn exported(cfg: &LsmConfig) -> (SimDisk, Vec<FileRef>) {
    let mut disk = written(cfg);
    let mut lsm = open(cfg, &mut disk);
    lsm.export(&key(0), &key(KEYS), ts(20, 0), "out".into());
    let done = lsm.run_io(&mut disk, Nanos::ZERO);
    // Everything is synced: a crash only closes the exporter's files before another LSM opens.
    drop(lsm);
    disk.crash(&mut Rng::new(1));
    (disk, done.exported.into_iter().next().unwrap().1)
}

fn ingest(
    lsm: &mut Lsm,
    disk: &mut SimDisk,
    files: Vec<FileRef>,
    start: &[u8],
    end: &[u8],
) -> Result<(), LsmError> {
    lsm.ingest(files, start, end);
    lsm.run_io(disk, Nanos::ZERO).ingested.into_iter().next().unwrap().1
}

#[test]
fn ingestions_into_ranges_with_tombstones_or_with_bad_files_are_refused() {
    let cfg = LsmConfig { dir: "other".into(), ..small() };
    let (mut disk, files) = exported(&small());
    let mut lsm = open(&cfg, &mut disk);
    // A range tombstone in the memtable over the range.
    lsm.delete_range(G, 1, &key(5), &key(6), ts(1, 0)).unwrap();
    assert_eq!(
        ingest(&mut lsm, &mut disk, files.clone(), &key(0), &key(KEYS)),
        Err(LsmError::IngestOverlap)
    );
    // The same tombstone in a file.
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    assert_eq!(
        ingest(&mut lsm, &mut disk, files.clone(), &key(0), &key(KEYS)),
        Err(LsmError::IngestOverlap)
    );
    // A file whose footer points past its size.
    let (mut other, good) = exported(&small());
    let path = good[0].path.clone();
    let size = good[0].size;
    assert!(other.corrupt(&path, size - 128));
    let mut fresh = open(&LsmConfig { dir: "fresh".into(), ..small() }, &mut other);
    let err = ingest(&mut fresh, &mut other, good.clone(), &key(0), &key(KEYS)).unwrap_err();
    assert!(matches!(err, LsmError::Corrupt { .. }), "{err:?}");
}

#[test]
fn an_ingestion_beside_data_outside_its_range_goes_to_a_free_level() {
    let cfg = LsmConfig { dir: "other".into(), ..small() };
    let (mut disk, files) = exported(&small());
    let mut lsm = open(&cfg, &mut disk);
    // Data after the range, in a file of the last level, and data before it in level 0.
    lsm.put(G, 1, b"z", ts(1, 0), b"after").unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    lsm.compact_all();
    lsm.run_io(&mut disk, Nanos::ZERO);
    lsm.put(G, 2, b"a", ts(1, 0), b"before").unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    ingest(&mut lsm, &mut disk, files, &key(0), &key(KEYS)).unwrap();
    let levels = lsm.metrics().level_bytes;
    assert!(levels[0] > 0 && levels[cfg.levels] > 0, "{levels:?}");
    assert!(right_or_corrupt(&cfg, &mut disk, "after ingesting"));
}

#[test]
fn an_ingestion_beside_tombstones_and_spans_outside_its_range_goes_through() {
    let cfg = LsmConfig { dir: "other".into(), ..small() };
    let (mut disk, files) = exported(&small());
    let mut lsm = open(&cfg, &mut disk);
    // A tombstone after the range in a file, one before it in the memtable, and a file of the last
    // level whose bounds span the range with no key inside.
    lsm.put(G, 1, b"a", ts(1, 0), b"v").unwrap();
    lsm.put(G, 2, b"z", ts(1, 0), b"v").unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    lsm.compact_all();
    lsm.run_io(&mut disk, Nanos::ZERO);
    lsm.delete_range(G, 3, b"x", b"y", ts(2, 0)).unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    lsm.delete_range(G, 4, b"b", b"c", ts(2, 0)).unwrap();
    ingest(&mut lsm, &mut disk, files, &key(0), &key(KEYS)).unwrap();
    let levels = lsm.metrics().level_bytes;
    assert!(levels[cfg.levels - 1] > 0, "one level up from the spanning file: {levels:?}");
}

#[test]
fn files_that_reach_past_the_range_or_are_too_small_are_refused() {
    let cfg = LsmConfig { dir: "other".into(), ..small() };
    let (mut disk, files) = exported(&small());
    let mut lsm = open(&cfg, &mut disk);
    let half = key(KEYS / 2);
    assert_eq!(
        ingest(&mut lsm, &mut disk, files.clone(), &key(0), &half),
        Err(LsmError::IngestOverlap)
    );
    let tiny = vec![FileRef::new(files[0].path.clone(), 0)];
    assert!(matches!(
        ingest(&mut lsm, &mut disk, tiny, &key(0), &key(KEYS)),
        Err(LsmError::Corrupt { .. })
    ));
    ingest(&mut lsm, &mut disk, files, &key(0), &key(KEYS)).unwrap();
}
