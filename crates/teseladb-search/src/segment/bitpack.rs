//! Fixed-width bit packing of small integers, low bits first. A full block of 128 values goes in
//! the four-lane layout of [`super::lanes`] (FR-027), marked by [`LANES_FLAG`] in its width byte;
//! the other blocks, and every block written before, are one stream of bits, which still reads.

use super::lanes;

/// The bit of the width byte that marks a block in the four-lane layout.
const LANES_FLAG: u8 = 0x80;

/// Bits needed for the largest of `values` (0 when every value is 0).
pub(crate) fn width(values: &[u32]) -> u8 {
    let max = values.iter().copied().max().unwrap_or(0);
    (32 - max.leading_zeros()) as u8
}

/// Appends the width and `values` packed at that width: in four lanes when they are a full block.
pub(crate) fn pack(out: &mut Vec<u8>, values: &[u32]) {
    let w = width(values);
    if let Ok(block) = <&[u32; lanes::VALUES]>::try_from(values) {
        out.push(LANES_FLAG | w);
        for word in lanes::pack(block, u32::from(w)) {
            out.extend_from_slice(&word.to_le_bytes());
        }
        return;
    }
    pack_stream(out, values, w);
}

/// Appends width `w` and `values` as one stream of bits, the layout of every block written
/// before the four lanes and of the blocks that are not full.
fn pack_stream(out: &mut Vec<u8>, values: &[u32], w: u8) {
    out.push(w);
    let mut acc = 0_u64;
    let mut bits = 0;
    for &v in values {
        acc |= u64::from(v) << bits;
        bits += u32::from(w);
        while bits >= 8 {
            out.push(acc as u8);
            acc >>= 8;
            bits -= 8;
        }
    }
    if bits > 0 {
        out.push(acc as u8);
    }
}

/// Reads `n` values packed by [`pack`] at `*pos`, in either layout, and moves past them.
pub(crate) fn unpack(bytes: &[u8], pos: &mut usize, n: usize) -> Option<Vec<u32>> {
    let head = *bytes.get(*pos)?;
    if head & LANES_FLAG != 0 {
        return unpack_lanes(bytes, pos, n, u32::from(head & !LANES_FLAG));
    }
    let w = u32::from(head);
    if w > 32 {
        return None;
    }
    let len = (n * w as usize).div_ceil(8);
    let data = bytes.get(*pos + 1..*pos + 1 + len)?;
    *pos += 1 + len;
    let mask = if w == 32 { u64::from(u32::MAX) } else { (1_u64 << w) - 1 };
    let mut out = Vec::with_capacity(n);
    let mut acc = 0_u64;
    let mut bits = 0;
    let mut next = data.iter();
    for _ in 0..n {
        while bits < w {
            acc |= u64::from(*next.next().expect("length checked")) << bits;
            bits += 8;
        }
        out.push((acc & mask) as u32);
        acc >>= w;
        bits -= w;
    }
    Some(out)
}

/// A full block in four lanes at width `w`, its width byte at `*pos`.
fn unpack_lanes(bytes: &[u8], pos: &mut usize, n: usize, w: u32) -> Option<Vec<u32>> {
    if n != lanes::VALUES || w > 32 {
        return None;
    }
    let words = lanes::words_of(w);
    let data = bytes.get(*pos + 1..*pos + 1 + 4 * words)?;
    *pos += 1 + 4 * words;
    let words: Vec<u32> = data.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect();
    let mut out = [0_u32; lanes::VALUES];
    lanes::unpack(lanes::kernel(), &words, w, &mut out);
    Some(out.to_vec())
}

/// The documents of a block from its gaps after document `base`, wrapping on overflow (the
/// reader refuses a block whose documents do not grow): with vectors for a full block.
pub(crate) fn documents(base: u32, mut gaps: Vec<u32>) -> Vec<u32> {
    match <&mut [u32; lanes::VALUES]>::try_from(gaps.as_mut_slice()) {
        Ok(block) => lanes::prefix(lanes::kernel(), base, block),
        Err(_) => {
            let mut acc = base;
            for g in &mut gaps {
                acc = acc.wrapping_add(*g);
                *g = acc;
            }
        }
    }
    gaps
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_width() {
        for w in 0..=32_u32 {
            let max = if w == 32 { u32::MAX } else { (1_u32 << w).wrapping_sub(1) };
            let values: Vec<u32> = (0..131_u32)
                .map(|i| max.wrapping_mul(i % 3) / 2 + i % 2 * (max / 2))
                .map(|v| v.min(max))
                .collect();
            let mut out = Vec::new();
            pack(&mut out, &values);
            assert!(out[0] <= w as u8);
            let mut pos = 0;
            assert_eq!(unpack(&out, &mut pos, values.len()), Some(values));
            assert_eq!(pos, out.len());
        }
    }

    #[test]
    fn a_full_block_goes_in_lanes_and_a_stream_block_still_reads() {
        for w in [0_u32, 1, 5, 17, 32] {
            let max = if w == 32 { u32::MAX } else { (1_u32 << w).wrapping_sub(1) };
            let values: Vec<u32> = (0..128_u32).map(|i| max.wrapping_sub(i * 7) & max).collect();
            let mut lanes = Vec::new();
            pack(&mut lanes, &values);
            assert_eq!(lanes[0] & LANES_FLAG, LANES_FLAG, "width {w}");
            assert_eq!(lanes.len(), 1 + 16 * w as usize);
            let mut pos = 0;
            assert_eq!(unpack(&lanes, &mut pos, 128), Some(values.clone()));
            assert_eq!(pos, lanes.len());
            // The layout of the files written before the lanes.
            let mut stream = Vec::new();
            pack_stream(&mut stream, &values, width(&values));
            let mut pos = 0;
            assert_eq!(unpack(&stream, &mut pos, 128), Some(values));
            assert_eq!(pos, stream.len());
        }
        let mut lanes = Vec::new();
        pack(&mut lanes, &[3; 128]);
        assert_eq!(unpack(&lanes, &mut 0, 127), None, "a block in lanes holds 128 values");
        assert_eq!(unpack(&lanes[..lanes.len() - 1], &mut 0, 128), None);
        assert_eq!(unpack(&[LANES_FLAG | 33], &mut 0, 128), None);
    }

    #[test]
    fn documents_are_the_running_sums_of_their_gaps() {
        let gaps: Vec<u32> = (0..128).map(|i| i % 5 + 1).collect();
        let mut want = Vec::new();
        let mut acc = 40_u32;
        for g in &gaps {
            acc += g;
            want.push(acc);
        }
        assert_eq!(documents(40, gaps.clone()), want);
        assert_eq!(documents(40, gaps[..9].to_vec()), want[..9].to_vec());
    }

    #[test]
    fn rejects_short_or_bad_input() {
        let mut out = Vec::new();
        pack(&mut out, &[5, 6, 7]);
        assert_eq!(unpack(&out[..out.len() - 1], &mut 0, 3), None);
        assert_eq!(unpack(&[33, 0, 0], &mut 0, 1), None);
        assert_eq!(unpack(&[], &mut 0, 1), None);
        assert_eq!(width(&[]), 0);
    }
}
