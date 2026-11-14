//! The four-lane layout of a full block of 128 postings (FR-027): value `i` goes to lane `i % 4`,
//! each lane packs its 32 values low bits first into 32-bit words, and lane word `k` of lane `l`
//! is word `4 k + l`, so one vector load holds word `k` of every lane and one shift and mask give
//! four consecutive values. A block at width `w` is `4 w` words, little endian. The prefix sum of
//! the gaps that turns them into documents runs on vectors too.
//!
//! Each kernel runs with NEON on AArch64 and on x86_64 with AVX2 when the CPU has it (detected
//! once per process), SSE2 otherwise; the scalar kernels are the reference every vector one is
//! tested against, and they run where there is neither.
//!
//! `unsafe` here (D-151 of the architecture decisions): the vector loads and stores go through
//! pointers into slices whose lengths are checked first, and the AVX2 kernel is called after
//! checking that the CPU has AVX2. Every block says why it holds; the tests run every kernel
//! against the scalar one, also under Miri.
#![allow(unsafe_code)]

/// Values of a full block.
pub(crate) const VALUES: usize = 128;
/// Lanes of the layout.
const LANES: usize = 4;
/// Values of one lane.
const PER_LANE: usize = VALUES / LANES;

/// The 32-bit words of a block packed at width `w`.
pub(crate) fn words_of(w: u32) -> usize {
    LANES * w as usize
}

/// The mask of the low `w` bits.
fn mask(w: u32) -> u32 {
    if w == 32 { u32::MAX } else { (1_u32 << w) - 1 }
}

/// Packs `values` at width `w` (every value below `2^w`) into the four-lane words.
pub(crate) fn pack(values: &[u32; VALUES], w: u32) -> Vec<u32> {
    let mut words = vec![0_u32; words_of(w)];
    for (i, &v) in values.iter().enumerate() {
        let (lane, j) = (i % LANES, i / LANES);
        let bit = j * w as usize;
        let (k, off) = (bit / 32, (bit % 32) as u32);
        if w == 0 {
            continue;
        }
        words[LANES * k + lane] |= v << off;
        if off + w > 32 {
            words[LANES * (k + 1) + lane] |= v >> (32 - off);
        }
    }
    words
}

/// The reference unpacking of [`pack`]: `words` holds [`words_of`]`(w)` words.
#[cfg(any(test, not(any(target_arch = "aarch64", target_arch = "x86_64"))))]
pub(crate) fn unpack_scalar(words: &[u32], w: u32, out: &mut [u32; VALUES]) {
    assert!(w <= 32 && words.len() >= words_of(w), "a block of {} words at width {w}", words.len());
    if w == 0 {
        out.fill(0);
        return;
    }
    let m = mask(w);
    for (i, slot) in out.iter_mut().enumerate() {
        let (lane, j) = (i % LANES, i / LANES);
        let bit = j * w as usize;
        let (k, off) = (bit / 32, (bit % 32) as u32);
        let mut v = words[LANES * k + lane] >> off;
        if off + w > 32 {
            v |= words[LANES * (k + 1) + lane] << (32 - off);
        }
        *slot = v & m;
    }
}

/// The reference prefix sum: each value becomes `base` plus the values up to it, wrapping.
#[cfg(any(test, not(any(target_arch = "aarch64", target_arch = "x86_64"))))]
pub(crate) fn prefix_scalar(base: u32, values: &mut [u32; VALUES]) {
    let mut acc = base;
    for v in values.iter_mut() {
        acc = acc.wrapping_add(*v);
        *v = acc;
    }
}

/// The kernels this CPU runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kernel {
    /// The scalar reference, on the other architectures and in the tests.
    #[cfg(any(test, not(any(target_arch = "aarch64", target_arch = "x86_64"))))]
    Scalar,
    /// NEON (AArch64).
    #[cfg(target_arch = "aarch64")]
    Neon,
    /// SSE2 (every x86_64 CPU).
    #[cfg(target_arch = "x86_64")]
    Sse2,
    /// AVX2, when the CPU has it.
    #[cfg(target_arch = "x86_64")]
    Avx2,
}

/// The kernels of this CPU, chosen once per process.
pub(crate) fn kernel() -> Kernel {
    #[cfg(target_arch = "aarch64")]
    {
        Kernel::Neon
    }
    #[cfg(target_arch = "x86_64")]
    {
        use std::sync::OnceLock;
        static CHOSEN: OnceLock<Kernel> = OnceLock::new();
        *CHOSEN.get_or_init(|| match std::arch::is_x86_feature_detected!("avx2") {
            true => Kernel::Avx2,
            false => Kernel::Sse2,
        })
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        Kernel::Scalar
    }
}

/// [`unpack_scalar`] with kernel `k`.
pub(crate) fn unpack(k: Kernel, words: &[u32], w: u32, out: &mut [u32; VALUES]) {
    assert!(w <= 32 && words.len() >= words_of(w), "a block of {} words at width {w}", words.len());
    if w == 0 {
        out.fill(0);
        return;
    }
    match k {
        #[cfg(any(test, not(any(target_arch = "aarch64", target_arch = "x86_64"))))]
        Kernel::Scalar => unpack_scalar(words, w, out),
        // SAFETY: NEON is part of every AArch64 CPU; the lengths are checked above.
        #[cfg(target_arch = "aarch64")]
        Kernel::Neon => unsafe { neon::unpack(words, w, out) },
        // SAFETY: SSE2 is part of every x86_64 CPU; the lengths are checked above.
        #[cfg(target_arch = "x86_64")]
        Kernel::Sse2 => unsafe { x86::unpack(words, w, out) },
        // SAFETY: `kernel` gives AVX2 only on a CPU that has it; the lengths are checked above.
        #[cfg(target_arch = "x86_64")]
        Kernel::Avx2 => unsafe { x86::unpack_avx2(words, w, out) },
    }
}

/// [`prefix_scalar`] with kernel `k`.
pub(crate) fn prefix(k: Kernel, base: u32, values: &mut [u32; VALUES]) {
    match k {
        #[cfg(any(test, not(any(target_arch = "aarch64", target_arch = "x86_64"))))]
        Kernel::Scalar => prefix_scalar(base, values),
        // SAFETY: NEON is part of every AArch64 CPU; the array has `VALUES` values.
        #[cfg(target_arch = "aarch64")]
        Kernel::Neon => unsafe { neon::prefix(base, values) },
        // SAFETY: SSE2 is part of every x86_64 CPU; the array has `VALUES` values.
        #[cfg(target_arch = "x86_64")]
        Kernel::Sse2 => unsafe { x86::prefix(base, values) },
        // SAFETY: `kernel` gives AVX2 only on a CPU that has it; the array has `VALUES` values.
        #[cfg(target_arch = "x86_64")]
        Kernel::Avx2 => unsafe { x86::prefix_avx2(base, values) },
    }
}

/// `$f::<N>($v)` for the shift `$n` in `1..32`, `$v` for 0: the vector shifts take their count
/// as a constant, and with the width a constant ([`by_width`]) the match folds away.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
macro_rules! by_count {
    ($f:ident, $v:expr, $n:expr) => {{
        let v = $v;
        by_count!(@arms $f, v, $n, 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25
            26 27 28 29 30 31)
    }};
    (@arms $f:ident, $v:ident, $n:expr, $($c:literal)*) => {
        match $n {
            $($c => $f::<$c>($v),)*
            _ => $v,
        }
    };
}

/// `$f::<W>($($a),*)` for width `$w` in `1..=32`.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
macro_rules! by_width {
    ($f:ident, $w:expr, $($a:expr),*) => {
        by_width!(@arms $f, $w, ($($a),*), 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23
            24 25 26 27 28 29 30 31 32)
    };
    (@arms $f:ident, $w:expr, $args:tt, $($c:literal)*) => {
        match $w {
            $($c => by_width!(@call $f, $c, $args),)*
            _ => unreachable!("a width above 32"),
        }
    };
    (@call $f:ident, $c:literal, ($($a:expr),*)) => { $f::<$c>($($a),*) };
}

#[cfg(target_arch = "aarch64")]
mod neon {
    use super::{LANES, PER_LANE, VALUES, mask};
    use std::arch::aarch64::*;

    /// # Safety
    /// NEON is available and `words` holds at least `4 w` words, `1 <= w <= 32`.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn unpack(words: &[u32], w: u32, out: &mut [u32; VALUES]) {
        // SAFETY: the caller's contract, for the width `w` names.
        unsafe { by_width!(unpack_w, w, words, out) }
    }

    /// [`unpack`] at width `W`.
    ///
    /// # Safety
    /// As [`unpack`], with `w = W`.
    #[target_feature(enable = "neon")]
    #[inline]
    unsafe fn unpack_w<const W: u32>(words: &[u32], out: &mut [u32; VALUES]) {
        let m = vdupq_n_u32(mask(W));
        let p = words.as_ptr();
        let o = out.as_mut_ptr();
        for j in 0..PER_LANE {
            let bit = j * W as usize;
            let (k, off) = (bit / 32, (bit % 32) as u32);
            // SAFETY: word `4 k + 3` is below `4 W`, since `k < W` for every `j < 32`.
            let mut v = by_count!(vshrq_n_u32, unsafe { vld1q_u32(p.add(LANES * k)) }, off);
            if off + W > 32 {
                // SAFETY: a value that crosses into word `k + 1` has that word below `W`.
                let next = unsafe { vld1q_u32(p.add(LANES * (k + 1))) };
                v = vorrq_u32(v, by_count!(vshlq_n_u32, next, 32 - off));
            }
            // SAFETY: values `4 j .. 4 j + 4` are inside the block of 128.
            unsafe { vst1q_u32(o.add(LANES * j), vandq_u32(v, m)) };
        }
    }

    /// # Safety
    /// NEON is available.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn prefix(base: u32, values: &mut [u32; VALUES]) {
        let zero = vdupq_n_u32(0);
        let mut carry = vdupq_n_u32(base);
        let p = values.as_mut_ptr();
        for i in (0..VALUES).step_by(LANES) {
            // SAFETY: values `i .. i + 4` are inside the block of 128.
            let mut v = unsafe { vld1q_u32(p.add(i)) };
            v = vaddq_u32(v, vextq_u32::<3>(zero, v));
            v = vaddq_u32(v, vextq_u32::<2>(zero, v));
            v = vaddq_u32(v, carry);
            carry = vdupq_laneq_u32::<3>(v);
            // SAFETY: as the load.
            unsafe { vst1q_u32(p.add(i), v) };
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use super::{LANES, PER_LANE, VALUES, mask};
    use std::arch::x86_64::*;

    /// Unpacks with 128-bit vectors at width `W`; [`unpack`] and [`unpack_avx2`] compile it for
    /// their instruction sets.
    ///
    /// # Safety
    /// SSE2 is available and `words` holds at least `4 W` words, `1 <= W <= 32`.
    #[inline(always)]
    unsafe fn unpack_w<const W: u32>(words: &[u32], out: &mut [u32; VALUES]) {
        // SAFETY: SSE2 is available (the caller's contract) for every intrinsic below.
        unsafe {
            let m = _mm_set1_epi32(mask(W) as i32);
            let p = words.as_ptr().cast::<__m128i>();
            let o = out.as_mut_ptr().cast::<__m128i>();
            for j in 0..PER_LANE {
                let bit = j * W as usize;
                let (k, off) = (bit / 32, (bit % 32) as u32);
                // SAFETY: word `4 k + 3` is below `4 W`, since `k < W` for every `j < 32`.
                let mut v = by_count!(_mm_srli_epi32, _mm_loadu_si128(p.add(k)), off);
                if off + W > 32 {
                    // SAFETY: a value that crosses into word `k + 1` has that word below `W`.
                    let next = _mm_loadu_si128(p.add(k + 1));
                    v = _mm_or_si128(v, by_count!(_mm_slli_epi32, next, 32 - off));
                }
                // SAFETY: values `4 j .. 4 j + 4` are inside the block of 128.
                _mm_storeu_si128(o.add(j), _mm_and_si128(v, m));
            }
        }
        debug_assert_eq!(PER_LANE * LANES, VALUES);
    }

    /// [`unpack_w`] for width `w`.
    ///
    /// # Safety
    /// As [`unpack_w`], with `W = w`.
    #[inline(always)]
    unsafe fn unpack_with(words: &[u32], w: u32, out: &mut [u32; VALUES]) {
        // SAFETY: the caller's contract, for the width `w` names.
        unsafe { by_width!(unpack_w, w, words, out) }
    }

    /// # Safety
    /// As [`unpack_with`].
    #[target_feature(enable = "sse2")]
    pub(super) unsafe fn unpack(words: &[u32], w: u32, out: &mut [u32; VALUES]) {
        // SAFETY: the caller's contract.
        unsafe { unpack_with(words, w, out) }
    }

    /// # Safety
    /// As [`unpack_with`], and AVX2 is available.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn unpack_avx2(words: &[u32], w: u32, out: &mut [u32; VALUES]) {
        // SAFETY: the caller's contract; AVX2 includes SSE2.
        unsafe { unpack_with(words, w, out) }
    }

    /// # Safety
    /// SSE2 is available.
    #[target_feature(enable = "sse2")]
    pub(super) unsafe fn prefix(base: u32, values: &mut [u32; VALUES]) {
        let mut carry = _mm_set1_epi32(base as i32);
        let p = values.as_mut_ptr().cast::<__m128i>();
        for i in 0..VALUES / LANES {
            // SAFETY: vector `i` holds values `4 i .. 4 i + 4`, inside the block of 128.
            let mut v = unsafe { _mm_loadu_si128(p.add(i)) };
            v = _mm_add_epi32(v, _mm_slli_si128::<4>(v));
            v = _mm_add_epi32(v, _mm_slli_si128::<8>(v));
            v = _mm_add_epi32(v, carry);
            carry = _mm_shuffle_epi32::<0xFF>(v);
            // SAFETY: as the load.
            unsafe { _mm_storeu_si128(p.add(i), v) };
        }
    }

    /// Eight values at a time: each half of the vector sums its four, then the upper half adds
    /// the lower half's last.
    ///
    /// # Safety
    /// AVX2 is available.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn prefix_avx2(base: u32, values: &mut [u32; VALUES]) {
        let mut carry = _mm256_set1_epi32(base as i32);
        let p = values.as_mut_ptr().cast::<__m256i>();
        for i in 0..VALUES / (2 * LANES) {
            // SAFETY: vector `i` holds values `8 i .. 8 i + 8`, inside the block of 128.
            let mut v = unsafe { _mm256_loadu_si256(p.add(i)) };
            v = _mm256_add_epi32(v, _mm256_slli_si256::<4>(v));
            v = _mm256_add_epi32(v, _mm256_slli_si256::<8>(v));
            let last = _mm256_shuffle_epi32::<0xFF>(v);
            v = _mm256_add_epi32(v, _mm256_permute2x128_si256::<0x08>(last, last));
            v = _mm256_add_epi32(v, carry);
            carry = _mm256_permute2x128_si256::<0x11>(v, v);
            carry = _mm256_shuffle_epi32::<0xFF>(carry);
            // SAFETY: as the load.
            unsafe { _mm256_storeu_si256(p.add(i), v) };
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// Every kernel this machine can run, the scalar one first.
    fn kernels() -> Vec<Kernel> {
        let mut out = vec![Kernel::Scalar];
        #[cfg(target_arch = "aarch64")]
        out.push(Kernel::Neon);
        #[cfg(target_arch = "x86_64")]
        {
            out.push(Kernel::Sse2);
            if std::arch::is_x86_feature_detected!("avx2") {
                out.push(Kernel::Avx2);
            }
        }
        out
    }

    /// 128 values below `2^w` from `seed`, with the largest one present.
    fn values(w: u32, seed: u64) -> [u32; VALUES] {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut out = [0_u32; VALUES];
        for v in out.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *v = (x as u32) & mask(w.max(1));
        }
        if w == 0 {
            out.fill(0);
        } else {
            out[(seed % VALUES as u64) as usize] = mask(w);
        }
        out
    }

    #[test]
    fn every_kernel_unpacks_every_width_as_the_scalar_one() {
        let seeds = if cfg!(miri) { 1 } else { 8 };
        for w in 0..=32 {
            for seed in 0..seeds {
                let want = values(w, seed);
                let words = pack(&want, w);
                assert_eq!(words.len(), words_of(w));
                for k in kernels() {
                    let mut got = [7_u32; VALUES];
                    unpack(k, &words, w, &mut got);
                    assert_eq!(got, want, "{k:?} at width {w}");
                }
            }
        }
    }

    #[test]
    fn every_kernel_sums_the_prefixes_as_the_scalar_one() {
        for (w, base) in [(0, 0), (1, 5), (7, 1 << 20), (16, u32::MAX - 3), (32, 9)] {
            let gaps = values(w, u64::from(w) + 3);
            let mut want = gaps;
            prefix_scalar(base, &mut want);
            for k in kernels() {
                let mut got = gaps;
                prefix(k, base, &mut got);
                assert_eq!(got, want, "{k:?} from {base} at width {w}");
            }
        }
        let mut ones = [1_u32; VALUES];
        prefix(kernel(), 10, &mut ones);
        assert_eq!(ones[0], 11);
        assert_eq!(ones[VALUES - 1], 10 + VALUES as u32);
    }

    #[test]
    #[should_panic(expected = "a block of 3 words at width 1")]
    fn a_short_block_is_refused_before_any_vector_reads_it() {
        unpack(kernel(), &[0; 3], 1, &mut [0; VALUES]);
    }
}
