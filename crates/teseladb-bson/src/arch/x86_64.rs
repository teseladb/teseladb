//! SSE2 and AVX2: 16 or 32 bytes compared with zero at a time; the byte mask's trailing zeros give
//! the position.

use std::arch::x86_64::{
    _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_setzero_si128, _mm256_cmpeq_epi8,
    _mm256_loadu_si256, _mm256_movemask_epi8, _mm256_setzero_si256,
};

/// The first 0 byte of `b`, 32 bytes at a time.
///
/// # Safety
/// The CPU must have AVX2.
#[target_feature(enable = "avx2")]
pub(super) unsafe fn find_nul_avx2(b: &[u8]) -> Option<usize> {
    let (blocks, rest) = b.as_chunks::<32>();
    let zero = _mm256_setzero_si256();
    for (i, blk) in blocks.iter().enumerate() {
        // SAFETY: each block has 32 bytes; the load takes any alignment.
        let v = unsafe { _mm256_loadu_si256(blk.as_ptr().cast()) };
        let mask = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, zero)).cast_unsigned();
        if mask != 0 {
            return Some(32 * i + mask.trailing_zeros() as usize);
        }
    }
    // SAFETY: a CPU with AVX2 has SSE2.
    unsafe { find_nul_sse2(rest) }.map(|p| 32 * blocks.len() + p)
}

/// The first 0 byte of `b`, 16 bytes at a time.
///
/// # Safety
/// The CPU must have SSE2 (every x86-64 CPU does).
#[target_feature(enable = "sse2")]
pub(super) unsafe fn find_nul_sse2(b: &[u8]) -> Option<usize> {
    let (blocks, rest) = b.as_chunks::<16>();
    let zero = _mm_setzero_si128();
    for (i, blk) in blocks.iter().enumerate() {
        // SAFETY: each block has 16 bytes; the load takes any alignment.
        let v = unsafe { _mm_loadu_si128(blk.as_ptr().cast()) };
        let mask = _mm_movemask_epi8(_mm_cmpeq_epi8(v, zero)).cast_unsigned();
        if mask != 0 {
            return Some(16 * i + mask.trailing_zeros() as usize);
        }
    }
    super::bytewise(rest).map(|p| 16 * blocks.len() + p)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_sse2_search_matches_the_bytewise_one() {
        // SAFETY: every x86-64 CPU has SSE2.
        super::super::tests::check(|b| unsafe { super::find_nul_sse2(b) });
    }

    #[test]
    fn the_avx2_search_matches_the_bytewise_one_on_a_cpu_with_avx2() {
        // The x86-64 runs of scripts/test-x86_64.sh use `qemu -cpu max`, which has AVX2: the test
        // asserts it there instead of passing without running the search.
        assert!(std::arch::is_x86_feature_detected!("avx2"), "this CPU has no AVX2");
        // SAFETY: AVX2 was checked above.
        super::super::tests::check(|b| unsafe { super::find_nul_avx2(b) });
    }
}
