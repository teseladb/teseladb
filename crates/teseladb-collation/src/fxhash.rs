//! A fast hash for the collation tables (the multiply-and-rotate hash of rustc's `FxHasher`).
//! The tables are built from the committed data, never from what a client sends, so a hash that
//! resists flooding buys nothing here; looking up code points is most of the cost of a sort key.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

/// The hasher.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FxHasher {
    hash: u64,
}

impl FxHasher {
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for word in words {
            self.add(u64::from_le_bytes(*word));
        }
        for &b in rest {
            self.add(u64::from(b));
        }
    }

    fn write_u32(&mut self, n: u32) {
        self.add(u64::from(n));
    }

    fn write_usize(&mut self, n: usize) {
        self.add(n as u64);
    }

    fn finish(&self) -> u64 {
        self.hash
    }
}

/// A map with the fast hash.
pub(crate) type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;
/// A set with the fast hash.
pub(crate) type FxSet<T> = HashSet<T, BuildHasherDefault<FxHasher>>;
