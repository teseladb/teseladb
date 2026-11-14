//! Byte kernels of text analysis and of the term dictionary (011 FR-028, FR-029): the classes of
//! the bytes of a 32-byte window of text as bit masks (bit `i` for byte `i`), the ASCII lowercase
//! of a window, and the bytes of a window equal to one byte, which finds the arc of a dense node
//! of the dictionary. Each kernel runs
//! with NEON on AArch64 and with AVX2 on x86_64 when the CPU has it (detected once), SSE2
//! otherwise; the scalar kernel is the reference every vector one is tested against.
//!
//! `unsafe` here (D-151 of the architecture decisions): the vector loads and stores go through a
//! pointer into a window of exactly [`WINDOW`] bytes, and the AVX2 kernels are called after
//! checking the CPU has AVX2. Every block says why it holds; the tests run every kernel against
//! the scalar one, also under Miri.
#![allow(unsafe_code)]

/// The bytes a kernel takes.
pub const WINDOW: usize = 32;

/// A window of text.
pub type Window = [u8; WINDOW];

/// The classes of the bytes of a window, one bit per byte.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Classes {
    /// Bytes of 0x80 and above: the bytes of a character outside ASCII.
    pub high: u32,
    /// `A` to `Z`.
    pub upper: u32,
    /// `a` to `z`.
    pub lower: u32,
    /// `0` to `9`.
    pub digit: u32,
    /// The ASCII characters that can join two characters into one word in UAX #29 (MidLetter,
    /// MidNum, MidNumLet, Single_Quote, Double_Quote and ExtendNumLet): `.,:;'"_`.
    pub joiner: u32,
    /// Java's white space in ASCII: tab to carriage return, 0x1C to 0x1F and the space.
    pub space: u32,
}

impl Classes {
    /// ASCII letters.
    pub fn letter(&self) -> u32 {
        self.upper | self.lower
    }

    /// ASCII letters and digits.
    pub fn alnum(&self) -> u32 {
        self.upper | self.lower | self.digit
    }
}

/// The window of `bytes` that starts at `at`, padded with zeros, and the mask of its bytes that
/// are in `bytes`.
pub fn window(bytes: &[u8], at: usize) -> (Window, u32) {
    let mut w = [0u8; WINDOW];
    let len = bytes.len().saturating_sub(at).min(WINDOW);
    w[..len].copy_from_slice(&bytes[at..at + len]);
    let valid = if len == WINDOW { u32::MAX } else { (1u32 << len) - 1 };
    (w, valid)
}

/// The classes of the bytes of `w`.
pub fn classify(w: &Window) -> Classes {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is part of every AArch64 target.
        unsafe { neon::classify(w) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        if has_avx2() {
            // SAFETY: the CPU has AVX2, checked just above.
            unsafe { x86::classify_avx2(w) }
        } else {
            // SAFETY: SSE2 is part of every x86_64 CPU.
            unsafe { x86::classify_sse2(w) }
        }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        scalar::classify(w)
    }
}

/// `w` with `A` to `Z` turned into `a` to `z`.
pub fn lowercase(w: &Window) -> Window {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is part of every AArch64 target.
        unsafe { neon::lowercase(w) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        if has_avx2() {
            // SAFETY: the CPU has AVX2, checked just above.
            unsafe { x86::lowercase_avx2(w) }
        } else {
            // SAFETY: SSE2 is part of every x86_64 CPU.
            unsafe { x86::lowercase_sse2(w) }
        }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        scalar::lowercase(w)
    }
}

/// The bytes of `w` equal to `b`.
pub fn equal(w: &Window, b: u8) -> u32 {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is part of every AArch64 target.
        unsafe { neon::equal(w, b) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        if has_avx2() {
            // SAFETY: the CPU has AVX2, checked just above.
            unsafe { x86::equal_avx2(w, b) }
        } else {
            // SAFETY: SSE2 is part of every x86_64 CPU.
            unsafe { x86::equal_sse2(w, b) }
        }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        scalar::equal(w, b)
    }
}

/// The index of the first byte of `bytes` equal to `b`, 32 bytes at a time.
pub fn find(bytes: &[u8], b: u8) -> Option<usize> {
    let mut at = 0;
    while at < bytes.len() {
        let (w, valid) = window(bytes, at);
        let hits = equal(&w, b) & valid;
        if hits != 0 {
            return Some(at + hits.trailing_zeros() as usize);
        }
        at += WINDOW;
    }
    None
}

/// Whether the CPU has AVX2, asked once.
#[cfg(target_arch = "x86_64")]
fn has_avx2() -> bool {
    use std::sync::OnceLock;
    static AVX2: OnceLock<bool> = OnceLock::new();
    *AVX2.get_or_init(|| std::arch::is_x86_feature_detected!("avx2"))
}

/// The joiners of [`Classes::joiner`].
const JOINERS: [u8; 7] = *b".,:;'\"_";

/// The scalar kernels: the reference of the vector ones, and the kernels of other targets.
#[cfg_attr(any(target_arch = "aarch64", target_arch = "x86_64"), allow(dead_code))]
mod scalar {
    use super::{Classes, JOINERS, Window};

    pub(super) fn classify(w: &Window) -> Classes {
        let mut c = Classes::default();
        for (i, &b) in w.iter().enumerate() {
            let bit = 1u32 << i;
            let set = |mask: &mut u32, yes: bool| {
                if yes {
                    *mask |= bit;
                }
            };
            set(&mut c.high, b >= 0x80);
            set(&mut c.upper, b.is_ascii_uppercase());
            set(&mut c.lower, b.is_ascii_lowercase());
            set(&mut c.digit, b.is_ascii_digit());
            set(&mut c.joiner, JOINERS.contains(&b));
            set(&mut c.space, matches!(b, b'\t'..=b'\r' | 0x1C..=0x1F | b' '));
        }
        c
    }

    pub(super) fn lowercase(w: &Window) -> Window {
        w.map(|b| b.to_ascii_lowercase())
    }

    pub(super) fn equal(w: &Window, b: u8) -> u32 {
        w.iter().enumerate().filter(|&(_, &x)| x == b).fold(0, |m, (i, _)| m | (1 << i))
    }
}

#[cfg(target_arch = "aarch64")]
mod neon {
    use std::arch::aarch64::*;

    use super::{Classes, JOINERS, WINDOW, Window};

    /// The 16 bytes of `w` at `half` (0 or 1).
    #[inline]
    #[target_feature(enable = "neon")]
    fn load(w: &Window, half: usize) -> uint8x16_t {
        assert!(half < 2);
        // SAFETY: `half * 16 + 16 <= 32`, so the 16 bytes are inside the window, and `vld1q_u8`
        // reads without alignment.
        unsafe { vld1q_u8(w.as_ptr().add(half * 16)) }
    }

    /// One bit per lane of `v`, whose lanes are all ones or all zeros.
    #[inline]
    #[target_feature(enable = "neon")]
    fn bits(v: uint8x16_t) -> u32 {
        const WEIGHTS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
        // SAFETY: `WEIGHTS` holds the 16 bytes `vld1q_u8` reads.
        let weights = unsafe { vld1q_u8(WEIGHTS.as_ptr()) };
        let m = vandq_u8(v, weights);
        u32::from(vaddv_u8(vget_low_u8(m))) | (u32::from(vaddv_u8(vget_high_u8(m))) << 8)
    }

    /// The lanes of `v` in `lo..=hi`.
    #[inline]
    #[target_feature(enable = "neon")]
    fn within(v: uint8x16_t, lo: u8, hi: u8) -> uint8x16_t {
        vandq_u8(vcgeq_u8(v, vdupq_n_u8(lo)), vcleq_u8(v, vdupq_n_u8(hi)))
    }

    #[target_feature(enable = "neon")]
    pub(super) fn classify(w: &Window) -> Classes {
        let mut c = Classes::default();
        for half in 0..2 {
            let v = load(w, half);
            let shift = half * 16;
            let mut joiner = vdupq_n_u8(0);
            for j in JOINERS {
                joiner = vorrq_u8(joiner, vceqq_u8(v, vdupq_n_u8(j)));
            }
            let space = vorrq_u8(
                vorrq_u8(within(v, b'\t', b'\r'), within(v, 0x1C, 0x1F)),
                vceqq_u8(v, vdupq_n_u8(b' ')),
            );
            c.high |= bits(vcgeq_u8(v, vdupq_n_u8(0x80))) << shift;
            c.upper |= bits(within(v, b'A', b'Z')) << shift;
            c.lower |= bits(within(v, b'a', b'z')) << shift;
            c.digit |= bits(within(v, b'0', b'9')) << shift;
            c.joiner |= bits(joiner) << shift;
            c.space |= bits(space) << shift;
        }
        c
    }

    #[target_feature(enable = "neon")]
    pub(super) fn equal(w: &Window, b: u8) -> u32 {
        let k = vdupq_n_u8(b);
        bits(vceqq_u8(load(w, 0), k)) | (bits(vceqq_u8(load(w, 1), k)) << 16)
    }

    #[target_feature(enable = "neon")]
    pub(super) fn lowercase(w: &Window) -> Window {
        let mut out = [0u8; WINDOW];
        for half in 0..2 {
            let v = load(w, half);
            let lowered = vaddq_u8(v, vandq_u8(within(v, b'A', b'Z'), vdupq_n_u8(0x20)));
            // SAFETY: `half * 16 + 16 <= 32`, so the 16 bytes written are inside `out`, and
            // `vst1q_u8` writes without alignment.
            unsafe { vst1q_u8(out.as_mut_ptr().add(half * 16), lowered) };
        }
        out
    }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::*;

    use super::{Classes, JOINERS, WINDOW, Window};

    /// The 16 bytes of `w` at `half` (0 or 1).
    #[inline]
    #[target_feature(enable = "sse2")]
    fn load16(w: &Window, half: usize) -> __m128i {
        assert!(half < 2);
        // SAFETY: `half * 16 + 16 <= 32`, so the 16 bytes are inside the window, and
        // `_mm_loadu_si128` reads without alignment.
        unsafe { _mm_loadu_si128(w.as_ptr().add(half * 16).cast()) }
    }

    /// The lanes of `v` in `lo..=hi`: unsigned, as the greatest of each lane and `lo` and the
    /// least of each lane and `hi`.
    #[inline]
    #[target_feature(enable = "sse2")]
    fn within16(v: __m128i, lo: u8, hi: u8) -> __m128i {
        let ge = _mm_cmpeq_epi8(_mm_max_epu8(v, _mm_set1_epi8(lo as i8)), v);
        let le = _mm_cmpeq_epi8(_mm_min_epu8(v, _mm_set1_epi8(hi as i8)), v);
        _mm_and_si128(ge, le)
    }

    #[inline]
    #[target_feature(enable = "sse2")]
    fn bits16(v: __m128i) -> u32 {
        _mm_movemask_epi8(v) as u32
    }

    #[target_feature(enable = "sse2")]
    pub(super) fn classify_sse2(w: &Window) -> Classes {
        let mut c = Classes::default();
        for half in 0..2 {
            let v = load16(w, half);
            let shift = half * 16;
            let mut joiner = _mm_setzero_si128();
            for j in JOINERS {
                joiner = _mm_or_si128(joiner, _mm_cmpeq_epi8(v, _mm_set1_epi8(j as i8)));
            }
            let space = _mm_or_si128(
                _mm_or_si128(within16(v, b'\t', b'\r'), within16(v, 0x1C, 0x1F)),
                _mm_cmpeq_epi8(v, _mm_set1_epi8(b' ' as i8)),
            );
            c.high |= bits16(v) << shift;
            c.upper |= bits16(within16(v, b'A', b'Z')) << shift;
            c.lower |= bits16(within16(v, b'a', b'z')) << shift;
            c.digit |= bits16(within16(v, b'0', b'9')) << shift;
            c.joiner |= bits16(joiner) << shift;
            c.space |= bits16(space) << shift;
        }
        c
    }

    #[target_feature(enable = "sse2")]
    pub(super) fn equal_sse2(w: &Window, b: u8) -> u32 {
        let k = _mm_set1_epi8(b as i8);
        bits16(_mm_cmpeq_epi8(load16(w, 0), k)) | (bits16(_mm_cmpeq_epi8(load16(w, 1), k)) << 16)
    }

    #[target_feature(enable = "sse2")]
    pub(super) fn lowercase_sse2(w: &Window) -> Window {
        let mut out = [0u8; WINDOW];
        for half in 0..2 {
            let v = load16(w, half);
            let add = _mm_and_si128(within16(v, b'A', b'Z'), _mm_set1_epi8(0x20));
            // SAFETY: `half * 16 + 16 <= 32`, so the 16 bytes written are inside `out`, and
            // `_mm_storeu_si128` writes without alignment.
            unsafe {
                _mm_storeu_si128(out.as_mut_ptr().add(half * 16).cast(), _mm_add_epi8(v, add))
            };
        }
        out
    }

    /// The 32 bytes of `w`.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn load32(w: &Window) -> __m256i {
        // SAFETY: the window holds the 32 bytes `_mm256_loadu_si256` reads, without alignment.
        unsafe { _mm256_loadu_si256(w.as_ptr().cast()) }
    }

    /// As [`within16`], on 32 lanes.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn within32(v: __m256i, lo: u8, hi: u8) -> __m256i {
        let ge = _mm256_cmpeq_epi8(_mm256_max_epu8(v, _mm256_set1_epi8(lo as i8)), v);
        let le = _mm256_cmpeq_epi8(_mm256_min_epu8(v, _mm256_set1_epi8(hi as i8)), v);
        _mm256_and_si256(ge, le)
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    fn bits32(v: __m256i) -> u32 {
        _mm256_movemask_epi8(v) as u32
    }

    #[target_feature(enable = "avx2")]
    pub(super) fn classify_avx2(w: &Window) -> Classes {
        let v = load32(w);
        let mut joiner = _mm256_setzero_si256();
        for j in JOINERS {
            joiner = _mm256_or_si256(joiner, _mm256_cmpeq_epi8(v, _mm256_set1_epi8(j as i8)));
        }
        let space = _mm256_or_si256(
            _mm256_or_si256(within32(v, b'\t', b'\r'), within32(v, 0x1C, 0x1F)),
            _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b' ' as i8)),
        );
        Classes {
            high: bits32(v),
            upper: bits32(within32(v, b'A', b'Z')),
            lower: bits32(within32(v, b'a', b'z')),
            digit: bits32(within32(v, b'0', b'9')),
            joiner: bits32(joiner),
            space: bits32(space),
        }
    }

    #[target_feature(enable = "avx2")]
    pub(super) fn equal_avx2(w: &Window, b: u8) -> u32 {
        bits32(_mm256_cmpeq_epi8(load32(w), _mm256_set1_epi8(b as i8)))
    }

    #[target_feature(enable = "avx2")]
    pub(super) fn lowercase_avx2(w: &Window) -> Window {
        let v = load32(w);
        let add = _mm256_and_si256(within32(v, b'A', b'Z'), _mm256_set1_epi8(0x20));
        let mut out = [0u8; WINDOW];
        // SAFETY: `out` holds the 32 bytes `_mm256_storeu_si256` writes, without alignment.
        unsafe { _mm256_storeu_si256(out.as_mut_ptr().cast(), _mm256_add_epi8(v, add)) };
        out
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// Windows that hold every byte value, at every lane, and some pseudo-random ones.
    fn windows() -> Vec<Window> {
        let mut out = Vec::new();
        for start in 0..=255u8 {
            let mut w = [0u8; WINDOW];
            for (i, b) in w.iter_mut().enumerate() {
                *b = start.wrapping_add((i as u8).wrapping_mul(37));
            }
            out.push(w);
        }
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..64 {
            let mut w = [0u8; WINDOW];
            for b in &mut w {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *b = state as u8;
            }
            out.push(w);
        }
        out
    }

    #[test]
    fn classes_match_the_scalar_kernel() {
        for w in windows() {
            assert_eq!(classify(&w), scalar::classify(&w), "{w:?}");
        }
    }

    #[test]
    fn lowercase_matches_the_scalar_kernel() {
        for w in windows() {
            assert_eq!(lowercase(&w), scalar::lowercase(&w), "{w:?}");
        }
    }

    #[test]
    fn equal_matches_the_scalar_kernel() {
        for w in windows() {
            for b in [0, 1, b'a', 0x7F, 0x80, 0xFF, w[0], w[31]] {
                assert_eq!(equal(&w, b), scalar::equal(&w, b), "{w:?} {b}");
            }
        }
        let bytes: Vec<u8> = (0..100).collect();
        assert_eq!(find(&bytes, 0), Some(0));
        assert_eq!(find(&bytes, 31), Some(31));
        assert_eq!(find(&bytes, 99), Some(99));
        assert_eq!(find(&bytes, 100), None);
        assert_eq!(find(&bytes[..3], 0), Some(0));
        assert_eq!(find(&[1, 2], 0), None);
        assert_eq!(find(&[], 0), None);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn sse2_kernels_match_the_scalar_kernel() {
        for w in windows() {
            // SAFETY: SSE2 is part of every x86_64 CPU.
            let (c, l) = unsafe { (x86::classify_sse2(&w), x86::lowercase_sse2(&w)) };
            assert_eq!(c, scalar::classify(&w));
            assert_eq!(l, scalar::lowercase(&w));
            // SAFETY: as above.
            assert_eq!(unsafe { x86::equal_sse2(&w, w[5]) }, scalar::equal(&w, w[5]));
        }
    }

    #[test]
    fn windows_are_padded_and_masked() {
        let text = b"0123456789";
        let (w, valid) = window(text, 4);
        assert_eq!(&w[..6], b"456789");
        assert!(w[6..].iter().all(|&b| b == 0));
        assert_eq!(valid, 0b11_1111);
        let long = [b'a'; 40];
        assert_eq!(window(&long, 0).1, u32::MAX);
        assert_eq!(window(&long, 40).1, 0);
        let c = classify(&window(b"Ab1._ \t\xC3\xA9z", 0).0);
        assert_eq!(c.upper, 0b1);
        assert_eq!(c.lower, 0b10_0000_0010);
        assert_eq!(c.letter(), 0b10_0000_0011);
        assert_eq!(c.digit, 0b100);
        assert_eq!(c.alnum(), 0b10_0000_0111);
        assert_eq!(c.joiner, 0b1_1000);
        assert_eq!(c.space, 0b110_0000);
        assert_eq!(c.high, 0b1_1000_0000);
    }
}
