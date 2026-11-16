//! Scatter/gather vector helpers.
//!
//! [`Iovec`] has the layout of the POSIX `struct iovec` and is passed directly to
//! `readv`/`writev` and to io_uring and linux-aio submissions. The helpers here sum the
//! lengths of a vector and trim it to the kernel's `IOV_MAX` limit.

use std::ffi::c_void;

/// One scatter/gather element, layout-compatible with the POSIX `struct iovec`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Iovec {
    /// Start of the buffer.
    pub iov_base: *mut c_void,
    /// Length of the buffer in bytes.
    pub iov_len: usize,
}

/// Returns the total length in bytes of the `len` iovecs starting at `begin`.
///
/// O(len), allocation-free.
///
/// # Safety
/// `begin` points to `len` initialized, properly aligned iovecs that are not written during
/// the call (`begin` may dangle only when `len` is 0, if it is still non-null and aligned).
#[inline]
pub unsafe fn iovec_len_ptr(begin: *const Iovec, len: usize) -> usize {
    let mut ret: usize = 0;
    // SAFETY: the caller guarantees `begin..begin+len` is a valid, aligned, initialized
    // range of `Iovec` that nobody writes while the slice lives.
    let s = unsafe { std::slice::from_raw_parts(begin, len) };
    for e in s {
        ret += e.iov_len;
    }
    ret
}

/// Returns the total length in bytes of the iovecs in `iov`.
///
/// O(n), allocation-free.
#[inline]
pub fn iovec_len(iov: &[Iovec]) -> usize {
    let mut ret: usize = 0;
    for e in iov {
        ret += e.iov_len;
    }
    ret
}

/// Trims a properly aligned vector of iovecs so that it respects the `IOV_MAX` limit,
/// keeping the alignment requirements, and returns the final total length of all iovecs.
/// Implemented next to the reactor's I/O submission code.
pub use crate::engine::core::reactor::internal_iovecs::sanitize_iovecs;
