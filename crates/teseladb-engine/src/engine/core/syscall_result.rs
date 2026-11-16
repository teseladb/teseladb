//! Results of raw system calls, paired with the `errno` they left.
//!
//! The reactor runs some system calls on its syscall thread and others inline; either way the
//! return value and `errno` are captured together right after the call (see [`wrap_syscall`])
//! so the error survives being moved to another thread or past other calls. The helpers turn a
//! failed result (`-1`) into the crate's error types.
use std::path::Path;

use crate::engine::core::future::{ExceptionPtr, FilesystemError, SystemError};
use crate::engine::core::reactor_backend::errno;

/// The integer types a system call can return; widened to `i64` for the `-1` check.
pub trait SyscallValue: Copy {
    /// The value as an `i64`.
    fn as_long(self) -> i64;
}

impl SyscallValue for i32 {
    #[inline]
    fn as_long(self) -> i64 {
        self as i64
    }
}

impl SyscallValue for i64 {
    #[inline]
    fn as_long(self) -> i64 {
        self
    }
}

/// The return value of a system call and the `errno` captured right after it.
#[derive(Clone, Copy, Debug)]
pub struct SyscallResult<T> {
    /// The value the system call returned; `-1` means failure.
    pub result: T,
    /// The `errno` value captured after the call (meaningful only when `result == -1`).
    pub error: i32,
}

impl<T: SyscallValue> SyscallResult<T> {
    /// Pairs a system call result with its `errno`.
    #[inline]
    pub fn new(result: T, error: i32) -> Self {
        SyscallResult { result, error }
    }

    /// Checks the result.
    ///
    /// # Errors
    ///
    /// Returns a [`SystemError`] built from [`ec`](Self::ec) when the result is `-1`.
    #[inline]
    pub fn throw_if_error(&self) -> Result<(), ExceptionPtr> {
        if self.result.as_long() == -1 {
            return Err(ExceptionPtr::from(SystemError::from_errno(self.ec())));
        }
        Ok(())
    }

    /// Builds a [`FilesystemError`] for `path` with message `reason` and this result's
    /// `errno`, for the caller to return.
    pub fn throw_fs_exception(&self, reason: &str, path: &Path) -> ExceptionPtr {
        ExceptionPtr::from(FilesystemError::new(reason, path, None, self.ec()))
    }

    /// Builds a [`FilesystemError`] that names two paths (for example a rename's source and
    /// target), with message `reason` and this result's `errno`.
    pub fn throw_fs_exception2(&self, reason: &str, path1: &Path, path2: &Path) -> ExceptionPtr {
        ExceptionPtr::from(FilesystemError::new(reason, path1, Some(path2), self.ec()))
    }

    /// Checks the result, naming `path` in the error.
    ///
    /// # Errors
    ///
    /// Returns a [`FilesystemError`] for `path` when the result is `-1`.
    #[inline]
    pub fn throw_fs_exception_if_error(
        &self,
        reason: &str,
        path: &str,
    ) -> Result<(), ExceptionPtr> {
        if self.result.as_long() == -1 {
            return Err(self.throw_fs_exception(reason, Path::new(path)));
        }
        Ok(())
    }

    /// Checks the result, naming two paths in the error.
    ///
    /// # Errors
    ///
    /// Returns a [`FilesystemError`] for `path1` and `path2` when the result is `-1`.
    #[inline]
    pub fn throw_fs_exception_if_error2(
        &self,
        reason: &str,
        path1: &str,
        path2: &str,
    ) -> Result<(), ExceptionPtr> {
        if self.result.as_long() == -1 {
            return Err(self.throw_fs_exception2(reason, Path::new(path1), Path::new(path2)));
        }
        Ok(())
    }

    /// The captured `errno` value.
    #[inline]
    pub fn ec(&self) -> i32 {
        self.error
    }
}

/// A system call result that also carries an output parameter the call filled in, plus the
/// `errno` captured right after it.
#[derive(Clone, Copy, Debug)]
pub struct SyscallResultExtra<Extra> {
    /// The value the system call returned; `-1` means failure.
    pub result: i32,
    /// The `errno` value captured after the call.
    pub error: i32,
    /// The output parameter returned by the call.
    pub extra: Extra,
}

impl<Extra> SyscallResultExtra<Extra> {
    /// Pairs a result, its `errno` and the call's output parameter.
    #[inline]
    pub fn new(result: i32, error: i32, extra: Extra) -> Self {
        SyscallResultExtra { result, error, extra }
    }

    #[inline]
    fn base(&self) -> SyscallResult<i32> {
        SyscallResult { result: self.result, error: self.error }
    }

    /// Checks the result.
    ///
    /// # Errors
    ///
    /// Returns a [`SystemError`] when the result is `-1`.
    #[inline]
    pub fn throw_if_error(&self) -> Result<(), ExceptionPtr> {
        self.base().throw_if_error()
    }

    /// Builds a [`FilesystemError`] for `path`; see [`SyscallResult::throw_fs_exception`].
    pub fn throw_fs_exception(&self, reason: &str, path: &Path) -> ExceptionPtr {
        self.base().throw_fs_exception(reason, path)
    }

    /// Checks the result, naming `path` in the error.
    ///
    /// # Errors
    ///
    /// Returns a [`FilesystemError`] for `path` when the result is `-1`.
    #[inline]
    pub fn throw_fs_exception_if_error(
        &self,
        reason: &str,
        path: &str,
    ) -> Result<(), ExceptionPtr> {
        self.base().throw_fs_exception_if_error(reason, path)
    }

    /// The captured `errno` value.
    #[inline]
    pub fn ec(&self) -> i32 {
        self.error
    }
}

/// Captures `errno` right after a system call that returned `result`.
///
/// Call it immediately after the system call, before anything else can change `errno`.
#[inline]
pub fn wrap_syscall<T: SyscallValue>(result: T) -> SyscallResult<T> {
    SyscallResult { result, error: errno() }
}

/// Captures `errno` right after a system call that returned `result` and filled the output
/// parameter `extra`.
///
/// `result` is the call's `int` return value; callers that hold it in a wider integer pass it
/// as is and it is truncated to `i32`.
#[inline]
pub fn wrap_syscall_extra<Extra>(result: i64, extra: Extra) -> SyscallResultExtra<Extra> {
    SyscallResultExtra { result: result as i32, error: errno(), extra }
}
