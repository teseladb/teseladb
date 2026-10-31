//! Field lengths as norms: exact, and as the one-byte length that BM25 scores with (D-035,
//! research R2).
//!
//! The rule comes from what `refsearch` reports as `dl` in `scoreDetails` for 434 field lengths from
//! 1 to 1,000,000 (`tests/fixtures/refsearch/norms.txt`): a length up to [`EXACT`] is kept as it is;
//! above it, the length minus [`OFFSET`] keeps its four leading binary digits and the rest become
//! zeros, so the length used for scoring never exceeds the real one (41 scores as 40, 1,000,000
//! as 983,064).
//!
//! The byte numbers the lengths that can be represented in increasing order: 0 to 39 for the
//! exact ones, then eight per binary order of magnitude of the length minus [`OFFSET`] (its three
//! digits after the leading one). Lengths above the last one (`24 + 15 · 2²⁷`, far beyond what a
//! 16 MiB document holds) take the last byte.

/// Longest length kept exact.
pub const EXACT: u32 = 39;
/// What is taken away from a length before keeping its leading digits.
pub const OFFSET: u32 = 24;
/// Leading binary digits kept.
const DIGITS: u32 = 4;
/// First byte of the rounded lengths.
const ROUNDED: u32 = EXACT + 1;
/// Bytes per binary order of magnitude: the values of the three digits after the leading one.
const PER_ORDER: u32 = 1 << (DIGITS - 1);

/// The norm byte of a field of `len` tokens.
pub fn encode(len: u32) -> u8 {
    if len <= EXACT {
        return len as u8;
    }
    let x = len - OFFSET;
    let order = 32 - x.leading_zeros() - DIGITS;
    let digits = (x >> order) - PER_ORDER;
    let code = ROUNDED + (order - 1) * PER_ORDER + digits;
    u8::try_from(code).unwrap_or(u8::MAX)
}

/// The length a norm byte stands for.
pub fn decode(norm: u8) -> u32 {
    let code = u32::from(norm);
    if code <= EXACT {
        return code;
    }
    let order = (code - ROUNDED) / PER_ORDER + 1;
    let digits = (code - ROUNDED) % PER_ORDER + PER_ORDER;
    OFFSET + (digits << order)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        for len in 0..=EXACT {
            assert_eq!(decode(encode(len)), len, "{len}");
        }
        assert_eq!(decode(encode(41)), 40);
        assert_eq!(decode(encode(1_000_000)), 983_064);
        assert_eq!(encode(40), 40);
        assert_eq!(decode(255), OFFSET + (15 << 27));
        assert_eq!(encode(OFFSET + (15 << 27)), 255);
        assert_eq!(encode(u32::MAX), 255);
    }

    #[test]
    fn encoding_is_monotonic_and_decoding_never_overshoots() {
        let mut prev = 0;
        let mut len = 0_u32;
        while len < u32::MAX {
            let b = encode(len);
            assert!(b >= prev);
            assert!(decode(b) <= len);
            assert_eq!(encode(decode(b)), b);
            prev = b;
            len = len.saturating_add(1 + len / 64);
        }
        for b in 0..=255_u8 {
            assert_eq!(encode(decode(b)), b);
            if b > 0 {
                assert!(decode(b) > decode(b - 1));
            }
        }
    }
}
