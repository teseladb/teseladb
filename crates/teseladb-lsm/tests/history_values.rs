//! The values that only serve the history go whole to the history files (004 FR-047): a flush
//! and a compaction keep the change metas of the documents (014) out of their files, where no
//! scan of documents wants them, and the scans by time read them from the history files with
//! the rest of the history. In the one-node load a scan of the cores took 6.9 entries of its
//! sources for each document it gave, 3.6 of them versions of its metas (run p63).

mod common;

use common::{open, small, ts};
use teseladb_lsm::{
    Commit, DOCUMENT_VALUE, GcTable, HISTORY_VALUE, Kind, Lsm, LsmConfig, ScanCounts, Step,
};
use teseladb_raftlog::MemDisk;
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);
const DOCS: u64 = 50;

fn doc(i: u64) -> Vec<u8> {
    format!("d{i:04}").into_bytes()
}

/// The key of the meta of document `i`: right after it, as the change streams key theirs.
fn meta(i: u64) -> Vec<u8> {
    [doc(i).as_slice(), &[0, b'm']].concat()
}

fn config() -> LsmConfig {
    LsmConfig { memtable_bytes: 4 << 20, ..small() }
}

/// Versions `from..=to` of every document and of its meta, each at the mark of its number.
fn write(lsm: &mut Lsm, index: &mut u64, (from, to): (u64, u64)) {
    for version in from..=to {
        for i in 0..DOCS {
            let v = u8::try_from(version).unwrap();
            *index += 2;
            lsm.put(G, *index - 1, &doc(i), ts(version, 0), &[DOCUMENT_VALUE, v]).unwrap();
            lsm.put(G, *index, &meta(i), ts(version, 0), &[HISTORY_VALUE, v]).unwrap();
        }
    }
}

fn flush(lsm: &mut Lsm, disk: &mut MemDisk) {
    lsm.flush();
    lsm.run_io(disk, Nanos::ZERO);
}

fn compact(lsm: &mut Lsm, disk: &mut MemDisk) {
    let id = lsm.compact_now();
    assert_eq!(lsm.run_io(disk, Nanos::ZERO).compacted, vec![id]);
}

/// What a scan of every key at mark `at` gives, with its kinds, and what it counted.
fn scan(lsm: &mut Lsm, disk: &mut MemDisk, at: u64) -> (Vec<(Vec<u8>, Kind)>, ScanCounts) {
    let view = lsm.view();
    let mut s = view.scan(b"d", b"e", ts(at, 0));
    let mut items = Vec::new();
    loop {
        match s.next(lsm) {
            Step::Item(i) => items.push((i.key, i.kind)),
            Step::Miss(_) => {
                lsm.run_io(disk, Nanos::ZERO);
            }
            Step::Done => return (items, s.counts()),
            Step::Failed(e) => panic!("{e}"),
        }
    }
}

/// The history values written in `(from, to]`, as a scan by time gives them: key and mark.
fn metas_between(lsm: &mut Lsm, disk: &mut MemDisk, (from, to): (u64, u64)) -> Vec<(Vec<u8>, u64)> {
    let view = lsm.view();
    let mut s = view.scan_time(b"d", b"e", ts(from, 0), ts(to, 0));
    let mut seen = Vec::new();
    loop {
        match s.next(lsm) {
            Step::Item(i) => {
                if i.value.first() == Some(&HISTORY_VALUE) {
                    seen.push((i.key, i.ts.physical));
                }
            }
            Step::Miss(_) => {
                lsm.run_io(disk, Nanos::ZERO);
            }
            Step::Done => return seen,
            Step::Failed(e) => panic!("{e}"),
        }
    }
}

/// Every meta at each of `marks`, the newest first, in key order.
fn every_meta(marks: &[u64]) -> Vec<(Vec<u8>, u64)> {
    (0..DOCS).flat_map(|i| marks.iter().map(move |m| (meta(i), *m))).collect()
}

fn documents() -> Vec<(Vec<u8>, Kind)> {
    (0..DOCS).map(|i| (doc(i), Kind::Put)).collect()
}

#[test]
fn a_flush_keeps_the_metas_out_of_its_file() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&config(), &mut disk);
    let mut index = 0;
    write(&mut lsm, &mut index, (1, 3));
    flush(&mut lsm, &mut disk);
    // A scan of documents reads a version of each and nothing else: no meta to pass over.
    let (given, counted) = scan(&mut lsm, &mut disk, 10);
    assert_eq!(given, documents());
    assert_eq!((counted.entries, counted.passed), (DOCS, 0), "{counted:?}");
    // The scans by time give every one of them.
    assert_eq!(metas_between(&mut lsm, &mut disk, (0, 10)), every_meta(&[3, 2, 1]));
    assert_eq!(metas_between(&mut lsm, &mut disk, (1, 2)), every_meta(&[2]));
}

#[test]
fn a_compaction_keeps_them_apart_and_the_threshold_frees_them() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&config(), &mut disk);
    let mut index = 0;
    write(&mut lsm, &mut index, (1, 2));
    flush(&mut lsm, &mut disk);
    write(&mut lsm, &mut index, (3, 4));
    flush(&mut lsm, &mut disk);
    compact(&mut lsm, &mut disk);
    let (given, counted) = scan(&mut lsm, &mut disk, 10);
    assert_eq!(given, documents());
    assert_eq!((counted.entries, counted.passed), (DOCS, 0), "{counted:?}");
    assert_eq!(metas_between(&mut lsm, &mut disk, (0, 10)), every_meta(&[4, 3, 2, 1]));
    // Reopened, the files and their history are the same.
    drop(lsm);
    let mut lsm = open(&config(), &mut disk);
    assert_eq!(metas_between(&mut lsm, &mut disk, (0, 10)), every_meta(&[4, 3, 2, 1]));
    // At or below the threshold every such version goes, the newest too (FR-018).
    lsm.set_gc_thresholds(GcTable::new(vec![(b"d".to_vec(), b"e".to_vec(), ts(3, 0))]));
    compact(&mut lsm, &mut disk);
    assert_eq!(metas_between(&mut lsm, &mut disk, (0, 10)), every_meta(&[4]));
    assert_eq!(scan(&mut lsm, &mut disk, 10).0, documents());
}

#[test]
fn an_intent_on_the_key_of_a_meta_stays_in_the_file() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&config(), &mut disk);
    let mut index = 0;
    write(&mut lsm, &mut index, (1, 1));
    // A transaction that has not ended wrote document 7: the intent on its meta is no history
    // value yet, and the scan that meets it decides what to do with its transaction.
    lsm.put_intent(G, index + 1, &meta(7), ts(2, 0), b"txn").unwrap();
    flush(&mut lsm, &mut disk);
    let (given, _) = scan(&mut lsm, &mut disk, 10);
    let mut want = documents();
    want.insert(8, (meta(7), Kind::Intent));
    assert_eq!(given, want);
}

#[test]
fn a_file_whose_only_versions_are_metas_is_written_and_read() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&config(), &mut disk);
    // Metas alone, in two flushes and a compaction of both: each output has only what goes to
    // its history file.
    for (index, version) in [(0u64, 1u64), (1_000, 2)] {
        for i in 0..DOCS {
            let v = u8::try_from(version).unwrap();
            lsm.put(G, index + i + 1, &meta(i), ts(version, 0), &[HISTORY_VALUE, v]).unwrap();
        }
        flush(&mut lsm, &mut disk);
    }
    assert_eq!(metas_between(&mut lsm, &mut disk, (0, 10)), every_meta(&[2, 1]));
    compact(&mut lsm, &mut disk);
    assert_eq!(metas_between(&mut lsm, &mut disk, (0, 10)), every_meta(&[2, 1]));
    let (given, counted) = scan(&mut lsm, &mut disk, 10);
    assert!(given.is_empty() && counted.passed == 0, "{given:?} {counted:?}");
    drop(lsm);
    let mut lsm = open(&config(), &mut disk);
    assert_eq!(metas_between(&mut lsm, &mut disk, (0, 10)), every_meta(&[2, 1]));
}

/// A transaction may commit at the mark of its intents (FR-007): the commit of a meta is
/// written over the key of its intent, which an older file still holds, and hides it by being
/// in a newer source. A flush keeps that meta in its file, where the scan walks past it and
/// gives no intent; the compaction that leaves nothing below its key sends it apart.
#[test]
fn a_meta_that_commits_at_the_mark_of_its_intent_stays_over_it_until_nothing_is_below() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&config(), &mut disk);
    lsm.put_intent(G, 1, &doc(1), ts(5, 0), b"txn").unwrap();
    lsm.put_intent(G, 2, &meta(1), ts(5, 0), b"txn").unwrap();
    flush(&mut lsm, &mut disk);
    let commit = |value: &'static [u8]| Some(Commit { ts: ts(5, 0), kind: Kind::Put, value });
    lsm.resolve_intent(G, 3, &doc(1), ts(5, 0), commit(&[DOCUMENT_VALUE, 5])).unwrap();
    lsm.resolve_intent(G, 4, &meta(1), ts(5, 0), commit(&[HISTORY_VALUE, 5])).unwrap();
    flush(&mut lsm, &mut disk);
    let one = vec![(doc(1), Kind::Put)];
    let (given, counted) = scan(&mut lsm, &mut disk, 10);
    assert_eq!(given, one, "the document, and no intent");
    assert_eq!(counted.passed, 1, "the meta is in its file, over the intent: {counted:?}");
    assert_eq!(metas_between(&mut lsm, &mut disk, (0, 10)), [(meta(1), 5)]);
    compact(&mut lsm, &mut disk);
    let (given, counted) = scan(&mut lsm, &mut disk, 10);
    assert_eq!(given, one);
    assert_eq!(
        (counted.entries, counted.passed),
        (1, 0),
        "apart with the intent gone: {counted:?}"
    );
    assert_eq!(metas_between(&mut lsm, &mut disk, (0, 10)), [(meta(1), 5)]);
}

/// Under the keys of the embedder's own records ([`teseladb_lsm::RAW_KEYS`]) a value is raw
/// bytes: one that starts with the byte of the history values is read by key and by a scan, in
/// the memtable and once flushed and compacted, and the threshold does not free it. A range
/// keeps its counters and marks there, and one of them may start with any byte.
#[test]
fn a_raw_record_that_starts_with_the_history_byte_is_read_as_any_other() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&config(), &mut disk);
    let key = [teseladb_lsm::RAW_KEYS, 7, 1];
    let value = [HISTORY_VALUE, 0, 0, 9];
    let read = |lsm: &mut Lsm, disk: &mut MemDisk| {
        let view = lsm.view();
        let by_key = common::get(lsm, disk, &view, &key, ts(50, 0)).map(|r| r.3);
        let mut s = view.scan(&[teseladb_lsm::RAW_KEYS], &[teseladb_lsm::RAW_KEYS + 1], ts(50, 0));
        let mut by_scan = Vec::new();
        loop {
            match s.next(lsm) {
                Step::Item(i) => by_scan.push(i.value.to_vec()),
                Step::Miss(_) => {
                    lsm.run_io(disk, Nanos::ZERO);
                }
                Step::Done => return (by_key, by_scan),
                Step::Failed(e) => panic!("{e}"),
            }
        }
    };
    let want = (Some(value.to_vec()), vec![value.to_vec()]);
    lsm.put(G, 1, &key, ts(1, 0), &[HISTORY_VALUE, 0, 0, 8]).unwrap();
    lsm.put(G, 2, &key, ts(2, 0), &value).unwrap();
    assert_eq!(read(&mut lsm, &mut disk), want, "in the memtable");
    flush(&mut lsm, &mut disk);
    assert_eq!(read(&mut lsm, &mut disk), want, "flushed");
    lsm.set_gc_thresholds(GcTable::new(vec![(vec![0], vec![1], ts(40, 0))]));
    compact(&mut lsm, &mut disk);
    assert_eq!(read(&mut lsm, &mut disk), want, "compacted, under the threshold");
}
