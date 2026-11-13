//! The vector kernels of the batch machine and of `$group` on columns (010 FR-030): a column of
//! up to 64 `i64` or `f64` values, gathered from the registers of a batch, compared with a
//! literal into a `u64` mask (bit `i` for value `i`), and its sum, least and greatest values.
//! Each kernel runs with NEON on AArch64 and with AVX2 on x86_64 when the CPU has it (detected
//! once), SSE2 otherwise; the scalar kernel is the reference every vector one is tested against.
//!
//! The comparisons of doubles are those of IEEE 754, which are RefDb's on numbers that are not
//! NaN: `-0.0` equals `0.0`. The callers keep NaN out of the columns they compare. The least and
//! greatest doubles skip NaN, which the caller orders on its own (it is below every number).
//!
//! `unsafe` here (D-151 of the architecture decisions): the vector loads read the column through
//! a pointer, and the AVX2 kernels are called after checking the CPU has AVX2. Every block says
//! why it holds; the tests run every kernel against the scalar one, also under Miri.
#![allow(unsafe_code)]

/// The values a kernel takes: a batch of the machine.
pub const LANES: usize = 64;

/// A column of a batch: the value of each lane; the lanes a mask leaves out hold anything.
pub type Column<T> = [T; LANES];

/// The comparison of a kernel: the column's value on the left, the literal on the right.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    /// Equal.
    Eq,
    /// Greater.
    Gt,
    /// Greater or equal.
    Gte,
    /// Less.
    Lt,
    /// Less or equal.
    Lte,
}

/// The lanes of `col` whose value stands in `cmp` to `k`.
pub fn cmp_i64(cmp: Cmp, col: &Column<i64>, k: i64) -> u64 {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is part of every AArch64 target.
        unsafe { neon::cmp_i64(cmp, col, k) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        if has_avx2() {
            // SAFETY: the CPU has AVX2, checked just above.
            unsafe { x86::cmp_i64_avx2(cmp, col, k) }
        } else {
            // SAFETY: SSE2 is part of every x86_64 CPU.
            unsafe { x86::cmp_i64_sse2(cmp, col, k) }
        }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        scalar::cmp_i64(cmp, col, k)
    }
}

/// The lanes of `col` whose value stands in `cmp` to `k` (IEEE comparison).
pub fn cmp_f64(cmp: Cmp, col: &Column<f64>, k: f64) -> u64 {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is part of every AArch64 target.
        unsafe { neon::cmp_f64(cmp, col, k) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        if has_avx2() {
            // SAFETY: the CPU has AVX2, checked just above.
            unsafe { x86::cmp_f64_avx2(cmp, col, k) }
        } else {
            // SAFETY: SSE2 is part of every x86_64 CPU.
            unsafe { x86::cmp_f64_sse2(cmp, col, k) }
        }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        scalar::cmp_f64(cmp, col, k)
    }
}

/// The exact sum of the lanes of `mask` in `col`.
pub fn sum_i64(col: &Column<i64>, mask: u64) -> i128 {
    let zeroed = masked(col, mask, 0);
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is part of every AArch64 target.
        unsafe { neon::sum_i64(&zeroed) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        if has_avx2() {
            // SAFETY: the CPU has AVX2, checked just above.
            unsafe { x86::sum_i64_avx2(&zeroed) }
        } else {
            // SAFETY: SSE2 is part of every x86_64 CPU.
            unsafe { x86::sum_i64_sse2(&zeroed) }
        }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        scalar::sum_i64(&zeroed)
    }
}

/// The greatest (`max`) or least value among the lanes of `mask` in `col`; `None` for no lane.
pub fn extreme_i64(col: &Column<i64>, mask: u64, max: bool) -> Option<i64> {
    let first = col[first_lane(mask)?];
    let filled = masked(col, mask, first);
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is part of every AArch64 target.
        Some(unsafe { neon::extreme_i64(&filled, max) })
    }
    #[cfg(target_arch = "x86_64")]
    {
        if has_avx2() {
            // SAFETY: the CPU has AVX2, checked just above.
            Some(unsafe { x86::extreme_i64_avx2(&filled, max) })
        } else {
            // SAFETY: SSE2 is part of every x86_64 CPU.
            Some(unsafe { x86::extreme_i64_sse2(&filled, max) })
        }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        Some(scalar::extreme_i64(&filled, max))
    }
}

/// The greatest (`max`) or least value among the lanes of `mask` in `col` that are not NaN, and
/// the lanes of `mask` that are NaN.
pub fn extreme_f64(col: &Column<f64>, mask: u64, max: bool) -> (Option<f64>, u64) {
    // NaN does not equal itself.
    let nan = mask & !not_nan(col);
    let numbers = mask & !nan;
    let Some(first) = first_lane(numbers).map(|l| col[l]) else { return (None, nan) };
    let filled = masked(col, numbers, first);
    // SAFETY: NEON is part of every AArch64 target.
    #[cfg(target_arch = "aarch64")]
    let best = unsafe { neon::extreme_f64(&filled, max) };
    #[cfg(target_arch = "x86_64")]
    let best = if has_avx2() {
        // SAFETY: the CPU has AVX2, checked just above.
        unsafe { x86::extreme_f64_avx2(&filled, max) }
    } else {
        // SAFETY: SSE2 is part of every x86_64 CPU.
        unsafe { x86::extreme_f64_sse2(&filled, max) }
    };
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    let best = scalar::extreme_f64(&filled, max);
    (Some(best), nan)
}

/// The lanes of `col` that are not NaN.
fn not_nan(col: &Column<f64>) -> u64 {
    col.iter().enumerate().fold(0, |m, (l, x)| m | (u64::from(!x.is_nan()) << l))
}

/// The first lane of `mask`.
fn first_lane(mask: u64) -> Option<usize> {
    (mask != 0).then(|| mask.trailing_zeros() as usize)
}

/// `col` with the lanes outside `mask` set to `fill`.
fn masked<T: Copy>(col: &Column<T>, mask: u64, fill: T) -> Column<T> {
    std::array::from_fn(|l| if (mask >> l) & 1 == 1 { col[l] } else { fill })
}

/// Whether the CPU has AVX2, detected on the first call.
#[cfg(target_arch = "x86_64")]
fn has_avx2() -> bool {
    use std::sync::OnceLock;
    static AVX2: OnceLock<bool> = OnceLock::new();
    *AVX2.get_or_init(|| std::arch::is_x86_feature_detected!("avx2"))
}

/// Whether `o` (the column's value against the literal) satisfies `cmp`.
fn holds(cmp: Cmp, o: std::cmp::Ordering) -> bool {
    use std::cmp::Ordering::{Equal, Greater, Less};
    match cmp {
        Cmp::Eq => o == Equal,
        Cmp::Gt => o == Greater,
        Cmp::Gte => o != Less,
        Cmp::Lt => o == Less,
        Cmp::Lte => o != Greater,
    }
}

/// The reference kernels, one lane at a time.
pub mod scalar {
    use super::{Cmp, Column, holds};

    /// The lanes of `col` whose value stands in `cmp` to `k`.
    pub fn cmp_i64(cmp: Cmp, col: &Column<i64>, k: i64) -> u64 {
        col.iter().enumerate().fold(0, |m, (l, x)| m | (u64::from(holds(cmp, x.cmp(&k))) << l))
    }

    /// The lanes of `col` whose value stands in `cmp` to `k` (false where either is NaN).
    pub fn cmp_f64(cmp: Cmp, col: &Column<f64>, k: f64) -> u64 {
        col.iter().enumerate().fold(0, |m, (l, x)| {
            m | (u64::from(x.partial_cmp(&k).is_some_and(|o| holds(cmp, o))) << l)
        })
    }

    /// The exact sum of every lane.
    pub fn sum_i64(col: &Column<i64>) -> i128 {
        col.iter().map(|&x| i128::from(x)).sum()
    }

    /// The greatest (`max`) or least lane.
    pub fn extreme_i64(col: &Column<i64>, max: bool) -> i64 {
        let it = col.iter().copied();
        if max { it.max() } else { it.min() }.unwrap_or_default()
    }

    /// `col` holds no NaN; of `-0.0` and `0.0` either may come out, as they are equal.
    pub fn extreme_f64(col: &Column<f64>, max: bool) -> f64 {
        col.iter().copied().fold(col[0], |b, x| if (x > b) == max && x != b { x } else { b })
    }
}

#[cfg(target_arch = "aarch64")]
mod neon {
    use std::arch::aarch64::*;

    use super::{Cmp, Column, LANES};

    /// The two lanes at `i` of `col`.
    #[inline]
    #[target_feature(enable = "neon")]
    fn load_i64(col: &Column<i64>, i: usize) -> int64x2_t {
        assert!(i + 2 <= LANES);
        // SAFETY: `i + 2 <= 64`, so both values are inside the column; NEON is part of the
        // AArch64 targets this builds for, and `vld1q_s64` reads without alignment.
        unsafe { vld1q_s64(col.as_ptr().add(i)) }
    }

    /// The two lanes at `i` of `col`.
    #[inline]
    #[target_feature(enable = "neon")]
    fn load_f64(col: &Column<f64>, i: usize) -> float64x2_t {
        assert!(i + 2 <= LANES);
        // SAFETY: as in `load_i64`.
        unsafe { vld1q_f64(col.as_ptr().add(i)) }
    }

    /// The two bits of a comparison's result (all ones or all zeros per lane).
    #[inline]
    #[target_feature(enable = "neon")]
    fn bits(r: uint64x2_t) -> u64 {
        (vgetq_lane_u64::<0>(r) & 1) | ((vgetq_lane_u64::<1>(r) & 1) << 1)
    }

    #[target_feature(enable = "neon")]
    pub(super) fn cmp_i64(cmp: Cmp, col: &Column<i64>, k: i64) -> u64 {
        let kk = vdupq_n_s64(k);
        let mut out = 0u64;
        for i in (0..LANES).step_by(2) {
            let v = load_i64(col, i);
            let r = match cmp {
                Cmp::Eq => vceqq_s64(v, kk),
                Cmp::Gt => vcgtq_s64(v, kk),
                Cmp::Gte => vcgeq_s64(v, kk),
                Cmp::Lt => vcltq_s64(v, kk),
                Cmp::Lte => vcleq_s64(v, kk),
            };
            out |= bits(r) << i;
        }
        out
    }

    #[target_feature(enable = "neon")]
    pub(super) fn cmp_f64(cmp: Cmp, col: &Column<f64>, k: f64) -> u64 {
        let kk = vdupq_n_f64(k);
        let mut out = 0u64;
        for i in (0..LANES).step_by(2) {
            let v = load_f64(col, i);
            let r = match cmp {
                Cmp::Eq => vceqq_f64(v, kk),
                Cmp::Gt => vcgtq_f64(v, kk),
                Cmp::Gte => vcgeq_f64(v, kk),
                Cmp::Lt => vcltq_f64(v, kk),
                Cmp::Lte => vcleq_f64(v, kk),
            };
            out |= bits(r) << i;
        }
        out
    }

    /// The sum as the sums of the high halves (signed) and of the low halves (unsigned) of each
    /// value, which 64 lanes cannot overflow.
    #[target_feature(enable = "neon")]
    pub(super) fn sum_i64(col: &Column<i64>) -> i128 {
        let low_mask = vdupq_n_s64(0xFFFF_FFFF);
        let (mut high, mut low) = (vdupq_n_s64(0), vdupq_n_s64(0));
        for i in (0..LANES).step_by(2) {
            let v = load_i64(col, i);
            high = vaddq_s64(high, vshrq_n_s64::<32>(v));
            low = vaddq_s64(low, vandq_s64(v, low_mask));
        }
        let high = i128::from(vgetq_lane_s64::<0>(high)) + i128::from(vgetq_lane_s64::<1>(high));
        let low = i128::from(vgetq_lane_s64::<0>(low)) + i128::from(vgetq_lane_s64::<1>(low));
        (high << 32) + low
    }

    #[target_feature(enable = "neon")]
    pub(super) fn extreme_i64(col: &Column<i64>, max: bool) -> i64 {
        let mut best = load_i64(col, 0);
        for i in (2..LANES).step_by(2) {
            let v = load_i64(col, i);
            let better = if max { vcgtq_s64(v, best) } else { vcltq_s64(v, best) };
            best = vbslq_s64(better, v, best);
        }
        let (a, b) = (vgetq_lane_s64::<0>(best), vgetq_lane_s64::<1>(best));
        if max { a.max(b) } else { a.min(b) }
    }

    #[target_feature(enable = "neon")]
    pub(super) fn extreme_f64(col: &Column<f64>, max: bool) -> f64 {
        let mut best = load_f64(col, 0);
        for i in (2..LANES).step_by(2) {
            let v = load_f64(col, i);
            // A select on a strict comparison: `vmaxq_f64` would order -0.0 below 0.0.
            let better = if max { vcgtq_f64(v, best) } else { vcltq_f64(v, best) };
            best = vbslq_f64(better, v, best);
        }
        let (a, b) = (vgetq_lane_f64::<0>(best), vgetq_lane_f64::<1>(best));
        if (b > a) == max && b != a { b } else { a }
    }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::*;

    use super::{Cmp, Column, LANES};

    /// The two lanes at `i` of `col`.
    #[inline]
    #[target_feature(enable = "sse2")]
    fn load_i64(col: &Column<i64>, i: usize) -> __m128i {
        assert!(i + 2 <= LANES);
        // SAFETY: `i + 2 <= 64`, so the 16 bytes are inside the column; SSE2 is part of every
        // x86_64 CPU and `_mm_loadu_si128` reads without alignment.
        unsafe { _mm_loadu_si128(col.as_ptr().add(i).cast()) }
    }

    /// The two lanes at `i` of `col`.
    #[inline]
    #[target_feature(enable = "sse2")]
    fn load_f64(col: &Column<f64>, i: usize) -> __m128d {
        assert!(i + 2 <= LANES);
        // SAFETY: as in `load_i64`.
        unsafe { _mm_loadu_pd(col.as_ptr().add(i)) }
    }

    /// `a > b` on each signed 64-bit lane with SSE2's 32-bit comparisons: the high halves
    /// compared signed, and on equal high halves the low ones unsigned (flipping their sign
    /// bit turns the signed comparison into an unsigned one).
    #[inline]
    #[target_feature(enable = "sse2")]
    fn gt_i64_sse2(a: __m128i, b: __m128i) -> __m128i {
        let flip = _mm_set1_epi64x(0x8000_0000);
        let (a, b) = (_mm_xor_si128(a, flip), _mm_xor_si128(b, flip));
        let gt = _mm_cmpgt_epi32(a, b);
        let eq = _mm_cmpeq_epi32(a, b);
        // Each low half's answer copied to its high half.
        let gt_low = _mm_shuffle_epi32::<0b10_10_00_00>(gt);
        let r = _mm_or_si128(gt, _mm_and_si128(eq, gt_low));
        // The high half's answer over the whole lane.
        _mm_shuffle_epi32::<0b11_11_01_01>(r)
    }

    #[inline]
    #[target_feature(enable = "sse2")]
    fn eq_i64_sse2(a: __m128i, b: __m128i) -> __m128i {
        let eq = _mm_cmpeq_epi32(a, b);
        _mm_and_si128(eq, _mm_shuffle_epi32::<0b10_11_00_01>(eq))
    }

    #[inline]
    #[target_feature(enable = "sse2")]
    fn not(a: __m128i) -> __m128i {
        _mm_xor_si128(a, _mm_set1_epi32(-1))
    }

    #[inline]
    #[target_feature(enable = "sse2")]
    fn bits128(r: __m128i) -> u64 {
        _mm_movemask_pd(_mm_castsi128_pd(r)) as u64
    }

    #[target_feature(enable = "sse2")]
    pub(super) fn cmp_i64_sse2(cmp: Cmp, col: &Column<i64>, k: i64) -> u64 {
        let kk = _mm_set1_epi64x(k);
        let mut out = 0u64;
        for i in (0..LANES).step_by(2) {
            let v = load_i64(col, i);
            let r = match cmp {
                Cmp::Eq => eq_i64_sse2(v, kk),
                Cmp::Gt => gt_i64_sse2(v, kk),
                Cmp::Gte => not(gt_i64_sse2(kk, v)),
                Cmp::Lt => gt_i64_sse2(kk, v),
                Cmp::Lte => not(gt_i64_sse2(v, kk)),
            };
            out |= bits128(r) << i;
        }
        out
    }

    #[target_feature(enable = "sse2")]
    pub(super) fn cmp_f64_sse2(cmp: Cmp, col: &Column<f64>, k: f64) -> u64 {
        let kk = _mm_set1_pd(k);
        let mut out = 0u64;
        for i in (0..LANES).step_by(2) {
            let v = load_f64(col, i);
            let r = match cmp {
                Cmp::Eq => _mm_cmpeq_pd(v, kk),
                Cmp::Gt => _mm_cmpgt_pd(v, kk),
                Cmp::Gte => _mm_cmpge_pd(v, kk),
                Cmp::Lt => _mm_cmplt_pd(v, kk),
                Cmp::Lte => _mm_cmple_pd(v, kk),
            };
            out |= (_mm_movemask_pd(r) as u64) << i;
        }
        out
    }

    /// As NEON's: the high halves (signed) and the low halves (unsigned) summed apart.
    #[target_feature(enable = "sse2")]
    pub(super) fn sum_i64_sse2(col: &Column<i64>) -> i128 {
        let low_mask = _mm_set1_epi64x(0xFFFF_FFFF);
        let (mut high, mut low) = (_mm_setzero_si128(), _mm_setzero_si128());
        for i in (0..LANES).step_by(2) {
            let v = load_i64(col, i);
            // SSE2 has no arithmetic 64-bit shift: the high half, sign-extended by hand.
            let h = _mm_srli_epi64::<32>(v);
            let sign = _mm_shuffle_epi32::<0b11_11_01_01>(_mm_srai_epi32::<31>(v));
            let h = _mm_or_si128(h, _mm_slli_epi64::<32>(sign));
            high = _mm_add_epi64(high, h);
            low = _mm_add_epi64(low, _mm_and_si128(v, low_mask));
        }
        lanes_sum(high, low)
    }

    #[inline]
    #[target_feature(enable = "sse2")]
    fn lanes_sum(high: __m128i, low: __m128i) -> i128 {
        let (h0, h1) = (_mm_cvtsi128_si64(high), _mm_cvtsi128_si64(_mm_unpackhi_epi64(high, high)));
        let (l0, l1) = (_mm_cvtsi128_si64(low), _mm_cvtsi128_si64(_mm_unpackhi_epi64(low, low)));
        ((i128::from(h0) + i128::from(h1)) << 32) + i128::from(l0) + i128::from(l1)
    }

    #[target_feature(enable = "sse2")]
    pub(super) fn extreme_i64_sse2(col: &Column<i64>, max: bool) -> i64 {
        let mut best = load_i64(col, 0);
        for i in (2..LANES).step_by(2) {
            let v = load_i64(col, i);
            let better = if max { gt_i64_sse2(v, best) } else { gt_i64_sse2(best, v) };
            best = _mm_or_si128(_mm_and_si128(better, v), _mm_andnot_si128(better, best));
        }
        let (a, b) = (_mm_cvtsi128_si64(best), _mm_cvtsi128_si64(_mm_unpackhi_epi64(best, best)));
        if max { a.max(b) } else { a.min(b) }
    }

    #[target_feature(enable = "sse2")]
    pub(super) fn extreme_f64_sse2(col: &Column<f64>, max: bool) -> f64 {
        let mut best = load_f64(col, 0);
        for i in (2..LANES).step_by(2) {
            let v = load_f64(col, i);
            // A select on a strict comparison: `_mm_max_pd` depends on the order of its
            // arguments for -0.0 and 0.0.
            let better = if max { _mm_cmpgt_pd(v, best) } else { _mm_cmplt_pd(v, best) };
            best = _mm_or_pd(_mm_and_pd(better, v), _mm_andnot_pd(better, best));
        }
        let (a, b) = (_mm_cvtsd_f64(best), _mm_cvtsd_f64(_mm_unpackhi_pd(best, best)));
        if (b > a) == max && b != a { b } else { a }
    }

    /// The four lanes at `i` of `col`.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn load4_i64(col: &Column<i64>, i: usize) -> __m256i {
        assert!(i + 4 <= LANES);
        // SAFETY: `i + 4 <= 64`, so the 32 bytes are inside the column, and the function runs
        // with AVX2; `_mm256_loadu_si256` reads without alignment.
        unsafe { _mm256_loadu_si256(col.as_ptr().add(i).cast()) }
    }

    /// The four lanes at `i` of `col`.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn load4_f64(col: &Column<f64>, i: usize) -> __m256d {
        assert!(i + 4 <= LANES);
        // SAFETY: as in `load4_i64`.
        unsafe { _mm256_loadu_pd(col.as_ptr().add(i)) }
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    fn bits256(r: __m256i) -> u64 {
        _mm256_movemask_pd(_mm256_castsi256_pd(r)) as u64
    }

    /// # Safety
    /// The CPU has AVX2.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn cmp_i64_avx2(cmp: Cmp, col: &Column<i64>, k: i64) -> u64 {
        let kk = _mm256_set1_epi64x(k);
        let ones = _mm256_set1_epi64x(-1);
        let mut out = 0u64;
        for i in (0..LANES).step_by(4) {
            let v = load4_i64(col, i);
            let r = match cmp {
                Cmp::Eq => _mm256_cmpeq_epi64(v, kk),
                Cmp::Gt => _mm256_cmpgt_epi64(v, kk),
                Cmp::Gte => _mm256_xor_si256(_mm256_cmpgt_epi64(kk, v), ones),
                Cmp::Lt => _mm256_cmpgt_epi64(kk, v),
                Cmp::Lte => _mm256_xor_si256(_mm256_cmpgt_epi64(v, kk), ones),
            };
            out |= bits256(r) << i;
        }
        out
    }

    /// # Safety
    /// The CPU has AVX2.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn cmp_f64_avx2(cmp: Cmp, col: &Column<f64>, k: f64) -> u64 {
        let kk = _mm256_set1_pd(k);
        let mut out = 0u64;
        for i in (0..LANES).step_by(4) {
            let v = load4_f64(col, i);
            // Ordered, non-signalling predicates: false when a side is NaN, as `partial_cmp`.
            let r = match cmp {
                Cmp::Eq => _mm256_cmp_pd::<_CMP_EQ_OQ>(v, kk),
                Cmp::Gt => _mm256_cmp_pd::<_CMP_GT_OQ>(v, kk),
                Cmp::Gte => _mm256_cmp_pd::<_CMP_GE_OQ>(v, kk),
                Cmp::Lt => _mm256_cmp_pd::<_CMP_LT_OQ>(v, kk),
                Cmp::Lte => _mm256_cmp_pd::<_CMP_LE_OQ>(v, kk),
            };
            out |= (_mm256_movemask_pd(r) as u64) << i;
        }
        out
    }

    /// # Safety
    /// The CPU has AVX2.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn sum_i64_avx2(col: &Column<i64>) -> i128 {
        let low_mask = _mm256_set1_epi64x(0xFFFF_FFFF);
        let (mut high, mut low) = (_mm256_setzero_si256(), _mm256_setzero_si256());
        for i in (0..LANES).step_by(4) {
            let v = load4_i64(col, i);
            // AVX2 has no arithmetic 64-bit shift: the high half, sign-extended by hand.
            let h = _mm256_srli_epi64::<32>(v);
            let sign = _mm256_shuffle_epi32::<0b11_11_01_01>(_mm256_srai_epi32::<31>(v));
            high = _mm256_add_epi64(high, _mm256_or_si256(h, _mm256_slli_epi64::<32>(sign)));
            low = _mm256_add_epi64(low, _mm256_and_si256(v, low_mask));
        }
        let fold =
            |x: __m256i| _mm_add_epi64(_mm256_castsi256_si128(x), _mm256_extracti128_si256::<1>(x));
        lanes_sum(fold(high), fold(low))
    }

    /// # Safety
    /// The CPU has AVX2.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn extreme_i64_avx2(col: &Column<i64>, max: bool) -> i64 {
        let mut best = load4_i64(col, 0);
        for i in (4..LANES).step_by(4) {
            let v = load4_i64(col, i);
            let better =
                if max { _mm256_cmpgt_epi64(v, best) } else { _mm256_cmpgt_epi64(best, v) };
            best = _mm256_blendv_epi8(best, v, better);
        }
        let mut lanes = [0i64; 4];
        // SAFETY: `lanes` holds the 32 bytes written, without alignment needed.
        unsafe { _mm256_storeu_si256(lanes.as_mut_ptr().cast(), best) };
        let it = lanes.into_iter();
        if max { it.max() } else { it.min() }.unwrap_or_default()
    }

    /// # Safety
    /// The CPU has AVX2.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn extreme_f64_avx2(col: &Column<f64>, max: bool) -> f64 {
        let mut best = load4_f64(col, 0);
        for i in (4..LANES).step_by(4) {
            let v = load4_f64(col, i);
            let better = if max {
                _mm256_cmp_pd::<_CMP_GT_OQ>(v, best)
            } else {
                _mm256_cmp_pd::<_CMP_LT_OQ>(v, best)
            };
            best = _mm256_blendv_pd(best, v, better);
        }
        let mut lanes = [0f64; 4];
        // SAFETY: `lanes` holds the 32 bytes written, without alignment needed.
        unsafe { _mm256_storeu_pd(lanes.as_mut_ptr(), best) };
        lanes[1..].iter().fold(lanes[0], |b, &x| if (x > b) == max && x != b { x } else { b })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMPS: [Cmp; 5] = [Cmp::Eq, Cmp::Gt, Cmp::Gte, Cmp::Lt, Cmp::Lte];

    /// A pseudo-random sequence (SplitMix64), for columns with repeated and extreme values.
    fn draws(mut state: u64) -> impl FnMut() -> u64 {
        move || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
    }

    const I64_EDGES: [i64; 9] = [0, 1, -1, i64::MAX, i64::MIN, 1 << 32, -(1 << 32), 1 << 53, 7];
    const F64_EDGES: [f64; 9] =
        [0.0, -0.0, 1.5, -1.5, f64::INFINITY, f64::NEG_INFINITY, f64::MAX, f64::MIN_POSITIVE, 7.0];

    fn i64_column(next: &mut impl FnMut() -> u64) -> Column<i64> {
        std::array::from_fn(|_| match next() % 3 {
            0 => I64_EDGES[(next() % 9) as usize],
            1 => (next() % 16) as i64 - 8,
            _ => next() as i64,
        })
    }

    fn f64_column(next: &mut impl FnMut() -> u64, nan: bool) -> Column<f64> {
        std::array::from_fn(|_| match next() % 4 {
            0 => F64_EDGES[(next() % 9) as usize],
            1 if nan => f64::NAN,
            1 => -0.0,
            2 => (next() % 16) as f64 - 8.0,
            _ => f64::from_bits(next() >> 2),
        })
    }

    /// Fewer rounds under Miri, which interprets every instruction.
    const ROUNDS: u64 = if cfg!(miri) { 4 } else { 300 };

    #[test]
    fn integer_comparisons_equal_the_scalar_kernel() {
        let mut next = draws(1);
        for _ in 0..ROUNDS {
            let col = i64_column(&mut next);
            let k = col[(next() % 64) as usize];
            for k in [k, k.wrapping_add(1), i64::MIN, i64::MAX] {
                for c in CMPS {
                    assert_eq!(cmp_i64(c, &col, k), scalar::cmp_i64(c, &col, k), "{c:?} {k}");
                }
            }
        }
    }

    #[test]
    fn double_comparisons_equal_the_scalar_kernel() {
        let mut next = draws(2);
        for _ in 0..ROUNDS {
            let col = f64_column(&mut next, true);
            let k = col[(next() % 64) as usize];
            for k in [k, 0.0, -0.0, f64::INFINITY, f64::NAN] {
                for c in CMPS {
                    assert_eq!(cmp_f64(c, &col, k), scalar::cmp_f64(c, &col, k), "{c:?} {k}");
                }
            }
        }
        // -0.0 equals 0.0.
        let zeros: Column<f64> = std::array::from_fn(|l| if l % 2 == 0 { 0.0 } else { -0.0 });
        assert_eq!(cmp_f64(Cmp::Eq, &zeros, 0.0), u64::MAX);
        assert_eq!(cmp_f64(Cmp::Lt, &zeros, 0.0), 0);
    }

    #[test]
    fn sums_are_exact() {
        let mut next = draws(3);
        for _ in 0..ROUNDS {
            let col = i64_column(&mut next);
            let mask = next();
            let want: i128 =
                (0..LANES).filter(|l| (mask >> l) & 1 == 1).map(|l| i128::from(col[l])).sum();
            assert_eq!(sum_i64(&col, mask), want);
        }
        assert_eq!(sum_i64(&[i64::MAX; LANES], u64::MAX), i128::from(i64::MAX) * 64);
        assert_eq!(sum_i64(&[i64::MIN; LANES], u64::MAX), i128::from(i64::MIN) * 64);
        assert_eq!(sum_i64(&[5; LANES], 0), 0);
    }

    #[test]
    fn extremes_equal_the_scalar_ones() {
        let mut next = draws(4);
        for _ in 0..ROUNDS {
            let col = i64_column(&mut next);
            let mask = next() | (1 << (next() % 64));
            let lanes = || (0..LANES).filter(|l| (mask >> l) & 1 == 1).map(|l| col[l]);
            assert_eq!(extreme_i64(&col, mask, true), lanes().max());
            assert_eq!(extreme_i64(&col, mask, false), lanes().min());
            let col = f64_column(&mut next, true);
            for max in [true, false] {
                let (best, nan) = extreme_f64(&col, mask, max);
                let want_nan = (0..LANES)
                    .filter(|l| (mask >> l) & 1 == 1 && col[*l].is_nan())
                    .fold(0, |m, l| m | 1 << l);
                assert_eq!(nan, want_nan);
                let numbers: Vec<f64> = (0..LANES)
                    .filter(|l| (mask >> l) & 1 == 1 && !col[*l].is_nan())
                    .map(|l| col[l])
                    .collect();
                let want =
                    numbers.iter().copied().reduce(|b, x| if max { b.max(x) } else { b.min(x) });
                // -0.0 and 0.0 are the same extreme.
                assert_eq!(best.map(|b| b == want.unwrap_or(f64::NAN)), want.map(|_| true));
            }
        }
        assert_eq!(extreme_i64(&[1; LANES], 0, true), None);
        assert_eq!(extreme_f64(&[f64::NAN; LANES], 0b101, true), (None, 0b101));
    }

    /// Both x86_64 kernels, SSE2 always and AVX2 when the CPU has it, against the scalar ones
    /// (the dispatch above takes only one of them).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn every_x86_kernel_equals_the_scalar_one() {
        let mut next = draws(5);
        let avx2 = has_avx2();
        for _ in 0..ROUNDS {
            let ints = i64_column(&mut next);
            let doubles = f64_column(&mut next, true);
            let (k, x) = (ints[(next() % 64) as usize], doubles[(next() % 64) as usize]);
            for c in CMPS {
                let (want_i, want_f) =
                    (scalar::cmp_i64(c, &ints, k), scalar::cmp_f64(c, &doubles, x));
                // SAFETY: SSE2 is part of every x86_64 CPU.
                assert_eq!(unsafe { x86::cmp_i64_sse2(c, &ints, k) }, want_i);
                // SAFETY: SSE2 is part of every x86_64 CPU.
                assert_eq!(unsafe { x86::cmp_f64_sse2(c, &doubles, x) }, want_f);
                if avx2 {
                    // SAFETY: the CPU has AVX2.
                    assert_eq!(unsafe { x86::cmp_i64_avx2(c, &ints, k) }, want_i);
                    // SAFETY: the CPU has AVX2.
                    assert_eq!(unsafe { x86::cmp_f64_avx2(c, &doubles, x) }, want_f);
                }
            }
            let numbers = f64_column(&mut next, false);
            // SAFETY: SSE2 is part of every x86_64 CPU.
            assert_eq!(unsafe { x86::sum_i64_sse2(&ints) }, scalar::sum_i64(&ints));
            for max in [true, false] {
                // SAFETY: SSE2 is part of every x86_64 CPU.
                let got = unsafe { x86::extreme_i64_sse2(&ints, max) };
                assert_eq!(got, scalar::extreme_i64(&ints, max));
                let want = scalar::extreme_f64(&numbers, max);
                // SAFETY: SSE2 is part of every x86_64 CPU.
                assert_eq!(unsafe { x86::extreme_f64_sse2(&numbers, max) }, want);
                if avx2 {
                    // SAFETY: the CPU has AVX2.
                    let got = unsafe { x86::extreme_i64_avx2(&ints, max) };
                    assert_eq!(got, scalar::extreme_i64(&ints, max));
                    // SAFETY: the CPU has AVX2.
                    assert_eq!(unsafe { x86::extreme_f64_avx2(&numbers, max) }, want);
                }
            }
            if avx2 {
                // SAFETY: the CPU has AVX2.
                assert_eq!(unsafe { x86::sum_i64_avx2(&ints) }, scalar::sum_i64(&ints));
            }
        }
    }
}
