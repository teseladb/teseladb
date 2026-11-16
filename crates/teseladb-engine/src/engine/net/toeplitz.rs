//! Toeplitz hash, the hash NICs use for receive-side scaling (RSS).
//!
//! The native stack computes the same hash in software to know which shard a NIC queue
//! delivers a flow to, so that connections it opens land on the shard that will receive
//! their packets. Two well-known default keys are provided.

/// An RSS hash key.
pub type RssKeyType = &'static [u8];

/// Bytes of the default 40-byte RSS key used by common Linux NIC drivers.
pub static DEFAULT_RSSKEY_40BYTES_V: [u8; 40] = [
    0xd1, 0x81, 0xc6, 0x2c, 0xf7, 0xf4, 0xdb, 0x5b, 0x19, 0x83, 0xa2, 0xfc, 0x94, 0x3e, 0x1a, 0xdb,
    0xd9, 0x38, 0x9e, 0x6b, 0xd1, 0x03, 0x9c, 0x2c, 0xa7, 0x44, 0x99, 0xad, 0x59, 0x3d, 0x56, 0xd9,
    0xf3, 0x25, 0x3c, 0x06, 0x2a, 0xdc, 0x1f, 0xfc,
];

/// The default 40-byte RSS key.
pub static DEFAULT_RSSKEY_40BYTES: RssKeyType = &DEFAULT_RSSKEY_40BYTES_V;

/// Bytes of the default 52-byte RSS key of i40e-family NICs.
pub static DEFAULT_RSSKEY_52BYTES_V: [u8; 52] = [
    0x44, 0x39, 0x79, 0x6b, 0xb5, 0x4c, 0x50, 0x23, 0xb6, 0x75, 0xea, 0x5b, 0x12, 0x4f, 0x9f, 0x30,
    0xb8, 0xa2, 0xc0, 0x3d, 0xdf, 0xdc, 0x4d, 0x02, 0xa0, 0x8c, 0x9b, 0x33, 0x4a, 0xf6, 0x4a, 0x4c,
    0x05, 0xc6, 0xfa, 0x34, 0x39, 0x58, 0xd8, 0x55, 0x7d, 0x99, 0x58, 0x3a, 0xe1, 0x38, 0xc9, 0x2e,
    0x81, 0x15, 0x03, 0x66,
];

/// The default 52-byte RSS key.
pub static DEFAULT_RSSKEY_52BYTES: RssKeyType = &DEFAULT_RSSKEY_52BYTES_V;

/// The input of a Toeplitz hash: a byte sequence with indexed access.
pub trait ToeplitzData {
    /// Number of bytes.
    fn size(&self) -> usize;
    /// Byte at index `i`.
    fn at(&self, i: usize) -> u8;
}

impl ToeplitzData for [u8] {
    fn size(&self) -> usize {
        self.len()
    }
    fn at(&self, i: usize) -> u8 {
        self[i]
    }
}

impl ToeplitzData for Vec<u8> {
    fn size(&self) -> usize {
        self.len()
    }
    fn at(&self, i: usize) -> u8 {
        self[i]
    }
}

/// Computes the Toeplitz hash of `data` under `key`.
///
/// The key must be at least 4 bytes long; bits beyond the end of the key shift in as zero.
/// O(8 * `data.size()`), allocation-free.
///
/// # Panics
/// If `key` is shorter than 4 bytes.
#[inline]
pub fn toeplitz_hash<T: ToeplitzData + ?Sized>(key: &[u8], data: &T) -> u32 {
    let mut hash: u32 = 0;
    let mut v: u32;

    // The key is expected to be at least `data.size() + 4` bytes long for every data bit to
    // be mixed with key bits.

    v = ((key[0] as u32) << 24)
        .wrapping_add((key[1] as u32) << 16)
        .wrapping_add((key[2] as u32) << 8)
        .wrapping_add(key[3] as u32);
    for i in 0..data.size() {
        for b in 0..8u32 {
            if data.at(i) as u32 & (1 << (7 - b)) != 0 {
                hash ^= v;
            }
            v <<= 1;
            if (i + 4) < key.len() && (key[i + 4] as u32 & (1 << (7 - b))) != 0 {
                v |= 1;
            }
        }
    }
    hash
}
