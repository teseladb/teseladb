//! Power-of-two alignment helpers for integers and byte pointers.
//!
//! Every function here assumes `align` is a non-zero power of two; with any other value the
//! result is meaningless (no check is made, so the helpers stay branch-free on hot paths such as
//! buffer and DMA size computations). The integer versions use wrapping arithmetic, so rounding
//! a value within `align - 1` of the type's maximum up wraps to zero instead of panicking.

/// Rounding to a power-of-two boundary, implemented for every primitive integer type.
pub trait AlignInt: Copy {
    /// Rounds `self` up to the next multiple of `align` (a power of two); a value already
    /// aligned is returned unchanged.
    fn align_up(self, align: Self) -> Self;
    /// Rounds `self` down to the previous multiple of `align` (a power of two); a value already
    /// aligned is returned unchanged.
    fn align_down(self, align: Self) -> Self;
}

macro_rules! impl_align_int {
    ($($t:ty),*) => {$(
        impl AlignInt for $t {
            #[inline]
            fn align_up(self, align: Self) -> Self {
                (self.wrapping_add(align).wrapping_sub(1)) & !(align.wrapping_sub(1))
            }
            #[inline]
            fn align_down(self, align: Self) -> Self {
                self & !(align.wrapping_sub(1))
            }
        }
    )*};
}

impl_align_int!(u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize);

/// Rounds `v` up to the next multiple of `align`, which must be a power of two.
#[inline]
pub fn align_up<T: AlignInt>(v: T, align: T) -> T {
    v.align_up(align)
}

/// Rounds a byte pointer up to the next address that is a multiple of `align` (a power of two).
///
/// The pointer keeps its provenance: the result is `v` offset by the padding, computed with
/// wrapping pointer arithmetic, so the function itself never dereferences or invalidates it.
#[inline]
pub fn align_up_ptr(v: *mut u8, align: usize) -> *mut u8 {
    v.wrapping_add(align_up(v as usize, align) - v as usize)
}

/// Rounds `v` down to the previous multiple of `align`, which must be a power of two.
#[inline]
pub fn align_down<T: AlignInt>(v: T, align: T) -> T {
    v.align_down(align)
}

/// Rounds a byte pointer down to the previous address that is a multiple of `align` (a power of
/// two), keeping the pointer's provenance as [`align_up_ptr`] does.
#[inline]
pub fn align_down_ptr(v: *mut u8, align: usize) -> *mut u8 {
    v.wrapping_sub(v as usize - align_down(v as usize, align))
}

/// `const` version of [`align_up`] for `usize`, usable in constant expressions.
///
/// # Panics
///
/// Overflows (and panics in debug builds) when `v + align - 1` exceeds `usize::MAX`.
#[inline]
pub const fn align_up_usize(v: usize, align: usize) -> usize {
    (v + align - 1) & !(align - 1)
}

/// `const` version of [`align_down`] for `usize`, usable in constant expressions.
#[inline]
pub const fn align_down_usize(v: usize, align: usize) -> usize {
    v & !(align - 1)
}
