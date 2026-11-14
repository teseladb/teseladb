//! Constant-time helpers: comparisons and selections whose running time does not depend on the
//! secret values compared or selected.

use std::hint::black_box;

/// Returns `true` when `a` and `b` are equal. The time depends only on the lengths.
#[must_use]
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    black_box(diff) == 0
}

/// All ones when `bit` is 1 and zero when it is 0.
pub(crate) fn mask64(bit: u64) -> u64 {
    black_box(bit & 1).wrapping_neg()
}

/// All ones when `x` is zero, zero otherwise.
pub(crate) fn is_zero64(x: u64) -> u64 {
    mask64(!(x | x.wrapping_neg()) >> 63)
}

/// Selects `a` when `mask` is all ones and `b` when it is zero.
pub(crate) fn select64(mask: u64, a: u64, b: u64) -> u64 {
    b ^ (mask & (a ^ b))
}

/// Swaps the slices when `mask` is all ones.
pub(crate) fn cswap64(mask: u64, a: &mut [u64], b: &mut [u64]) {
    for (x, y) in a.iter_mut().zip(b.iter_mut()) {
        let t = mask & (*x ^ *y);
        *x ^= t;
        *y ^= t;
    }
}
