//! The encoding keeps RefDb's order (003 FR-008, FR-010, SC-002, T013): for every pair of a pool
//! of random values of every type, `cmp(enc(a), enc(b)) = compare(a, b)`, ascending, descending
//! (the order reversed) and in compound keys with mixed directions. 3,200 values give 10,240,000
//! pairs.

use std::cmp::Ordering;

mod common;

use common::{pool, raw};
use teseladb_bson::compare;
use teseladb_keys::{Direction, encode};

const POOL: usize = 3_200;

#[test]
fn every_pair_keeps_its_order_ascending_and_descending() {
    let docs = pool(1, POOL);
    let values: Vec<_> = docs.iter().map(|d| raw(d)).collect();
    let up: Vec<Vec<u8>> = values.iter().map(|v| encode(&[(*v, Direction::Ascending)]).0).collect();
    let down: Vec<Vec<u8>> =
        values.iter().map(|v| encode(&[(*v, Direction::Descending)]).0).collect();
    let mut equal = 0;
    for i in 0..values.len() {
        for j in 0..values.len() {
            let want = compare(values[i], values[j], None);
            assert_eq!(up[i].cmp(&up[j]), want, "{:?} against {:?}", values[i], values[j]);
            assert_eq!(
                down[i].cmp(&down[j]),
                want.reverse(),
                "{:?} against {:?} descending",
                values[i],
                values[j]
            );
            equal += usize::from(want == Ordering::Equal && i != j);
        }
    }
    // Equal values of different types or forms come up often enough to mean something.
    assert!(equal > 10_000, "{equal} equal pairs");
}

#[test]
fn compound_keys_with_mixed_directions() {
    let docs = pool(2, 1_200);
    let values: Vec<_> = docs.iter().map(|d| raw(d)).collect();
    let dirs = [Direction::Ascending, Direction::Descending];
    // Each key: two values of the pool, the first ascending and the second descending.
    let keys: Vec<(usize, usize, Vec<u8>)> = (0..values.len())
        .map(|i| {
            let j = (i * 7 + 3) % values.len();
            (i, j, encode(&[(values[i], dirs[0]), (values[j], dirs[1])]).0)
        })
        .collect();
    for (a1, a2, ka) in &keys {
        for (b1, b2, kb) in &keys {
            let want = compare(values[*a1], values[*b1], None)
                .then_with(|| compare(values[*a2], values[*b2], None).reverse());
            assert_eq!(ka.cmp(kb), want);
        }
    }
}
