//! Zone maps (004 FR-039): a scan with a filter passes over the data blocks whose zones the
//! filter rules out, and never one whose versions would let an older version of a key show
//! through: a deeper level, an older file of level 0, or the file's own history file when the
//! read opens it.

mod common;

use common::{open, small, ts};
use teseladb_bson::{Document, RawValue};
use teseladb_keys::zone::{Bounds, Zones};
use teseladb_keys::{Direction, KeyBuilder, TypeBits, encode_value};
use teseladb_lsm::{DOCUMENT_VALUE, Kind, Lsm, LsmConfig, Step};
use teseladb_raftlog::MemDisk;
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);
/// The filter of every scan here: `{a: {$gte: 100}}`.
const AT_LEAST: i32 = 100;
const DOCS: i32 = 300;

fn cfg() -> LsmConfig {
    LsmConfig { memtable_bytes: 8 << 20, target_file_bytes: 8 << 20, ..small() }
}

fn key(i: i32) -> Vec<u8> {
    KeyBuilder::document(1, 7, RawValue::Int32(i)).as_bytes().to_vec()
}

fn doc(a: i32) -> Vec<u8> {
    let d = Document::new().with("a", a).with("pad", "p".repeat(60));
    [&[DOCUMENT_VALUE][..], &d.to_bytes().unwrap()].concat()
}

fn a_of(stored: &[u8]) -> i32 {
    let d = teseladb_bson::RawDoc::from_bytes(&stored[1..]).unwrap();
    match d.get("a") {
        Some(RawValue::Int32(a)) => a,
        other => panic!("{other:?}"),
    }
}

fn encoded(a: i32) -> Vec<u8> {
    let mut out = Vec::new();
    encode_value(&mut out, RawValue::Int32(a), Direction::Ascending, &mut TypeBits::default());
    out
}

/// Whether a block may hold a document with `a >= 100`.
fn may_match(z: &dyn Zones) -> bool {
    match z.field("a") {
        Bounds::Within { max, .. } => max >= encoded(AT_LEAST).as_slice(),
        Bounds::Absent => false,
        Bounds::Unknown => true,
    }
}

/// Every visible version of the collection at `at`, as `(id key, a)` (a deletion shows nothing),
/// with the filter's blocks passed over when `pruned`; and the blocks passed over.
fn scan(lsm: &mut Lsm, disk: &mut MemDisk, at: u64, pruned: bool) -> (Vec<(Vec<u8>, i32)>, u64) {
    let view = lsm.view();
    let start = KeyBuilder::collection_start(1, 7);
    let end = KeyBuilder::collection_end(1, 7);
    let mut s = view.scan(start.as_bytes(), end.as_bytes(), ts(at, 0));
    let mut rows = Vec::new();
    loop {
        let step = match pruned {
            true => s.next_pruned(lsm, &may_match),
            false => s.next(lsm),
        };
        match step {
            Step::Item(i) => rows.push((i.key, a_of(&i.value))),
            Step::Miss(_) => {
                lsm.run_io(disk, Nanos::ZERO);
            }
            Step::Done => return (rows, s.skipped_blocks()),
            Step::Failed(e) => panic!("{e}"),
        }
    }
}

/// What the filter keeps of a scan.
fn kept(rows: &[(Vec<u8>, i32)]) -> Vec<(Vec<u8>, i32)> {
    rows.iter().filter(|(_, a)| *a >= AT_LEAST).cloned().collect()
}

fn flush(lsm: &mut Lsm, disk: &mut MemDisk) {
    lsm.flush();
    lsm.run_io(disk, Nanos::ZERO);
}

#[test]
fn a_filtered_scan_passes_over_the_blocks_its_filter_rules_out() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg(), &mut disk);
    // `a` grows with the key (keys are spread by their bucket, not by `i`): only the blocks at
    // the end hold values of 100 and above.
    let mut keys: Vec<Vec<u8>> = (0..DOCS).map(key).collect();
    keys.sort();
    for (rank, k) in keys.iter().enumerate() {
        lsm.put(G, rank as u64 + 1, k, ts(5, 0), &doc(rank as i32 / 2)).unwrap();
    }
    flush(&mut lsm, &mut disk);
    let (all, none_skipped) = scan(&mut lsm, &mut disk, 9, false);
    assert_eq!(all.len(), DOCS as usize);
    assert_eq!(none_skipped, 0);
    let (pruned, skipped) = scan(&mut lsm, &mut disk, 9, true);
    assert!(skipped >= 3, "{skipped} blocks passed over");
    assert_eq!(kept(&pruned), kept(&all));
}

/// The newest version of every key, in a newer file, does not match; the older one, in an
/// older file, does. Passing over the newer file's blocks would show the older version.
#[test]
fn an_older_file_with_the_same_keys_keeps_the_newer_blocks_read() {
    for deleted in [false, true] {
        let mut disk = MemDisk::new();
        let mut lsm = open(&cfg(), &mut disk);
        let mut index = 0;
        for i in 0..DOCS {
            index += 1;
            lsm.put(G, index, &key(i), ts(2, 0), &doc(500)).unwrap();
        }
        flush(&mut lsm, &mut disk);
        for i in 0..DOCS {
            index += 1;
            match deleted {
                true => lsm.delete(G, index, &key(i), ts(4, 0)).unwrap(),
                false => lsm.put(G, index, &key(i), ts(4, 0), &doc(1)).unwrap(),
            }
        }
        flush(&mut lsm, &mut disk);
        let (all, _) = scan(&mut lsm, &mut disk, 9, false);
        let (pruned, skipped) = scan(&mut lsm, &mut disk, 9, true);
        assert_eq!(kept(&pruned), kept(&all), "deleted: {deleted}");
        assert!(kept(&pruned).is_empty(), "deleted: {deleted}");
        assert_eq!(skipped, 0, "deleted: {deleted}");
        // After a compaction the older versions are gone from the file the scan reads
        // first, or sit in its history file, which a read above its highest mark leaves closed.
        lsm.compact_all();
        lsm.run_io(&mut disk, Nanos::ZERO);
        let (after, skipped) = scan(&mut lsm, &mut disk, 9, true);
        assert!(kept(&after).is_empty(), "deleted: {deleted}");
        // The deletions with nothing under them leave with the compaction.
        assert_eq!(skipped > 0, !deleted, "deleted: {deleted}: the file alone holds the keys");
    }
}

/// A read below the file's highest mark opens its history file, whose older versions would
/// show through the blocks passed over.
#[test]
fn the_history_file_a_read_opens_keeps_the_file_s_blocks_read() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg(), &mut disk);
    let mut index = 0;
    for (at, a) in [(2, 500), (4, 1)] {
        for i in 0..DOCS {
            index += 1;
            lsm.put(G, index, &key(i), ts(at, 0), &doc(a)).unwrap();
        }
    }
    // A key far after the others, at a later mark: the file's highest mark is above the read.
    lsm.put(G, index + 1, &key(1_000_000), ts(8, 0), &doc(7)).unwrap();
    flush(&mut lsm, &mut disk);
    let (all, _) = scan(&mut lsm, &mut disk, 6, false);
    assert!(kept(&all).is_empty());
    let (pruned, skipped) = scan(&mut lsm, &mut disk, 6, true);
    assert_eq!(kept(&pruned), kept(&all));
    assert_eq!(skipped, 0, "the history file is open");
    // At the file's highest mark the history file stays closed and the blocks are passed over.
    let (closed, skipped) = scan(&mut lsm, &mut disk, 8, true);
    assert!(skipped > 0);
    assert!(kept(&closed).is_empty());
    // Below every newest version the history file shows the older ones, which match.
    let (old, _) = scan(&mut lsm, &mut disk, 3, true);
    assert_eq!(kept(&old).len(), DOCS as usize);
}

/// A block with an intent is read whatever its zones say: the scan must see the intent.
#[test]
fn a_block_with_an_intent_is_read() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg(), &mut disk);
    for i in 0..DOCS {
        lsm.put(G, i as u64 + 1, &key(i), ts(2, 0), &doc(1)).unwrap();
    }
    lsm.put_intent(G, DOCS as u64 + 1, &key(5), ts(3, 0), b"intent").unwrap();
    flush(&mut lsm, &mut disk);
    let view = lsm.view();
    let (start, end) = (KeyBuilder::collection_start(1, 7), KeyBuilder::collection_end(1, 7));
    let mut s = view.scan(start.as_bytes(), end.as_bytes(), ts(9, 0));
    loop {
        match s.next_pruned(&mut lsm, &may_match) {
            Step::Item(i) if i.kind == Kind::Intent => break,
            Step::Item(_) => {}
            Step::Miss(_) => {
                lsm.run_io(&mut disk, Nanos::ZERO);
            }
            Step::Done => panic!("the intent was passed over"),
            Step::Failed(e) => panic!("{e}"),
        }
    }
}

/// The scan of the collection from another thread's side: over the published view, with the
/// blocks it misses read from `disk` and given to the shared cache.
fn shared_scan(lsm: &Lsm, disk: &mut MemDisk, pruned: bool) -> (Vec<(Vec<u8>, i32)>, u64) {
    use teseladb_lsm::SharedStep;
    let shared = lsm.shared();
    let view = shared.view();
    let start = KeyBuilder::collection_start(1, 7);
    let end = KeyBuilder::collection_end(1, 7);
    let mut s = view.scan(start.as_bytes(), end.as_bytes(), ts(9, 0));
    let mut rows = Vec::new();
    loop {
        let step = match pruned {
            true => s.next_pruned(&shared, &may_match),
            false => s.next(&shared),
        };
        match step {
            SharedStep::Item(i) => rows.push((i.key, a_of(&i.value))),
            SharedStep::Need(need) => {
                let all = teseladb_raftlog::Disk::read_all(disk, std::path::Path::new(&need.path));
                let all = all.unwrap();
                let at = need.offset as usize;
                let bytes = &all[at..at + need.len as usize];
                // A block that is not the one asked for is refused and the cache keeps nothing.
                // The pages also hold the ends of the blocks next to it (FR-043): every byte
                // changes.
                let damaged: Vec<u8> = bytes.iter().map(|b| b ^ 0xFF).collect();
                assert!(shared.supply(&need, &damaged).is_err());
                shared.supply(&need, bytes).unwrap();
            }
            SharedStep::Done => return (rows, s.skipped_blocks()),
            SharedStep::Failed(e) => panic!("{e}"),
        }
    }
}

#[test]
fn a_filtered_scan_from_another_thread_passes_over_the_same_blocks() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg(), &mut disk);
    let mut keys: Vec<Vec<u8>> = (0..DOCS).map(key).collect();
    keys.sort();
    for (rank, k) in keys.iter().enumerate() {
        lsm.put(G, rank as u64 + 1, k, ts(5, 0), &doc(rank as i32 / 2)).unwrap();
    }
    flush(&mut lsm, &mut disk);
    lsm.publish();
    let (all, none_skipped) = shared_scan(&lsm, &mut disk, false);
    assert_eq!(all.len(), DOCS as usize);
    assert_eq!(none_skipped, 0);
    let (pruned, skipped) = shared_scan(&lsm, &mut disk, true);
    let (_, skipped_by_the_core) = scan(&mut lsm, &mut disk, 9, true);
    assert_eq!(skipped, skipped_by_the_core);
    assert!(skipped >= 3, "{skipped} blocks passed over");
    assert_eq!(kept(&pruned), kept(&all));
}
