//! The ESC search reads ahead (017 SC-002): the documents its probes will read are asked for
//! together first, and the probes then read the same documents and find the same state as without
//! reading ahead.

use std::collections::{BTreeMap, BTreeSet};

use teseladb_fle::esc::{EscKeys, EscReader, EscState, Overlay, state};
use teseladb_fle::tokens::Token;

/// An ESC in memory that keeps what was read ahead and counts the rounds that reach it (one per
/// read ahead and one per probe that was not read ahead) and the documents they read.
#[derive(Default)]
struct Rounds {
    docs: BTreeMap<Vec<u8>, Vec<u8>>,
    ahead: BTreeSet<Vec<u8>>,
    rounds: usize,
    read: usize,
    probes: Vec<Vec<u8>>,
}

impl EscReader for Rounds {
    type Error = ();

    fn get(&mut self, id: &Token) -> Result<Option<Vec<u8>>, ()> {
        self.probes.push(id.0.to_vec());
        if !self.ahead.contains(id.0.as_slice()) {
            self.rounds += 1;
            self.read += 1;
        }
        Ok(self.docs.get(id.0.as_slice()).cloned())
    }

    fn read_ahead(&mut self, ids: &[Token]) -> Result<(), ()> {
        self.rounds += 1;
        self.read += ids.len();
        self.ahead.extend(ids.iter().map(|t| t.0.to_vec()));
        Ok(())
    }
}

/// The same documents, read one probe at a time.
struct OneByOne(BTreeMap<Vec<u8>, Vec<u8>>, Vec<Vec<u8>>);

impl EscReader for OneByOne {
    type Error = ();

    fn get(&mut self, id: &Token) -> Result<Option<Vec<u8>>, ()> {
        self.1.push(id.0.to_vec());
        Ok(self.0.get(id.0.as_slice()).cloned())
    }
}

fn keys() -> EscKeys {
    EscKeys::of(&Token([7; 32]))
}

/// An ESC with positions `1..=count` and, if given, a null anchor and anchors.
fn esc(count: u64, null: Option<(u64, u64)>, anchors: &[(u64, u64)]) -> Rounds {
    let k = keys();
    let mut r = Rounds::default();
    if let Some((a, c)) = null {
        r.docs.insert(k.anchor_id(0).0.to_vec(), k.anchor_value(a, c, [1; 16]));
    }
    for (a, c) in anchors {
        r.docs.insert(k.anchor_id(*a).0.to_vec(), k.anchor_value(0, *c, [2; 16]));
    }
    for p in 1..=count {
        r.docs.insert(k.position_id(p).0.to_vec(), Vec::new());
    }
    r
}

/// The state with and without reading ahead, which probe the same documents in the same order;
/// the rounds of the one that reads ahead, the documents it read and the probes.
fn both(mut r: Rounds) -> (EscState, usize, usize, usize) {
    let mut plain = OneByOne(r.docs.clone(), Vec::new());
    let expected = state(&mut plain, &keys()).unwrap();
    let got = state(&mut r, &keys()).unwrap();
    assert_eq!(got, expected);
    assert_eq!(r.probes, plain.1, "the same probes in the same order");
    (got, r.rounds, r.read, plain.1.len())
}

#[test]
fn a_short_esc_is_read_in_two_rounds_at_most() {
    for count in 0..=30 {
        let (s, rounds, read, probes) = both(esc(count, None, &[]));
        assert_eq!(s.count, count);
        assert!(rounds <= 2, "{count} positions: {rounds} rounds");
        // About twice the documents the probes read, never the whole run of positions.
        assert!(read <= 2 * probes + 8, "{count} positions: {read} read for {probes} probes");
    }
    // A value never written, or once, in one round: the first one holds its probes.
    assert_eq!(both(esc(0, None, &[])).1, 1);
}

#[test]
fn a_long_esc_reads_ahead_a_few_levels_a_round() {
    let (s, rounds, read, probes) = both(esc(1_000, None, &[]));
    assert_eq!(s.count, 1_000);
    // Doublings to 31, then to 1 023, then the binary search four levels a round: fewer rounds
    // than the ~20 probes and a few times their documents.
    assert!(rounds <= 6, "{rounds} rounds");
    assert!(read <= 4 * probes, "{read} read for {probes} probes");
}

#[test]
fn anchors_and_a_null_anchor_find_the_same_state() {
    let (s, ..) = both(esc(0, None, &[(1, 4), (2, 9)]));
    assert_eq!((s.apos, s.anchored), (2, 9));
    let mut r = esc(0, Some((2, 9)), &[(3, 12)]);
    let k = keys();
    for p in 13..=20 {
        r.docs.insert(k.position_id(p).0.to_vec(), Vec::new());
    }
    let (s, rounds, ..) = both(r);
    assert_eq!((s.null, s.apos, s.anchored, s.count), (Some((2, 9)), 3, 12, 20));
    assert!(rounds <= 5, "{rounds} rounds");
}

#[test]
fn an_overlay_reads_ahead_through_to_its_reader() {
    let mut r = esc(3, None, &[]);
    let mut overlay = Overlay::new(&mut r);
    overlay.write(keys().position_id(4));
    assert_eq!(state(&mut overlay, &keys()).unwrap().count, 4);
    // Positions 1, 3 and 7, then 5 to 6 between 3 and 7: the written 4 is not asked for.
    assert_eq!(r.rounds, 2);
    assert!(!r.ahead.contains(keys().position_id(4).0.as_slice()));
}
