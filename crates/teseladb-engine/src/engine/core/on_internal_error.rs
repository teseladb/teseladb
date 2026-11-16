//! Reporting of internal errors: broken invariants detected at run time.
//!
//! Each function logs the message together with the current backtrace. Whether the process then
//! aborts or the error is handed back as an [`ExceptionPtr`] for the caller to propagate is a
//! process-wide policy set with [`set_abort_on_internal_error`] (default: return the error). Aborting
//! is useful in testing, where the core dump shows the state at the point of failure.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::engine::core::future::{ExceptionPtr, RuntimeError};
use crate::engine::util::backtrace::{current_backtrace, make_backtraced_exception_ptr};
use crate::engine::util::log::Logger;

unsafe extern "C" {
    fn abort() -> !;
}

static ABORT_ON_INTERNAL_ERROR: AtomicBool = AtomicBool::new(false);

/// Sets whether [`on_internal_error`] and its variants abort the process (`true`) or return the
/// error to the caller (`false`, the default).
///
/// Returns the previous value of the setting. The flag is global to the process.
pub fn set_abort_on_internal_error(do_abort: bool) -> bool {
    ABORT_ON_INTERNAL_ERROR.swap(do_abort, Ordering::SeqCst)
}

fn log_error_and_backtrace<M: std::fmt::Display + ?Sized>(logger: &Logger, msg: &M) {
    crate::log_error!(logger, "{}, at: {}", msg, current_backtrace());
}

fn do_abort() -> ! {
    // SAFETY: abort has no preconditions.
    unsafe { abort() }
}

/// Reports an internal error described by `msg`.
///
/// Logs `msg` and the current backtrace at error level on `logger`. If aborting is enabled (see
/// [`set_abort_on_internal_error`]) the process then aborts; otherwise a runtime error carrying `msg`
/// and the backtrace is returned for the caller to propagate.
#[cold]
pub fn on_internal_error(logger: &Logger, msg: &str) -> ExceptionPtr {
    log_error_and_backtrace(logger, msg);
    if ABORT_ON_INTERNAL_ERROR.load(Ordering::SeqCst) {
        do_abort();
    }
    make_backtraced_exception_ptr(RuntimeError::new(msg.to_string()))
}

/// Reports an internal error carried by the error `ex`.
///
/// Logs `ex` and the current backtrace on `logger`, then aborts if aborting is enabled, or returns
/// `ex` unchanged. No backtrace is attached to `ex`; a caller that wants one must attach it before
/// the call.
#[cold]
pub fn on_internal_error_ex(logger: &Logger, ex: ExceptionPtr) -> ExceptionPtr {
    log_error_and_backtrace(logger, &ex);
    if ABORT_ON_INTERNAL_ERROR.load(Ordering::SeqCst) {
        do_abort();
    }
    ex
}

/// Reports an internal error where no error can be returned, such as in `Drop`.
///
/// Logs `msg` and the current backtrace on `logger`, and aborts if aborting is enabled; otherwise
/// it returns normally.
#[cold]
pub fn on_internal_error_noexcept(logger: &Logger, msg: &str) {
    log_error_and_backtrace(logger, msg);
    if ABORT_ON_INTERNAL_ERROR.load(Ordering::SeqCst) {
        do_abort();
    }
}

/// Reports an internal error and aborts the process, whatever the abort setting.
///
/// Logs `msg` and the current backtrace on `logger` first. Use it in place of an assertion when the
/// failure must be logged before the process ends.
#[cold]
pub fn on_fatal_internal_error(logger: &Logger, msg: &str) -> ! {
    log_error_and_backtrace(logger, msg);
    do_abort()
}
