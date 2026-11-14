//! The bytes a memtable counts against its budget (004 FR-038, 012 FR-013) are those it holds
//! from the allocator: the test allocator counts the bytes the process holds while a core's LSM
//! takes versions of the sizes a node writes, and the memtable's count is never below them nor
//! more than a tenth above. Before the count came from the radix tree it was a fixed 64 bytes per
//! version besides the key and the value, the figure of the skiplist, and with a leaf keeping the
//! sort key and the internal key it was below what the memtable held.

use std::sync::Mutex;

use teseladb_keys::Hlc;
use teseladb_lsm::{Lsm, LsmConfig};
use teseladb_sys::alloc_count::{CountingAlloc, held_bytes};
use teseladb_raftlog::MemDisk;
use teseladb_types::GroupId;

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// One test measures at a time, so that the other's memory does not count.
static MEASURING: Mutex<()> = Mutex::new(());

/// Versions written in each case.
const VERSIONS: usize = 20_000;

/// How far above the bytes held the count may be: a tenth.
const MOST_ABOVE: f64 = 1.10;

/// The next number of a xorshift generator.
fn next(rng: &mut u64) -> u64 {
    *rng ^= *rng << 13;
    *rng ^= *rng >> 7;
    *rng ^= *rng << 17;
    *rng
}

/// A number in `[lo, hi]`.
fn between(rng: &mut u64, lo: usize, hi: usize) -> usize {
    lo + (next(rng) % (hi - lo + 1) as u64) as usize
}

/// A document key: a collection prefix and a document id, 20 to 40 bytes, zero bytes included.
fn document_key(rng: &mut u64) -> Vec<u8> {
    let mut k = vec![0, 0, 0, 7, b'd'];
    let len = between(rng, 20, 40);
    while k.len() < len {
        k.push((next(rng) % 256) as u8);
    }
    k
}

/// An index key: an index prefix, an encoded field value and the document id it points to.
fn index_key(rng: &mut u64) -> Vec<u8> {
    let mut k = vec![0, 0, 0, 7, b'i', 0, 3];
    let field = between(rng, 4, 24);
    k.extend((0..field).map(|_| b'a' + (next(rng) % 26) as u8));
    k.push(0);
    k.extend((0..12).map(|_| (next(rng) % 256) as u8));
    k
}

/// Writes `versions` into a fresh LSM and gives the bytes the process came to hold and the
/// bytes its memtable counts.
fn measure(versions: &[(Vec<u8>, Vec<u8>)]) -> (u64, u64) {
    let cfg = LsmConfig { memtable_bytes: u64::MAX, ..LsmConfig::default() };
    let mut disk = MemDisk::new();
    let (mut lsm, _) = Lsm::recover(&cfg, &mut disk).expect("an empty LSM");
    let counted_before = lsm.metrics().memtable_bytes;
    let held_before = held_bytes();
    for (i, (key, value)) in versions.iter().enumerate() {
        let ts = Hlc { physical: 1_000 + i as u64, logical: 0 };
        lsm.put(GroupId(1), i as u64 + 1, key, ts, value).expect("a write");
    }
    let held = held_bytes() - held_before;
    let counted = lsm.metrics().memtable_bytes - counted_before;
    (held as u64, counted)
}

/// Builds the versions and measures them while no other test allocates.
fn check(name: &str, build: impl FnOnce() -> Vec<(Vec<u8>, Vec<u8>)>) {
    let _one = MEASURING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let versions = build();
    let (held, counted) = measure(&versions);
    let per_version = |b: u64| b as f64 / versions.len() as f64;
    assert!(
        counted >= held,
        "{name}: the memtable counts {counted} bytes and holds {held} ({:.1} and {:.1} per version)",
        per_version(counted),
        per_version(held),
    );
    assert!(
        counted as f64 <= held as f64 * MOST_ABOVE,
        "{name}: the memtable counts {counted} bytes and holds {held}",
    );
}

#[test]
fn documents_of_100_bytes_to_4_kib_count_what_they_hold() {
    check("documents", || {
        let mut rng = 0x00D0_C0DE;
        (0..VERSIONS)
            .map(|_| (document_key(&mut rng), vec![7; between(&mut rng, 100, 4096)]))
            .collect()
    });
}

#[test]
fn small_documents_count_what_they_hold() {
    check("documents of 100 bytes", || {
        let mut rng = 0x5A11;
        (0..VERSIONS).map(|_| (document_key(&mut rng), vec![7; 100])).collect()
    });
}

#[test]
fn index_entries_count_what_they_hold() {
    check("index entries", || {
        let mut rng = 0x1DE7;
        (0..VERSIONS).map(|_| (index_key(&mut rng), Vec::new())).collect()
    });
}

#[test]
fn versions_of_the_same_documents_count_what_they_hold() {
    // A few hundred documents written again and again: many versions under each key.
    check("versions", || {
        let mut rng = 0x7E57;
        let docs: Vec<Vec<u8>> = (0..300).map(|_| document_key(&mut rng)).collect();
        (0..VERSIONS)
            .map(|_| {
                let doc = docs[between(&mut rng, 0, docs.len() - 1)].clone();
                (doc, vec![7; between(&mut rng, 100, 1024)])
            })
            .collect()
    });
}
