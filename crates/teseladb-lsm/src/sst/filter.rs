//! Bloom filter of the logical keys of a file (FR-003, research R4): 10 bits per key and 7 probes
//! (about 1 % false positives), chosen by double hashing over two CRC32C values with different
//! seeds. Payload: `bits · probes(u8)`.

use teseladb_hash::{crc32c, crc32c_append};
use teseladb_types::Bytes;

use super::Bad;

/// Bits per key.
const BITS_PER_KEY: usize = 10;
/// Probes per key.
const PROBES: u8 = 7;
/// Seed of the second hash.
const SEED: u32 = 0x9e37_79b9;

/// The two hashes of a logical key.
pub(crate) fn hashes(key: &[u8]) -> (u32, u32) {
    (crc32c(key), crc32c_append(SEED, key) | 1)
}

/// Builds the payload of the filter of `keys` (their [`hashes`]).
pub(crate) fn build(keys: &[(u32, u32)]) -> Vec<u8> {
    let bits = (keys.len() * BITS_PER_KEY).max(64).next_multiple_of(8);
    let mut out = vec![0u8; bits / 8];
    for &(h1, h2) in keys {
        let mut h = h1;
        for _ in 0..PROBES {
            let bit = h as usize % bits;
            out[bit / 8] |= 1 << (bit % 8);
            h = h.wrapping_add(h2);
        }
    }
    out.push(PROBES);
    out
}

/// A filter read from its payload.
#[derive(Clone, Debug)]
pub(crate) struct Filter {
    bits: Bytes,
    probes: u8,
}

impl Filter {
    /// Reads a payload; the filter keeps pointing into its bytes.
    pub(crate) fn parse(payload: &Bytes) -> Result<Filter, Bad> {
        let (&probes, bits) = payload.split_last().ok_or(Bad)?;
        if bits.is_empty() || probes == 0 || probes > 30 {
            return Err(Bad);
        }
        Ok(Filter { bits: payload.slice(..bits.len()), probes })
    }

    /// Whether the key may be in the file (`false` means it is not).
    pub(crate) fn may_contain(&self, (h1, h2): (u32, u32)) -> bool {
        let bits = self.bits.len() * 8;
        let mut h = h1;
        for _ in 0..self.probes {
            let bit = h as usize % bits;
            if self.bits[bit / 8] & (1 << (bit % 8)) == 0 {
                return false;
            }
            h = h.wrapping_add(h2);
        }
        true
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn no_false_negatives_and_about_one_percent_false_positives() {
        let keys: Vec<Vec<u8>> = (0..10_000).map(|i| format!("user:{i}").into_bytes()).collect();
        let hashed: Vec<(u32, u32)> = keys.iter().map(|k| hashes(k)).collect();
        let f = Filter::parse(&Bytes::from_vec(build(&hashed))).unwrap();
        assert_eq!(f.bits.len(), 12_500);
        assert!(hashed.iter().all(|h| f.may_contain(*h)));
        let false_positives =
            (0..10_000).filter(|i| f.may_contain(hashes(format!("other:{i}").as_bytes()))).count();
        assert!(false_positives < 200, "{false_positives} in 10 000");
    }

    #[test]
    fn an_empty_filter_says_no_and_garbage_is_refused() {
        let f = Filter::parse(&Bytes::from_vec(build(&[]))).unwrap();
        assert!(!f.may_contain(hashes(b"x")));
        assert_eq!(Filter::parse(&Bytes::copy_from_slice(&[])).err(), Some(Bad));
        assert_eq!(Filter::parse(&Bytes::copy_from_slice(&[7])).err(), Some(Bad));
        assert_eq!(Filter::parse(&Bytes::copy_from_slice(&[0, 0])).err(), Some(Bad));
        assert_eq!(Filter::parse(&Bytes::copy_from_slice(&[0, 31])).err(), Some(Bad));
    }
}
