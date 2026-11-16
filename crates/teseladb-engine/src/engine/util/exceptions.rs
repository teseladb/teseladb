//! Constructors for filesystem errors.
//!
//! File operations report failures as [`FilesystemError`], which carries the action, the
//! path(s) involved and the `errno` value. These helpers build it from a failed system call.

use std::path::PathBuf;

/// The error type for failed filesystem operations (action, paths and `errno`).
pub use crate::engine::core::future::FilesystemError;

/// Builds a [`FilesystemError`] for a system call with a single file operand.
///
/// - `what`: describes the action that failed;
/// - `path`: the file that hit the error;
/// - `error`: the system error number (see `errno(3)`).
pub fn make_filesystem_error(what: &str, path: PathBuf, error: i32) -> FilesystemError {
    FilesystemError::new(what, &path, None, error)
}

/// Builds a [`FilesystemError`] for a system call with two file operands.
///
/// - `what`: describes the action that failed;
/// - `path1`, `path2`: the files involved;
/// - `error`: the system error number (see `errno(3)`).
///
/// Note: both operands of the returned error are `path1`; `path2` is accepted and ignored.
/// Callers that need the second path in the message must build the error themselves.
pub fn make_filesystem_error2(
    what: &str,
    path1: PathBuf,
    path2: PathBuf,
    error: i32,
) -> FilesystemError {
    // Both operands are deliberately `path1` (see the doc comment); `path2` is unused.
    let _ = path2;
    FilesystemError::new(what, &path1, Some(&path1), error)
}
