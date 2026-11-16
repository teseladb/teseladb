//! Safe forms of the engine's DMA reads and writes of a file (023 FR-010), for the disk executor of
//! `teseladb-io`, which forbids unsafe code.
//!
//! The engine's raw operations are `unsafe` for two reasons: the buffer must stay alive and
//! untouched until the kernel is done with it, and an optional I/O intent pointer must outlive the
//! request. These functions pass no intent and move the buffer, and a clone of the file, into the
//! continuation of the request, so both live until it resolves whatever the caller does with the
//! returned future.

use std::ptr;

use teseladb::engine::core::file::File;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;

/// Reads `len` bytes at `offset` of `file` into a new buffer: offset and length need no alignment
/// (the engine reads the aligned span that covers them and trims it). The buffer is shorter than
/// `len` at the end of the file. It fails with the engine's end-of-file error when `offset` is past
/// the end, and with the system error of a failed read.
pub fn read(file: &mut File, offset: u64, len: usize) -> Future<TemporaryBuffer<u8>> {
    let keep = file.clone();
    // SAFETY: a null intent is allowed and nothing has to outlive the request; the engine
    // allocates the destination buffer itself and hands it over with the result.
    let read = unsafe { file.dma_read_bulk::<u8>(offset, len, ptr::null_mut()) };
    read.then(move |buf: TemporaryBuffer<u8>| {
        drop(keep);
        Ok::<_, ExceptionPtr>(buf)
    })
}

/// The alignment of the buffers of [`aligned_buffer`], enough for any device's direct I/O.
pub const ALIGN: usize = 4096;

/// A buffer of `len` bytes whose start is aligned to [`ALIGN`], holding each of `parts` at its
/// offset and zeros elsewhere: the source of a direct write.
///
/// # Panics
///
/// When a part does not fit in `len` bytes.
#[must_use]
pub fn aligned_buffer(len: usize, parts: &[(usize, &[u8])]) -> TemporaryBuffer<u8> {
    for (at, bytes) in parts {
        assert!(
            at.checked_add(bytes.len()).is_some_and(|end| end <= len),
            "a part past the buffer"
        );
    }
    let mut buf = TemporaryBuffer::<u8>::aligned(ALIGN, len);
    let to = buf.get_write();
    // Parts may overlap (a write over the blocks read around it): only one part over the whole
    // buffer leaves nothing to zero.
    let whole = matches!(parts, [(0, b)] if b.len() == len);
    if !whole {
        // SAFETY: `to` points at `len` writable bytes of the new buffer.
        unsafe { ptr::write_bytes(to, 0, len) };
    }
    for (at, bytes) in parts {
        // SAFETY: the part fits in the buffer (checked above) and does not overlap it, as the
        // buffer is new.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), to.add(*at), bytes.len()) };
    }
    buf
}

/// `fcntl(F_GETFL)`.
const F_GETFL: i32 = 3;
/// `O_DIRECT` (x86_64 and aarch64 differ).
#[cfg(target_arch = "aarch64")]
const O_DIRECT: i32 = 0o200000;
#[cfg(not(target_arch = "aarch64"))]
const O_DIRECT: i32 = 0o40000;

/// Whether the engine opened `file` with `O_DIRECT`: it does unless the reactor runs with the
/// kernel's page cache, or the file system does not take it (tmpfs). A direct file only takes
/// aligned writes.
pub fn is_direct(file: &mut File) -> Future<bool> {
    // SAFETY: `F_GETFL` takes no argument and only reads the descriptor's status flags.
    let flags = unsafe { file.fcntl_short(F_GETFL, 0) };
    flags.then(|f: i32| Ok::<_, ExceptionPtr>(f & O_DIRECT != 0))
}

/// Writes the whole of `buf` at `offset` of `file` with one DMA request and resolves with the
/// bytes written, fewer on a short write. With `O_DIRECT`, `offset`, the length and the buffer's
/// address must be aligned to the file's DMA alignment (4 KiB is always enough); a misaligned
/// request fails with `EINVAL`.
pub fn write(file: &mut File, offset: u64, buf: TemporaryBuffer<u8>) -> Future<usize> {
    let keep = file.clone();
    let (at, len) = (buf.get(), buf.size());
    // SAFETY: `buf` moves into the continuation below, so its memory stays allocated and nothing
    // writes to it until the request has resolved; the intent is null.
    let wrote = unsafe { file.dma_write::<u8>(offset, at, len, ptr::null_mut()) };
    wrote.then(move |n: usize| {
        drop((keep, buf));
        Ok::<_, ExceptionPtr>(n)
    })
}
