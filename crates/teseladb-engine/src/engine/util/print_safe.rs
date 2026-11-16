//! Async-signal-safe printing.
//!
//! Signal handlers (for example the crash handler that prints a backtrace) may not
//! allocate, lock or use buffered I/O. These functions format integers into caller-owned
//! stack buffers and write to standard error with the raw `write(2)` system call, retrying
//! on `EINTR`. Everything here is async-signal safe and allocation-free.

use std::ffi::{c_int, c_void};

unsafe extern "C" {
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
}

const STDERR_FILENO: c_int = 2;
const EINTR: i32 = 4;

/// Writes `len` bytes starting at `str` to standard error. Async-signal safe.
///
/// Retries partial writes and `EINTR`; stops silently on any other error or on a
/// zero-length write.
///
/// # Safety
/// `str` points to `len` readable bytes (it is not read when `len` is 0).
#[inline]
pub unsafe fn print_safe_len(str: *const u8, len: usize) {
    let mut str = str;
    let mut len = len;
    while len != 0 {
        // SAFETY: the caller's contract: `str` points to `len` readable bytes.
        let result = unsafe { write(STDERR_FILENO, str as *const c_void, len) };
        if result > 0 {
            len -= result as usize;
            // SAFETY: result <= len, still within the buffer.
            str = unsafe { str.add(result as usize) };
        } else if result == 0 {
            break;
        } else {
            if std::io::Error::last_os_error().raw_os_error() == Some(EINTR) {
                // retry
            } else {
                break; // Nothing else can be done from a signal handler.
            }
        }
    }
}

/// Writes `str` to standard error. Async-signal safe.
#[inline]
pub fn print_safe(str: &[u8]) {
    // SAFETY: the slice is `len` readable bytes.
    unsafe { print_safe_len(str.as_ptr(), str.len()) };
}

/// A primitive integer usable by the signal-safe converters.
pub trait SafeIntegral: Copy {
    /// `size_of::<Self>()`.
    const SIZE: usize;
    /// The low 4 bits, as an index into a hex digit table.
    fn low_nibble(self) -> usize;
    /// `self >> 4` (arithmetic shift for signed types).
    fn shr4(self) -> Self;
    /// Whether `self == 0`.
    fn is_zero(self) -> bool;
    /// `self % 10`, as a digit value.
    fn mod10(self) -> u8;
    /// `self / 10`.
    fn div10(self) -> Self;
}

macro_rules! impl_safe_integral {
    ($($t:ty),*) => {$(
        impl SafeIntegral for $t {
            const SIZE: usize = std::mem::size_of::<$t>();
            fn low_nibble(self) -> usize { (self & 0xf) as usize }
            // Arithmetic shift for signed types.
            fn shr4(self) -> Self { self >> 4 }
            fn is_zero(self) -> bool { self == 0 }
            fn mod10(self) -> u8 { (self % 10) as u8 }
            fn div10(self) -> Self { self / 10 }
        }
    )*};
}
impl_safe_integral!(u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize);

/// Fills `buf` with the hexadecimal representation of `n`, right-aligned and padded on the
/// left with `PADDING`, and returns the index of the first digit.
///
/// For example, a 4-byte buffer and `12u16` with `PADDING = b' '` give `"   c"` and 3.
///
/// # Panics
///
/// Aborts the process (via [`engine_assert!`](crate::engine_assert)) if `buf` is too small
/// for the digits.
pub fn convert_hex_safe_with<Integral: SafeIntegral, const PADDING: u8>(
    buf: &mut [u8],
    n: Integral,
) -> usize {
    let digits = b"0123456789abcdef";
    let bufsz = buf.len();
    for b in buf.iter_mut() {
        *b = PADDING;
    }
    let mut p = bufsz;
    let mut n = n;
    loop {
        crate::engine_assert!(p > 0);
        p -= 1;
        buf[p] = digits[n.low_nibble()];
        n = n.shr4();
        if n.is_zero() {
            break;
        }
    }
    p
}

/// Space-padded [`convert_hex_safe_with`]: fills `buf` with the right-aligned hex digits
/// of `n` and returns the index of the first digit.
pub fn convert_hex_safe<Integral: SafeIntegral>(buf: &mut [u8], n: Integral) -> usize {
    convert_hex_safe_with::<Integral, b' '>(buf, n)
}

/// Fills `buf` with the zero-padded hexadecimal representation of `n`.
/// For example, a 4-byte buffer and `12u16` give `"000c"`.
pub fn convert_zero_padded_hex_safe<Integral: SafeIntegral>(buf: &mut [u8], n: Integral) {
    convert_hex_safe_with::<Integral, b'0'>(buf, n);
}

/// Prints `n` to standard error in zero-padded hexadecimal, two digits per byte of the
/// type. For example, `12u16` prints `000c`. Async-signal safe.
pub fn print_zero_padded_hex_safe<Integral: SafeIntegral>(n: Integral) {
    let mut buf = [0u8; 32];
    let len = Integral::SIZE * 2;
    convert_zero_padded_hex_safe(&mut buf[..len], n);
    print_safe(&buf[..len]);
}

/// Writes the decimal representation of `n` at the start of `buf` and returns its length.
/// For example, `12` writes `"12"` and returns 2.
///
/// The digits are produced right to left in a scratch buffer of `bufsz` positions and
/// then copied out, so `bufsz` must be `3 * size_of::<Integral>()` (as
/// [`print_decimal_safe`] passes); `buf` must have room for the digits. Negative values
/// are not supported.
///
/// # Panics
///
/// Aborts the process (via [`engine_assert!`](crate::engine_assert)) if the digits do not
/// fit in `bufsz`; panics if `buf` is too short.
pub fn convert_decimal_safe<Integral: SafeIntegral>(
    buf: &mut [u8],
    bufsz: usize,
    n: Integral,
) -> usize {
    let mut tmp = [0u8; 48];
    let tmp_len = Integral::SIZE * 3;
    let mut i = bufsz;
    let mut n = n;
    loop {
        crate::engine_assert!(i > 0);
        i -= 1;
        tmp[i] = b'0' + n.mod10();
        n = n.div10();
        if n.is_zero() {
            break;
        }
    }
    buf[..tmp_len - i].copy_from_slice(&tmp[i..tmp_len]);
    tmp_len - i
}

/// Prints the decimal representation of `n` to standard error. For example, `12` prints
/// `12`. Async-signal safe.
pub fn print_decimal_safe<Integral: SafeIntegral>(n: Integral) {
    let mut buf = [0u8; 48];
    let i = Integral::SIZE * 3;
    let len = convert_decimal_safe(&mut buf, i, n);
    print_safe(&buf[..len]);
}
