//! Binary size units (powers of 1024) as constants and `const` constructors.

/// 2^10 bytes.
pub const KB: usize = 1 << 10;
/// 2^20 bytes.
pub const MB: usize = 1 << 20;
/// 2^30 bytes.
pub const GB: usize = 1 << 30;

/// `n` kibibytes (`n * 2^10`) in bytes.
#[inline(always)]
pub const fn kib(n: u64) -> usize {
    (n << 10) as usize
}

/// `n` mebibytes (`n * 2^20`) in bytes.
#[inline(always)]
pub const fn mib(n: u64) -> usize {
    (n << 20) as usize
}

/// `n` gibibytes (`n * 2^30`) in bytes.
#[inline(always)]
pub const fn gib(n: u64) -> usize {
    (n << 30) as usize
}

/// `n` tebibytes (`n * 2^40`) in bytes; truncated on targets whose `usize` is narrower than 64
/// bits.
#[inline(always)]
pub const fn tib(n: u64) -> usize {
    (n << 40) as usize
}
