//! POSIX signal handling for the per-core reactor.
//!
//! Signals are delivered to the process, but the reactor turns each one into a task on the
//! shard that registered it: the kernel-level handler only records the signal, and the
//! reactor runs the registered [`NoncopyableFunction`] from its poll loop, so handlers run as
//! ordinary reactor tasks and may use any shard-local state.

use crate::engine::core::future::ExceptionPtr;
use crate::engine::core::reactor::engine;
use crate::engine::util::noncopyable_function::NoncopyableFunction;

/// Installs `handler` for signal `signo` on the calling shard's reactor.
///
/// * `signo` - the signal number (for example `SIGHUP`).
/// * `handler` - called from the reactor poll loop each time the signal arrives.
/// * `once` - when `true` the handler is removed after its first invocation.
///
/// Must be called on a reactor thread.
///
/// # Errors
///
/// Returns the error raised while installing the kernel-level handler (for example an
/// invalid signal number).
pub fn handle_signal(
    signo: i32,
    handler: NoncopyableFunction<fn()>,
    once: bool,
) -> Result<(), ExceptionPtr> {
    // SAFETY: on this shard's reactor thread; the signal table is reached for this call only
    // (registering a handler runs none).
    let signals = unsafe { &mut *std::ptr::addr_of_mut!((*engine())._signals) };
    if once {
        signals.handle_signal_once(signo, handler)
    } else {
        signals.handle_signal(signo, handler)
    }
}
