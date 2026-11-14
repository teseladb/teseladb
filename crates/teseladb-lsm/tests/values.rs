//! Value files (004 T060, FR-034): a value longer than `value_separation_bytes` goes to a value
//! file and the file of its key keeps a reference. Every read returns the value; a compaction
//! moves the references and leaves the value files as they are; a value file whose live part falls
//! under half is collected; a restart, encryption and compression read them back.

mod common;

use std::sync::Arc;

use common::{check_all, get, history, open, scan, scan_back, small, ts};
use teseladb_crypto::at_rest::{CipherMode, KeyRing};
use teseladb_keys::Hlc;
use teseladb_lsm::{Codec, Encryption, GcTable, Lsm, LsmConfig, Step};
use teseladb_raftlog::{Disk, MemDisk};
use teseladb_types::{GroupId, Nanos, Rng};

const G: GroupId = GroupId(1);

fn key(i: u64) -> Vec<u8> {
    format!("doc{i:05}").into_bytes()
}

/// A document of `len` bytes that names its key and its round.
fn doc(i: u64, round: u64, len: usize) -> Vec<u8> {
    let mut rng = Rng::new(i * 1000 + round);
    let mut v = vec![0; len];
    rng.fill(&mut v);
    v[..8].copy_from_slice(&(i * 1000 + round).to_le_bytes());
    v
}

fn cfg() -> LsmConfig {
    LsmConfig { compaction_bytes_per_sec: 1 << 40, ..small() }
}

fn settle(lsm: &mut Lsm, disk: &mut MemDisk) {
    lsm.flush();
    lsm.run_io(disk, Nanos::ZERO);
}

fn compact(lsm: &mut Lsm, disk: &mut MemDisk) {
    let id = lsm.compact_now();
    assert_eq!(lsm.run_io(disk, Nanos::ZERO).compacted, vec![id]);
}

/// Writes `n` documents of round `round` at `at`, from 5 to 40 KiB.
fn write_docs(lsm: &mut Lsm, n: u64, round: u64, at: Hlc, index: &mut u64) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            let v = doc(i, round, 5_000 + (i as usize * 997) % 36_000);
            *index += 1;
            lsm.put(G, *index, &key(i), at, &v).unwrap();
            v
        })
        .collect()
}

/// Every read of `docs` at `at`: point reads, forward and backward scans and a time scan.
fn reads_return(lsm: &mut Lsm, disk: &mut MemDisk, docs: &[Vec<u8>], at: Hlc) {
    let view = lsm.view();
    for (i, v) in docs.iter().enumerate() {
        let got = get(lsm, disk, &view, &key(i as u64), at).expect("the document");
        assert!(got.3 == *v, "document {i}");
    }
    let end = key(docs.len() as u64);
    let rows = scan(lsm, disk, &view, &key(0), &end, at);
    assert_eq!(rows.len(), docs.len());
    assert!(rows.iter().zip(docs).all(|(r, v)| r.3 == *v));
    let back = scan_back(lsm, disk, &view, &key(0), &end, at);
    assert!(back.iter().rev().zip(docs).all(|(r, v)| r.3 == *v));
    let mut times = view.scan_time(&key(0), &end, ts(0, 0), at);
    let mut newest = std::collections::BTreeMap::new();
    for _ in 0..100_000 {
        match times.next(lsm) {
            Step::Item(item) => {
                newest.entry(item.key.clone()).or_insert((item.ts, item.value));
            }
            Step::Miss(_) => {
                lsm.run_io(disk, Nanos::ZERO);
            }
            Step::Done => break,
            Step::Failed(e) => panic!("{e}"),
        }
    }
    assert_eq!(newest.len(), docs.len());
    for (i, v) in docs.iter().enumerate() {
        assert!(newest[&key(i as u64)].1.as_ref() == v.as_slice(), "time scan {i}");
    }
}

#[test]
fn large_documents_go_to_value_files_and_every_read_returns_them() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg(), &mut disk);
    let mut index = 0;
    let docs = write_docs(&mut lsm, 120, 0, ts(10, 0), &mut index);
    settle(&mut lsm, &mut disk);
    let m = lsm.metrics();
    assert!(m.value_files >= 1, "{m:?}");
    assert!(m.value_live_bytes * 2 > m.value_bytes, "{m:?}");
    // The files of the keys keep references: much smaller than the documents.
    let keys: u64 = m.level_bytes.iter().sum();
    assert!(keys * 10 < m.value_bytes, "{keys} bytes of keys, {} of values", m.value_bytes);
    reads_return(&mut lsm, &mut disk, &docs, ts(20, 0));
    // The size of the interval counts the values in value files.
    let total: u64 = docs.iter().map(|d| d.len() as u64).sum();
    let size = lsm.interval_size(&key(0), &key(120));
    assert!(size.bytes >= total && size.bytes < total * 2, "{} for {total}", size.bytes);
    assert!(lsm.interval_size_bound(&key(0), &key(120)) >= size.bytes);
    // A compaction moves the references: the value files stay as they are.
    compact(&mut lsm, &mut disk);
    let after = lsm.metrics();
    assert_eq!((after.value_files, after.value_bytes), (m.value_files, m.value_bytes));
    reads_return(&mut lsm, &mut disk, &docs, ts(20, 0));
    // After a restart the manifest lists them with their files.
    drop(lsm);
    let mut lsm = open(&cfg(), &mut disk);
    assert_eq!(lsm.metrics().value_files, m.value_files);
    reads_return(&mut lsm, &mut disk, &docs, ts(20, 0));
}

#[test]
fn a_value_file_mostly_dead_is_collected_and_leaves_the_disk() {
    let mut disk = MemDisk::new();
    // One memtable takes every document: they go to one value file.
    let cfg = LsmConfig { memtable_bytes: 64 << 20, ..self::cfg() };
    let mut lsm = open(&cfg, &mut disk);
    let mut index = 0;
    write_docs(&mut lsm, 100, 0, ts(10, 0), &mut index);
    settle(&mut lsm, &mut disk);
    compact(&mut lsm, &mut disk);
    let first = lsm.metrics();
    assert_eq!(first.value_files, 1, "{first:?}");
    let names = |disk: &mut MemDisk| disk.list(&cfg.dir).unwrap();
    let before = names(&mut disk);
    // Three quarters of the documents change, and the old versions fall under the threshold.
    let mut docs = write_docs(&mut lsm, 75, 1, ts(20, 0), &mut index);
    let view = lsm.view();
    for i in 75..100 {
        docs.push(get(&mut lsm, &mut disk, &view, &key(i), ts(30, 0)).unwrap().3);
    }
    drop(view);
    settle(&mut lsm, &mut disk);
    lsm.set_gc_thresholds(GcTable::new(vec![(Vec::new(), vec![0xff], ts(25, 0))]));
    compact(&mut lsm, &mut disk);
    // The compaction left the first value files mostly dead; the next jobs collect them.
    lsm.run_io(&mut disk, Nanos::ZERO);
    let m = lsm.metrics();
    assert_eq!((m.value_files, m.value_files_sparse), (2, 0), "{m:?}");
    assert!(m.compactions_total > first.compactions_total + 1, "{m:?}");
    // The first value file was collected: what stays alive of it moved to another one, and the
    // bytes on disk are the live ones and a little more.
    assert!(m.value_live_bytes * 2 >= m.value_bytes - 3 * 8192 * m.value_files as u64, "{m:?}");
    let after = names(&mut disk);
    let gone = before.iter().filter(|p| !after.contains(p)).count();
    assert!(gone >= 1, "{before:?} {after:?}");
    assert!(m.value_bytes < first.value_bytes * 2, "{first:?} {m:?}");
    reads_return(&mut lsm, &mut disk, &docs, ts(30, 0));
    drop(lsm);
    let mut lsm = open(&cfg, &mut disk);
    reads_return(&mut lsm, &mut disk, &docs, ts(30, 0));
}

/// Random histories with every value over 16 bytes in a value file, against the model, before and
/// after a restart and a compaction of everything.
#[test]
fn random_histories_read_the_same_with_small_values_apart() {
    for seed in 0..6 {
        let mut disk = MemDisk::new();
        let cfg = LsmConfig { value_separation_bytes: 16, ..cfg() };
        let mut lsm = open(&cfg, &mut disk);
        let model = history(seed, 3_000, &mut lsm, &mut disk);
        // What the memtables hold comes back from the log after a restart: here it is flushed.
        settle(&mut lsm, &mut disk);
        assert!(lsm.metrics().value_files > 0, "seed {seed}");
        drop(lsm);
        let mut lsm = open(&cfg, &mut disk);
        check_all(&mut lsm, &mut disk, &model, 10_000);
        compact(&mut lsm, &mut disk);
        check_all(&mut lsm, &mut disk, &model, 10_000);
    }
}

fn ring(master: [u8; 32]) -> Encryption {
    let ring = KeyRing::new(
        &master,
        CipherMode::Gcm,
        |b: &mut [u8]| {
            b.fill(5);
            Ok(())
        },
        "test",
    );
    Encryption(Arc::new(ring))
}

/// Value files are compressed and sealed like the files of the keys: no document shows in clear,
/// and they read back after a restart.
#[test]
fn value_files_are_compressed_and_encrypted_like_the_others() {
    for codec in [Codec::Lz4, Codec::Zstd] {
        let mut disk = MemDisk::new();
        let cfg = LsmConfig { codec, encryption: Some(ring([7; 32])), ..cfg() };
        let mut lsm = open(&cfg, &mut disk);
        let marker = b"plain-text-marker-of-a-large-document";
        let docs: Vec<Vec<u8>> = (0..40u64)
            .map(|i| {
                let v = marker.repeat(300);
                lsm.put(G, i + 1, &key(i), ts(10, 0), &v).unwrap();
                v
            })
            .collect();
        settle(&mut lsm, &mut disk);
        let m = lsm.metrics();
        assert!(m.value_files >= 1);
        // Compressed: a repeated text takes far less than its length.
        assert!(m.block_bytes_written[1] * 10 < m.block_bytes_written[0], "{m:?}");
        for p in disk.list(&cfg.dir).unwrap() {
            let bytes = disk.read_all(&p).unwrap();
            assert!(!bytes.windows(marker.len()).any(|w| w == marker), "{p:?} in clear");
        }
        drop(lsm);
        let mut lsm = open(&cfg, &mut disk);
        reads_return(&mut lsm, &mut disk, &docs, ts(20, 0));
    }
}

/// A value that changed in its value file is corruption, never another value.
#[test]
fn a_value_file_that_lost_its_value_fails_the_read() {
    let mut disk = teseladb_raftlog::SimDisk::default();
    let mut lsm = open(&cfg(), &mut disk);
    let mut index = 0;
    write_docs(&mut lsm, 3, 0, ts(10, 0), &mut index);
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    // The largest file of the directory is the value file: a byte of its first value flips.
    let path = disk
        .list(&cfg().dir)
        .unwrap()
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "sst"))
        .max_by_key(|p| disk.read_all(p).unwrap().len())
        .unwrap();
    drop(lsm);
    assert!(disk.corrupt(&path, 100));
    let mut lsm = open(&cfg(), &mut disk);
    let view = lsm.view();
    let failed = (0..100).find_map(|_| match view.get(&mut lsm, &key(0), ts(20, 0)) {
        teseladb_lsm::Lookup::Miss(_) => {
            lsm.run_io(&mut disk, Nanos::ZERO);
            None
        }
        other => Some(other),
    });
    assert!(matches!(failed, Some(teseladb_lsm::Lookup::Failed(_))), "{failed:?}");
}
