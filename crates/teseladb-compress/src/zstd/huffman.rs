//! Huffman-coded literals (RFC 8878, sections 4.2.1 and 4.2.2): the tree description, with its
//! weights written directly or compressed with FSE, and the decoding of one stream.

use crate::Error;
use crate::zstd::bits::BackReader;
use crate::zstd::fse::Table;

/// Longest prefix code.
const MAX_BITS: u32 = 11;
/// Accuracy limit of the FSE table that compresses the weights.
const WEIGHT_LOG: u32 = 6;

/// A decoding table indexed by the next `max_bits` bits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Huffman {
    max_bits: u32,
    /// `(symbol, code length)` per index.
    entries: Vec<(u8, u8)>,
}

impl Huffman {
    /// Reads a tree description at the start of `src`; the table and the bytes it took.
    pub(crate) fn read(src: &[u8]) -> Result<(Huffman, usize), Error> {
        let header = usize::from(*src.first().ok_or(Error::Truncated)?);
        let (weights, used) = if header >= 128 {
            let n = header - 127;
            let bytes = src.get(1..1 + n.div_ceil(2)).ok_or(Error::Truncated)?;
            let w =
                (0..n).map(|i| (bytes[i / 2] >> if i % 2 == 0 { 4 } else { 0 }) & 0xf).collect();
            (w, 1 + n.div_ceil(2))
        } else {
            let body = src.get(1..1 + header).ok_or(Error::Truncated)?;
            (fse_weights(body)?, 1 + header)
        };
        Ok((Huffman::from_weights(&weights)?, used))
    }

    /// The table of `weights`, the last symbol's weight being implied.
    fn from_weights(weights: &[u8]) -> Result<Huffman, Error> {
        if weights.len() > 255 || weights.iter().any(|&w| w > MAX_BITS as u8) {
            return Err(Error::Corrupt("Huffman weight out of range"));
        }
        let sum: u32 = weights.iter().filter(|&&w| w > 0).map(|&w| 1u32 << (w - 1)).sum();
        if sum == 0 {
            return Err(Error::Corrupt("Huffman tree without symbols"));
        }
        let max_bits = sum.ilog2() + 1;
        let rest = (1u32 << max_bits) - sum;
        if max_bits > MAX_BITS || !rest.is_power_of_two() {
            return Err(Error::Corrupt("Huffman weights do not form a tree"));
        }
        let mut all = weights.to_vec();
        all.push((rest.ilog2() + 1) as u8);
        let mut entries = vec![(0u8, 0u8); 1 << max_bits];
        let mut at = 0usize;
        for w in 1..=max_bits as u8 {
            for (s, _) in all.iter().enumerate().filter(|&(_, &x)| x == w) {
                let n = 1usize << (w - 1);
                let len = max_bits as u8 + 1 - w;
                entries[at..at + n].fill((s as u8, len));
                at += n;
            }
        }
        Ok(Huffman { max_bits, entries })
    }

    /// Decodes `n` symbols of the stream `src` into `out`; the stream must end exactly.
    pub(crate) fn decode(&self, src: &[u8], n: usize, out: &mut Vec<u8>) -> Result<(), Error> {
        let mut r = BackReader::new(src)?;
        for _ in 0..n {
            let (symbol, len) = self.entries[r.peek(self.max_bits) as usize];
            r.consume(u32::from(len));
            out.push(symbol);
        }
        match r.left() {
            0 => Ok(()),
            _ => Err(Error::Corrupt("Huffman stream length mismatch")),
        }
    }
}

/// Weights compressed with FSE: two interleaved states over one backward stream.
fn fse_weights(src: &[u8]) -> Result<Vec<u8>, Error> {
    let (table, used) = Table::read(src, WEIGHT_LOG, 255)?;
    let mut r = BackReader::new(src.get(used..).ok_or(Error::Truncated)?)?;
    let mut states = [table.init(&mut r), table.init(&mut r)];
    let mut out = Vec::new();
    let mut k = 0;
    loop {
        if out.len() >= 255 {
            return Err(Error::Corrupt("too many Huffman weights"));
        }
        out.push(table.cells[states[k]].symbol);
        states[k] = table.next(states[k], &mut r);
        if r.left() < 0 {
            out.push(table.cells[states[1 - k]].symbol);
            break;
        }
        k = 1 - k;
    }
    Ok(out)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn direct_weights_build_a_tree() {
        // Weights 1, 1, 2 for symbols 0..2; symbol 3 gets the implied weight 3 (sum 4 → 8).
        let (h, used) = Huffman::read(&[127 + 3, 0x11, 0x20]).unwrap();
        assert_eq!(used, 3);
        assert_eq!(h.max_bits, 3);
        assert_eq!(h.entries[0], (0, 3));
        assert_eq!(h.entries[1], (1, 3));
        assert_eq!(h.entries[2], (2, 2));
        assert_eq!(h.entries[4], (3, 1));
    }

    #[test]
    fn rejects_bad_trees() {
        assert!(Huffman::read(&[]).is_err());
        assert!(Huffman::read(&[127 + 2, 0x00]).is_err());
        assert!(Huffman::read(&[127 + 2, 0xc0]).is_err());
        // Weights 3 and 1 sum 5, which leaves 3 to reach 8: not a power of two.
        assert!(Huffman::read(&[127 + 2, 0x31]).is_err());
        assert!(Huffman::read(&[127 + 4]).is_err());
        assert!(Huffman::read(&[5, 1]).is_err());
    }

    #[test]
    fn too_long_codes_and_random_trees() {
        // 128 weights of 11 sum far past 2^11: the codes would be longer than 11 bits.
        let mut d = vec![127 + 128];
        d.extend(std::iter::repeat_n(0xbb, 64));
        assert!(Huffman::read(&d).is_err());
        // Random FSE-compressed descriptions are refused or give a full table.
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut ok = 0;
        for _ in 0..100_000 {
            let mut bytes = [0u8; 40];
            for b in &mut bytes {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *b = state as u8;
            }
            bytes[0] %= 40;
            if let Ok((h, _)) = Huffman::read(&bytes) {
                assert_eq!(h.entries.len(), 1 << h.max_bits);
                ok += 1;
            }
        }
        assert!(ok > 0);
    }

    #[test]
    fn stream_must_end_exactly() {
        let (h, _) = Huffman::read(&[127 + 3, 0x11, 0x20]).unwrap();
        let mut out = Vec::new();
        // Mark at bit 1 and one bit 0: "0" is only the prefix of a 3-bit code.
        assert!(h.decode(&[0b10], 1, &mut out).is_err());
        out.clear();
        // Bits `1` (symbol 3) then the mark.
        h.decode(&[0b11], 1, &mut out).unwrap();
        assert_eq!(out, [3]);
    }
}
