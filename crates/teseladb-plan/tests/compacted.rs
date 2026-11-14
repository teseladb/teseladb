//! The registry of statistics as the compactions feed it (010 FR-029): the ranges a compaction
//! rewrote wait once each for their statistics; a range published over keys another range held
//! replaces it (a split, a merge), and the ranges that left the node are forgotten. One test, in
//! its own process: the registry and the queue are the process's.

use teseladb_bson::Value;
use teseladb_plan::stats::{
    collection_stats, mark_compacted, recompute_range, retain_ranges, take_compacted,
};

fn ints(n: i32) -> Vec<Value> {
    (0..n).map(Value::Int32).collect()
}

fn span(a: &[u8], b: &[u8]) -> (Vec<u8>, Vec<u8>) {
    (a.to_vec(), b.to_vec())
}

#[test]
fn compacted_ranges_wait_once_and_their_statistics_follow_splits_merges_and_moves() {
    // Marked twice, a range waits once, with the interval it was marked with last.
    assert!(take_compacted().is_empty());
    mark_compacted(4, b"\x01a", b"\x01m");
    mark_compacted(2, b"\x01m", b"");
    mark_compacted(4, b"\x01a", b"\x01k");
    assert_eq!(
        take_compacted(),
        vec![(2, b"\x01m".to_vec(), Vec::new()), (4, b"\x01a".to_vec(), b"\x01k".to_vec())]
    );
    assert!(take_compacted().is_empty());

    let coll = b"\x01coll";
    // Range 1 holds the whole collection.
    recompute_range(coll, (1, span(b"\x01", b"")), 100, vec![("a".to_owned(), ints(100))]);
    assert_eq!(collection_stats(coll).expect("published").1.documents, 100);
    // It splits at `\x01m`: the new range 2 publishes its half, and range 1's whole is gone.
    recompute_range(coll, (2, span(b"\x01m", b"")), 40, vec![("a".to_owned(), ints(40))]);
    let s = collection_stats(coll).expect("published").1;
    assert_eq!(s.documents, 40);
    assert_eq!(s.fields["a"].count, 40);
    // Range 1 compacts its half again.
    recompute_range(coll, (1, span(b"\x01", b"\x01m")), 60, vec![("a".to_owned(), ints(60))]);
    let s = collection_stats(coll).expect("published").1;
    assert_eq!(s.documents, 100);
    assert_eq!(s.fields["a"].count, 100);
    // Range 2 moves to another node: only range 1 stays.
    retain_ranges(|r| r == 1);
    assert_eq!(collection_stats(coll).expect("published").1.documents, 60);
    // Range 1 absorbs a neighbour that was elsewhere and publishes the whole again.
    recompute_range(coll, (1, span(b"\x01", b"")), 90, vec![("a".to_owned(), ints(90))]);
    assert_eq!(collection_stats(coll).expect("published").1.documents, 90);
    // No range left: no statistics.
    retain_ranges(|_| false);
    assert!(collection_stats(coll).is_none());
}
