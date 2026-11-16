//! Two-pass relocation of objects between raw memory slots.
//!
//! Containers that grow by relocating their elements (for example the circular buffers used by
//! the reactor's queues) move each element with [`transfer_pass1`] and then call
//! [`transfer_pass2`] on every pair once all moves have succeeded. A Rust move is a bitwise copy
//! that cannot fail, so the first pass does the whole job (it reads the source and writes the
//! destination, leaving the source logically uninitialized) and the second pass is a no-op. The
//! two-pass shape is kept so that callers have a single relocation protocol.

/// Moves the object at `from` into the uninitialized slot `to`; `from` is left uninitialized.
///
/// The caller must not drop or read the value at `from` afterwards; ownership now lives at `to`.
///
/// # Safety
/// `from` must hold an initialized `T`, `to` must be valid for writes and properly aligned, and
/// the two regions must not overlap.
#[inline(always)]
pub unsafe fn transfer_pass1<T>(from: *mut T, to: *mut T) {
    // SAFETY: the caller guarantees `from` is initialized, `to` is writable and aligned, and the
    // two do not overlap, which is all `read` and `write` require.
    unsafe { to.write(from.read()) };
}

/// Completes a relocation started by [`transfer_pass1`]; nothing is left to do after a move.
///
/// # Safety
/// No requirement; it is `unsafe` only to pair with [`transfer_pass1`] at call sites.
#[inline(always)]
pub unsafe fn transfer_pass2<T>(_from: *mut T, _to: *mut T) {}
