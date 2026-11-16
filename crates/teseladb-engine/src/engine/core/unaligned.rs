//! Values stored at addresses that may not satisfy their type's alignment.
//!
//! Network headers and on-disk records often place multi-byte integers at arbitrary offsets.
//! Reading such a field through an ordinary `&T` is undefined behaviour in Rust when the address
//! is misaligned. [`Unaligned<T>`] is a `#[repr(C, packed)]` wrapper (alignment 1): the compiler
//! knows every access may be misaligned and emits the right loads and stores, which on x86 and
//! ARM are the same instructions as an aligned access, and on strict-alignment CPUs a safe
//! multi-instruction sequence.
//!
//! Only `Copy` types are supported, because a packed field can be read and written by value but
//! never borrowed.

/// A `T` stored with alignment 1. Read with [`get`](Self::get), write with [`set`](Self::set);
/// never take a reference to `raw`, which may be misaligned.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct Unaligned<T: Copy> {
    /// The stored value. Access it only by value (copy in or out), never by reference.
    pub raw: T,
}

impl<T: Copy> Unaligned<T> {
    /// Wraps `x`.
    #[inline]
    pub const fn new(x: T) -> Self {
        Unaligned { raw: x }
    }
    /// Replaces the stored value with `x`.
    #[inline]
    pub fn set(&mut self, x: T) {
        self.raw = x;
    }
    /// Returns a copy of the stored value.
    #[inline]
    pub fn get(&self) -> T {
        self.raw
    }
}

impl<T: Copy> From<T> for Unaligned<T> {
    #[inline]
    fn from(x: T) -> Self {
        Unaligned::new(x)
    }
}

/// Reinterprets a pointer to any `F` as a pointer to an [`Unaligned<T>`] at the same address.
///
/// Deprecated: accessing memory of one type through a pointer to another breaks the aliasing
/// rules; copy the bytes out (for example with `read_unaligned`) instead.
#[deprecated(
    note = "violates strict aliasing rules; copy the bytes out with read_unaligned instead"
)]
#[inline]
pub fn unaligned_cast<T: Copy, F>(p: *mut F) -> *mut Unaligned<T> {
    p.cast()
}

/// Constant-pointer version of [`unaligned_cast`]; deprecated for the same reason.
#[deprecated(
    note = "violates strict aliasing rules; copy the bytes out with read_unaligned instead"
)]
#[inline]
pub fn unaligned_cast_const<T: Copy, F>(p: *const F) -> *const Unaligned<T> {
    p.cast()
}
