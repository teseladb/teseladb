//! NEON: 16 bytes compared with zero at a time; the narrowing shift turns the comparison into a
//! 64-bit mask with 4 bits per byte, whose trailing zeros give the position.

use std::arch::aarch64::{
    vceqzq_u8, vget_lane_u64, vld1q_u8, vreinterpret_u64_u8, vreinterpretq_u16_u8, vshrn_n_u16,
};

/// The first 0 byte of `b`.
///
/// # Safety
/// The CPU must have NEON (every `AArch64` CPU does).
#[target_feature(enable = "neon")]
pub(super) unsafe fn find_nul(b: &[u8]) -> Option<usize> {
    let (blocks, rest) = b.as_chunks::<16>();
    for (i, blk) in blocks.iter().enumerate() {
        // SAFETY: each block has 16 bytes.
        let v = unsafe { vld1q_u8(blk.as_ptr()) };
        let eq = vceqzq_u8(v);
        let mask =
            vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(eq))));
        if mask != 0 {
            return Some(16 * i + (mask.trailing_zeros() / 4) as usize);
        }
    }
    super::bytewise(rest).map(|p| 16 * blocks.len() + p)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_neon_search_matches_the_bytewise_one() {
        // SAFETY: every `AArch64` CPU has NEON.
        super::super::tests::check(|b| unsafe { super::find_nul(b) });
    }
}
