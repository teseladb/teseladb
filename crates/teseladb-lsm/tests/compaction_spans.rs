//! The keys a compaction rewrote (010 FR-029): each compaction that ends gives the lowest and
//! the highest logical key of the files it wrote, so that the range layer recomputes the
//! statistics of the ranges they fall in. A flush is no compaction and gives none.

mod common;

use common::{open, small, ts};
use teseladb_bson::{Document, RawValue};
use teseladb_keys::KeyBuilder;
use teseladb_lsm::{Completions, DOCUMENT_VALUE, Lsm, LsmConfig};
use teseladb_raftlog::MemDisk;
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);

fn cfg() -> LsmConfig {
    LsmConfig { memtable_bytes: 8 << 20, target_file_bytes: 8 << 20, ..small() }
}

fn key(collection: u32, i: i32) -> Vec<u8> {
    KeyBuilder::document(1, collection, RawValue::Int32(i)).as_bytes().to_vec()
}

fn doc(a: i32) -> Vec<u8> {
    let d = Document::new().with("a", a);
    [&[DOCUMENT_VALUE][..], &d.to_bytes().unwrap()].concat()
}

/// Runs the LSM's disk work until it has none, gathering what ended.
fn settle(lsm: &mut Lsm, disk: &mut MemDisk) -> Completions {
    let mut all = Completions::default();
    for _ in 0..64 {
        let c = lsm.run_io(disk, Nanos::ZERO);
        all.rewritten.extend(c.rewritten);
        all.flushed.extend(c.flushed);
    }
    all
}

#[test]
fn a_compaction_gives_the_keys_of_the_files_it_wrote_and_a_flush_none() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg(), &mut disk);
    let mut index = 0;
    let mut keys = Vec::new();
    for collection in [7, 9] {
        for i in 0..100 {
            index += 1;
            let k = key(collection, i);
            lsm.put(G, index, &k, ts(5, 0), &doc(i)).unwrap();
            keys.push(k);
        }
    }
    keys.sort();
    lsm.flush();
    let flushed = settle(&mut lsm, &mut disk);
    assert!(!flushed.flushed.is_empty(), "the memtable went to a file");
    assert!(flushed.rewritten.is_empty(), "{:?}", flushed.rewritten);
    lsm.compact_all();
    let compacted = settle(&mut lsm, &mut disk);
    assert!(!compacted.rewritten.is_empty(), "the compaction said what it wrote");
    let lowest = compacted.rewritten.iter().map(|(lo, _)| lo).min().unwrap();
    let highest = compacted.rewritten.iter().map(|(_, hi)| hi).max().unwrap();
    assert_eq!(lowest, &keys[0]);
    assert_eq!(highest, keys.last().unwrap());
    for (lo, hi) in &compacted.rewritten {
        assert!(lo <= hi, "{lo:?} > {hi:?}");
    }
}
