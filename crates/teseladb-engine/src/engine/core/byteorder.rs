//! Byte-order conversions between the CPU's native order and little or big endian, plus
//! unaligned reads and writes of integers in a given byte order.
//!
//! All functions are `#[inline(always)]` and compile to a no-op or a single byte swap; the
//! pointer versions use unaligned loads and stores, so they work at any offset in a packet or
//! record buffer.

use crate::engine::core::unaligned::Unaligned;

/// The integer types with a byte order.
pub trait ByteOrder: Copy {
    /// Converts from the native byte order to little endian.
    fn native_to_little(self) -> Self;
    /// Converts from little endian to the native byte order.
    fn little_to_native(self) -> Self;
    /// Converts from the native byte order to big endian.
    fn native_to_big(self) -> Self;
    /// Converts from big endian to the native byte order.
    fn big_to_native(self) -> Self;
}

macro_rules! impl_byte_order {
    ($($t:ty),*) => {$(
        impl ByteOrder for $t {
            #[inline(always)]
            fn native_to_little(self) -> Self { self.to_le() }
            #[inline(always)]
            fn little_to_native(self) -> Self { <$t>::from_le(self) }
            #[inline(always)]
            fn native_to_big(self) -> Self { self.to_be() }
            #[inline(always)]
            fn big_to_native(self) -> Self { <$t>::from_be(self) }
        }
    )*};
}

impl_byte_order!(u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize);

/// Converts from the CPU byte order to little endian.
#[inline(always)]
pub fn cpu_to_le<T: ByteOrder>(x: T) -> T {
    x.native_to_little()
}

/// Converts from little endian to the CPU byte order.
#[inline(always)]
pub fn le_to_cpu<T: ByteOrder>(x: T) -> T {
    x.little_to_native()
}

/// Converts from the CPU byte order to big endian.
#[inline(always)]
pub fn cpu_to_be<T: ByteOrder>(x: T) -> T {
    x.native_to_big()
}

/// Converts from big endian to the CPU byte order.
#[inline(always)]
pub fn be_to_cpu<T: ByteOrder>(x: T) -> T {
    x.big_to_native()
}

/// Reads an [`Unaligned`] value and converts it from the CPU byte order to little endian.
#[inline(always)]
pub fn cpu_to_le_unaligned<T: ByteOrder>(v: &Unaligned<T>) -> T {
    cpu_to_le(v.get())
}

/// Reads an [`Unaligned`] little-endian value and converts it to the CPU byte order.
#[inline(always)]
pub fn le_to_cpu_unaligned<T: ByteOrder>(v: &Unaligned<T>) -> T {
    le_to_cpu(v.get())
}

/// Reads a little-endian `T` from `p` (unaligned, `p` holds at least `size_of::<T>()` bytes).
///
/// # Safety
/// `p` must be valid for reading `size_of::<T>()` bytes.
#[inline(always)]
pub unsafe fn read_le<T: ByteOrder>(p: *const u8) -> T {
    // SAFETY: the caller guarantees `p` is readable for `size_of::<T>()` bytes, and
    // read_unaligned has no alignment requirement.
    le_to_cpu(unsafe { (p as *const T).read_unaligned() })
}

/// Writes `datum` at `p` in little endian (unaligned).
///
/// # Safety
/// `p` must be valid for writing `size_of::<T>()` bytes.
#[inline(always)]
pub unsafe fn write_le<T: ByteOrder>(p: *mut u8, datum: T) {
    let datum = cpu_to_le(datum);
    // SAFETY: the caller guarantees `p` is writable for `size_of::<T>()` bytes, and
    // write_unaligned has no alignment requirement.
    unsafe { (p as *mut T).write_unaligned(datum) };
}

/// Reads a big-endian `T` from `p` (unaligned).
///
/// # Safety
/// `p` must be valid for reading `size_of::<T>()` bytes.
#[inline(always)]
pub unsafe fn read_be<T: ByteOrder>(p: *const u8) -> T {
    // SAFETY: the caller guarantees `p` is readable for `size_of::<T>()` bytes; the read is
    // unaligned.
    be_to_cpu(unsafe { (p as *const T).read_unaligned() })
}

/// Writes `datum` at `p` in big endian (unaligned).
///
/// # Safety
/// `p` must be valid for writing `size_of::<T>()` bytes.
#[inline(always)]
pub unsafe fn write_be<T: ByteOrder>(p: *mut u8, datum: T) {
    let datum = cpu_to_be(datum);
    // SAFETY: the caller guarantees `p` is writable for `size_of::<T>()` bytes; the write is
    // unaligned.
    unsafe { (p as *mut T).write_unaligned(datum) };
}

/// Reads a big-endian `T` at `*p` and advances `*p` past it.
///
/// # Safety
/// `*p` must be valid for reading `size_of::<T>()` bytes.
#[inline(always)]
pub unsafe fn consume_be<T: ByteOrder>(p: &mut *const u8) -> T {
    // SAFETY: the caller guarantees `*p` is readable for `size_of::<T>()` bytes.
    let ret = unsafe { read_be::<T>(*p) };
    *p = p.wrapping_add(size_of::<T>());
    ret
}

/// Writes `datum` in big endian at `*p` and advances `*p` past it.
///
/// # Safety
/// `*p` must be valid for writing `size_of::<T>()` bytes.
#[inline(always)]
pub unsafe fn produce_be<T: ByteOrder>(p: &mut *mut u8, datum: T) {
    // SAFETY: the caller guarantees `*p` is writable for `size_of::<T>()` bytes.
    unsafe { write_be::<T>(*p, datum) };
    *p = p.wrapping_add(size_of::<T>());
}
