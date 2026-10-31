//! The Zstandard encoder: one frame with content size and checksum, blocks of at most 128 KiB
//! with raw literals and sequences coded with the predefined distributions.

use crate::zstd::bits::BitWriter;
use crate::zstd::frame::{BLOCK_COMPRESSED, BLOCK_RAW, CHECKSUM, MAGIC, SINGLE_SEGMENT};
use crate::zstd::fse::{Encoder, Table};
use crate::zstd::sequences::{
    BLOCK_MAX, LL_BASE, LL_BITS, LL_DEFAULT, LL_DEFAULT_LOG, ML_BASE, ML_BITS, ML_DEFAULT,
    ML_DEFAULT_LOG, MODE_PREDEFINED, OF_DEFAULT, OF_DEFAULT_LOG,
};
use crate::zstd::xxhash::xxh64;

/// Shortest match the encoder looks for.
const MIN_MATCH: usize = 4;
/// Farthest match: its offset code (at most 28) must be in the predefined distribution.
const MAX_OFFSET: usize = (1 << 29) - 4;
/// Bits of the hash table of 4-byte prefixes.
const HASH_BITS: u32 = 16;

/// A sequence before coding: literals before it, match length and offset.
#[derive(Clone, Copy, Debug)]
struct Seq {
    ll: u32,
    ml: u32,
    offset: u32,
}

/// The code of `value` in a table of baselines and its extra bits.
fn code(value: u32, base: &[u32]) -> (u8, u32) {
    let c = base.partition_point(|&b| b <= value) - 1;
    (c as u8, value - base[c])
}

fn hash(data: &[u8], i: usize) -> usize {
    let v = u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
    (v.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
}

/// Compresses `data` into one frame.
pub fn compress(data: &[u8]) -> Vec<u8> {
    let mut out = MAGIC.to_le_bytes().to_vec();
    let n = data.len() as u64;
    let (flag, size): (u8, Vec<u8>) = match n {
        0..=255 => (0, vec![n as u8]),
        256..=65_791 => (1, ((n - 256) as u16).to_le_bytes().to_vec()),
        65_792..=0xffff_ffff => (2, (n as u32).to_le_bytes().to_vec()),
        _ => (3, n.to_le_bytes().to_vec()),
    };
    out.push(flag << 6 | SINGLE_SEGMENT | CHECKSUM);
    out.extend_from_slice(&size);
    let ll = Table::spread(&LL_DEFAULT, LL_DEFAULT_LOG);
    let of = Table::spread(&OF_DEFAULT, OF_DEFAULT_LOG);
    let ml = Table::spread(&ML_DEFAULT, ML_DEFAULT_LOG);
    let enc = (Encoder::new(&ll), Encoder::new(&of), Encoder::new(&ml));
    let mut table = vec![0u32; 1 << HASH_BITS];
    let mut start = 0;
    loop {
        let end = (start + BLOCK_MAX).min(data.len());
        let last = end == data.len();
        let seqs = find(data, (start, end), &mut table, MAX_OFFSET);
        let body = block(&data[start..end], &seqs, &enc);
        let (kind, content) = match body {
            Some(b) if b.len() < end - start => (BLOCK_COMPRESSED, b),
            _ => (BLOCK_RAW, data[start..end].to_vec()),
        };
        let header = u32::from(last) | kind << 1 | (content.len() as u32) << 3;
        out.extend_from_slice(&header.to_le_bytes()[..3]);
        out.extend_from_slice(&content);
        if last {
            break;
        }
        start = end;
    }
    out.extend_from_slice(&(xxh64(data) as u32).to_le_bytes());
    out
}

/// Greedy matches of `data[start..end]` against everything before them in the frame, at most
/// `max_offset` back.
fn find(
    data: &[u8],
    (start, end): (usize, usize),
    table: &mut [u32],
    max_offset: usize,
) -> Vec<Seq> {
    let mut seqs = Vec::new();
    let mut lit_start = start;
    let mut i = start;
    while i + MIN_MATCH <= end {
        let h = hash(data, i);
        let candidate = table[h] as usize;
        table[h] = (i + 1) as u32;
        let ok = candidate > 0
            && i - (candidate - 1) <= max_offset
            && data[candidate - 1..candidate - 1 + MIN_MATCH] == data[i..i + MIN_MATCH];
        if !ok {
            i += 1;
            continue;
        }
        let c = candidate - 1;
        let mut len = MIN_MATCH;
        while i + len < end && data[c + len] == data[i + len] {
            len += 1;
        }
        seqs.push(Seq { ll: (i - lit_start) as u32, ml: len as u32, offset: (i - c) as u32 });
        for k in i + 1..(i + len).min(end.saturating_sub(MIN_MATCH - 1)) {
            table[hash(data, k)] = (k + 1) as u32;
        }
        i += len;
        lit_start = i;
    }
    seqs
}

/// A compressed block: raw literals and the sequences; `None` if it would not fit a block.
fn block(
    raw: &[u8],
    seqs: &[Seq],
    enc: &(Encoder<'_>, Encoder<'_>, Encoder<'_>),
) -> Option<Vec<u8>> {
    let mut lits = Vec::with_capacity(raw.len());
    let mut pos = 0usize;
    for s in seqs {
        lits.extend_from_slice(&raw[pos..pos + s.ll as usize]);
        pos += (s.ll + s.ml) as usize;
    }
    lits.extend_from_slice(&raw[pos..]);
    let mut out = Vec::new();
    let n = lits.len();
    match n {
        0..=31 => out.push((n as u8) << 3),
        32..=4095 => out.extend_from_slice(&[(n as u8 & 0xf) << 4 | 0b0100, (n >> 4) as u8]),
        _ => {
            out.extend_from_slice(&[(n as u8 & 0xf) << 4 | 0b1100, (n >> 4) as u8, (n >> 12) as u8])
        }
    }
    out.extend_from_slice(&lits);
    let count = seqs.len();
    match count {
        0 => {
            out.push(0);
            return (out.len() <= BLOCK_MAX).then_some(out);
        }
        1..=127 => out.push(count as u8),
        128..=0x7eff => out.extend_from_slice(&[(count >> 8) as u8 + 128, count as u8]),
        _ => out.extend_from_slice(&[255, (count - 0x7f00) as u8, ((count - 0x7f00) >> 8) as u8]),
    }
    out.push(MODE_PREDEFINED << 6 | MODE_PREDEFINED << 4 | MODE_PREDEFINED << 2);
    out.extend_from_slice(&stream(seqs, enc));
    (out.len() <= BLOCK_MAX).then_some(out)
}

/// The backward stream of `seqs`: written from the last sequence to the first, in the reverse
/// order of the decoder's reads.
fn stream(seqs: &[Seq], (ll, of, ml): &(Encoder<'_>, Encoder<'_>, Encoder<'_>)) -> Vec<u8> {
    let mut w = BitWriter::default();
    let mut states = (0usize, 0usize, 0usize);
    for (i, s) in seqs.iter().enumerate().rev() {
        let (llc, llx) = code(s.ll, &LL_BASE);
        let (mlc, mlx) = code(s.ml, &ML_BASE);
        let ov = s.offset + 3;
        let ofc = ov.ilog2() as u8;
        if i + 1 == seqs.len() {
            states = (ll.start(llc), of.start(ofc), ml.start(mlc));
        } else {
            let (so, bo, no) = of.step(ofc, states.1);
            let (sm, bm, nm) = ml.step(mlc, states.2);
            let (sl, bl, nl) = ll.step(llc, states.0);
            w.write(bo, no);
            w.write(bm, nm);
            w.write(bl, nl);
            states = (sl, so, sm);
        }
        w.write(u64::from(llx), u32::from(LL_BITS[usize::from(llc)]));
        w.write(u64::from(mlx), u32::from(ML_BITS[usize::from(mlc)]));
        w.write(u64::from(ov - (1 << ofc)), u32::from(ofc));
    }
    w.write(states.2 as u64, ML_DEFAULT_LOG);
    w.write(states.1 as u64, OF_DEFAULT_LOG);
    w.write(states.0 as u64, LL_DEFAULT_LOG);
    w.finish()
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn matches_stop_at_the_farthest_offset() {
        let data = b"abcdefgh--------abcdefgh".to_vec();
        let mut table = vec![0u32; 1 << HASH_BITS];
        let near = find(&data, (0, data.len()), &mut table, 16);
        assert!(near.iter().any(|s| (s.offset, s.ml) == (16, 8)), "{near:?}");
        let mut table = vec![0u32; 1 << HASH_BITS];
        let far: Vec<Seq> = find(&data, (0, data.len()), &mut table, 15)
            .into_iter()
            .filter(|s| s.offset > 1)
            .collect();
        assert!(far.is_empty());
    }
}
