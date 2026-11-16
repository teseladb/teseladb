//! The hashing of the stack's lookup tables (connections, listeners, channels, fragments,
//! ARP entries, protocol handlers).
//!
//! These maps are looked up once per received packet, so their hash must be cheap. Rust's
//! default `RandomState` runs SipHash, a keyed cryptographic hash, on every lookup.
//! [`IdentityHash`] gives the identity hash of an integer-like key (the XOR of the integers it
//! writes, so a single integer hashes to itself), which the connection-id hash of the IP
//! layer combines. [`FxBuildHasher`] is the `BuildHasher` of the maps: it mixes the words with
//! one multiply per word (the FxHash step), because the Swiss table of `HashMap` takes its
//! control bits from the top of the hash, which an identity hash of a small integer leaves at
//! zero.
//!
//! The hash is not keyed: a peer that chooses its addresses and ports can force collisions.
//! The maps are per shard and the cost of a collision is a longer probe, never a wrong result.

use std::hash::{BuildHasherDefault, Hash, Hasher};

/// The multiplier of FxHash (rustc's hasher).
const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

/// FxHash: `h = (h.rotl(5) ^ word) * SEED` per word written.
#[derive(Default, Clone, Copy, Debug)]
pub struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(FX_SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for c in chunks {
            self.add(u64::from_ne_bytes(*c));
        }
        if !rest.is_empty() {
            let mut w = [0u8; 8];
            w[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_ne_bytes(w));
        }
    }
    #[inline]
    fn write_u8(&mut self, v: u8) {
        self.add(v as u64);
    }
    #[inline]
    fn write_u16(&mut self, v: u16) {
        self.add(v as u64);
    }
    #[inline]
    fn write_u32(&mut self, v: u32) {
        self.add(v as u64);
    }
    #[inline]
    fn write_u64(&mut self, v: u64) {
        self.add(v);
    }
    #[inline]
    fn write_usize(&mut self, v: usize) {
        self.add(v as u64);
    }
}

/// The `BuildHasher` of the stack's maps.
pub type FxBuildHasher = BuildHasherDefault<FxHasher>;

/// The identity hash of the integer-like keys of the stack: the XOR of the integers the
/// key's `Hash` writes, which is the identity for a single integer. Byte slices are folded
/// big-endian into the value (`h = h << 8 | byte`).
#[derive(Default, Clone, Copy, Debug)]
pub struct IdentityHash {
    hash: u64,
}

impl IdentityHash {
    /// The identity hash of `v`. Allocation-free; one XOR per integer written.
    #[inline]
    pub fn of<V: Hash + ?Sized>(v: &V) -> u64 {
        let mut h = IdentityHash::default();
        v.hash(&mut h);
        h.hash
    }
}

impl Hasher for IdentityHash {
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.hash = (self.hash << 8) | b as u64;
        }
    }
    #[inline]
    fn write_u8(&mut self, v: u8) {
        self.hash ^= v as u64;
    }
    #[inline]
    fn write_u16(&mut self, v: u16) {
        self.hash ^= v as u64;
    }
    #[inline]
    fn write_u32(&mut self, v: u32) {
        self.hash ^= v as u64;
    }
    #[inline]
    fn write_u64(&mut self, v: u64) {
        self.hash ^= v;
    }
    #[inline]
    fn write_usize(&mut self, v: usize) {
        self.hash ^= v as u64;
    }
}
