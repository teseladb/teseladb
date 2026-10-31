//! Fixed-width bit packing of small integers, low bits first.

/// Bits needed for the largest of `values` (0 when every value is 0).
pub(crate) fn width(values: &[u32]) -> u8 {
    let max = values.iter().copied().max().unwrap_or(0);
    (32 - max.leading_zeros()) as u8
}

/// Appends the width and `values` packed at that width.
pub(crate) fn pack(out: &mut Vec<u8>, values: &[u32]) {
    let w = width(values);
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

/// Reads `n` values packed by [`pack`] at `*pos` and moves past them.
pub(crate) fn unpack(bytes: &[u8], pos: &mut usize, n: usize) -> Option<Vec<u32>> {
    let w = u32::from(*bytes.get(*pos)?);
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
    fn rejects_short_or_bad_input() {
        let mut out = Vec::new();
        pack(&mut out, &[5, 6, 7]);
        assert_eq!(unpack(&out[..out.len() - 1], &mut 0, 3), None);
        assert_eq!(unpack(&[33, 0, 0], &mut 0, 1), None);
        assert_eq!(unpack(&[], &mut 0, 1), None);
        assert_eq!(width(&[]), 0);
    }
}
