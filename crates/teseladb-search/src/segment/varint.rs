//! LEB128 integers: seven bits per byte, low bits first.

/// Appends `v` to `out`.
pub(crate) fn put(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Reads an integer at `*pos` and moves past it; `None` past the end or beyond 64 bits.
pub(crate) fn get(bytes: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0_u64;
    for shift in (0..64).step_by(7) {
        let b = *bytes.get(*pos)?;
        *pos += 1;
        let low = u64::from(b & 0x7f);
        if shift == 63 && low > 1 {
            return None;
        }
        v |= low << shift;
        if b < 0x80 {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_bad_input() {
        for v in [0, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX - 1, u64::MAX] {
            let mut out = Vec::new();
            put(&mut out, v);
            let mut pos = 0;
            assert_eq!(get(&out, &mut pos), Some(v));
            assert_eq!(pos, out.len());
        }
        assert_eq!(get(&[0x80], &mut 0), None);
        assert_eq!(get(&[0xff; 9].iter().copied().chain([0x02]).collect::<Vec<_>>(), &mut 0), None);
        assert_eq!(get(&[0xff; 11], &mut 0), None);
    }
}
