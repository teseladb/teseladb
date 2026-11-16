//! Process-wide policy for operations on an invalid file descriptor.
//!
//! When a system call made by the engine fails with `EBADF` or `ENOTSOCK`, the target descriptor was
//! not valid, which usually means a use-after-close bug in the application. By default the failure is
//! returned to the caller as an ordinary system error; with the policy enabled the process aborts at
//! the point of failure, which leaves a core dump with the faulty call on the stack. The setting is a
//! single atomic flag shared by all shards.

/// Sets whether an operation that fails with `EBADF` or `ENOTSOCK` aborts the process (`true`) or
/// returns a system error to its caller (`false`, the default).
///
/// The flag is global to the process and takes effect immediately on every shard.
pub use crate::engine::core::reactor::set_abort_on_ebadf;

/// Returns the current value of the policy set with [`set_abort_on_ebadf`]: `true` if an invalid
/// file descriptor aborts the process.
pub use crate::engine::core::reactor::is_abort_on_ebadf_enabled;
