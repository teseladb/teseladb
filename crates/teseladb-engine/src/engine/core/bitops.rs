//! Bit-counting helpers and integer base-2 logarithms.
//!
//! The `count_*_zeros` functions are thin `const` wrappers over the standard library's
//! `leading_zeros` / `trailing_zeros` (a single instruction on every supported CPU). The `_ul`
//! and `_ull` variants take `u64`; all three exist so that callers can pick the width by name.
//! [`log2ceil`] and [`log2floor`] are generic over [`Integral`]; the `const` per-width versions
//! at the end of the module serve constant expressions, since trait methods cannot be `const`.

/// Number of leading zero bits of a `u32` (32 for zero).
#[inline]
pub const fn count_leading_zeros(x: u32) -> u32 {
    x.leading_zeros()
}

/// Number of leading zero bits of a `u64` (64 for zero).
#[inline]
pub const fn count_leading_zeros_ul(x: u64) -> u32 {
    x.leading_zeros()
}

/// Number of leading zero bits of a `u64` (64 for zero); same as [`count_leading_zeros_ul`].
#[inline]
pub const fn count_leading_zeros_ull(x: u64) -> u32 {
    x.leading_zeros()
}

/// Number of trailing zero bits of a `u32` (32 for zero).
#[inline]
pub const fn count_trailing_zeros(x: u32) -> u32 {
    x.trailing_zeros()
}

/// Number of trailing zero bits of a `u64` (64 for zero).
#[inline]
pub const fn count_trailing_zeros_ul(x: u64) -> u32 {
    x.trailing_zeros()
}

/// Number of trailing zero bits of a `u64` (64 for zero); same as [`count_trailing_zeros_ul`].
#[inline]
pub const fn count_trailing_zeros_ull(x: u64) -> u32 {
    x.trailing_zeros()
}

/// The integer types [`log2ceil`] and [`log2floor`] accept: every primitive signed and unsigned
/// integer. Signed values must be positive.
pub trait Integral: Copy {
    /// Bit width of the type, sign bit included (32 for `i32` and `u32`): `leading_zeros`
    /// of a positive signed value counts the sign bit, so the logarithms subtract from the
    /// full width.
    const DIGITS: u32;
    /// Whether the value is 1.
    fn is_one(self) -> bool;
    /// Leading zero bits of `self - 1`.
    fn clz_minus_one(self) -> u32;
    /// Leading zero bits of `self`.
    fn clz(self) -> u32;
}

macro_rules! impl_integral {
    ($($t:ty => $digits:expr),*) => {$(
        impl Integral for $t {
            const DIGITS: u32 = $digits;
            #[inline]
            fn is_one(self) -> bool { self == 1 }
            #[inline]
            fn clz_minus_one(self) -> u32 { (self - 1).leading_zeros() }
            #[inline]
            fn clz(self) -> u32 { self.leading_zeros() }
        }
    )*};
}

impl_integral!(u8 => 8, u16 => 16, u32 => 32, u64 => 64, u128 => 128, usize => usize::BITS,
    i8 => 8, i16 => 16, i32 => 32, i64 => 64, i128 => 128, isize => isize::BITS);

/// Smallest `k` such that `2^k >= n`; `n` must be positive (0 for `n == 1`).
#[inline]
pub fn log2ceil<T: Integral>(n: T) -> u32 {
    if n.is_one() {
        return 0;
    }
    T::DIGITS - n.clz_minus_one()
}

/// Largest `k` such that `2^k <= n`; `n` must be positive.
#[inline]
pub fn log2floor<T: Integral>(n: T) -> u32 {
    T::DIGITS - n.clz() - 1
}

// `const` per-width versions of `log2ceil` / `log2floor`, usable in constant expressions
// (trait methods cannot be `const`).

macro_rules! const_log2 {
    ($($ceil:ident, $floor:ident, $t:ty);*) => {$(
        #[doc = concat!("`const` [`log2ceil`] for `", stringify!($t), "`; `n` must be positive.")]
        #[inline]
        pub const fn $ceil(n: $t) -> u32 {
            if n == 1 {
                return 0;
            }
            <$t>::BITS - (n - 1).leading_zeros()
        }

        #[doc = concat!("`const` [`log2floor`] for `", stringify!($t), "`; `n` must be positive.")]
        #[inline]
        pub const fn $floor(n: $t) -> u32 {
            <$t>::BITS - n.leading_zeros() - 1
        }
    )*};
}

const_log2!(log2ceil_u32, log2floor_u32, u32; log2ceil_u64, log2floor_u64, u64;
    log2ceil_usize, log2floor_usize, usize);
