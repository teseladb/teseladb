//! Stop signals through the engine's reactor (023 FR-031): `SIGINT` and `SIGTERM` are handled by
//! shard 0's reactor, which records the stop; the node's main thread sees it with
//! [`stop_requested`] and stops the node in order (no new connections, the requests in flight
//! answered, each core's writes on disk within 10 s, then the shards).
//!
//! The process blocks both signals in its main thread before it starts any other
//! (`teseladb_sys::process::block_stop_signals`), so that every thread inherits the mask and the
//! signals stay pending until shard 0 unblocks them when it registers its handlers: a signal sent
//! while the node starts is handled once the engine runs, and none lands on a thread the engine
//! does not know, where it would be lost.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use teseladb::engine::core::signal::handle_signal;
use teseladb::engine::util::noncopyable_function::NoncopyableFunction;

/// `SIGINT` (Ctrl-C).
pub const SIGINT: i32 = 2;
/// `SIGTERM`.
pub const SIGTERM: i32 = 15;

/// Whether a stop signal arrived.
static STOP: AtomicBool = AtomicBool::new(false);

/// Whether a stop signal arrived since the process started.
pub fn stop_requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// Registers the stop signals on the calling shard's reactor (shard 0 of the process's engine).
///
/// # Errors
///
/// The engine's error installing a handler.
pub(crate) fn install_here() -> io::Result<()> {
    for signo in [SIGINT, SIGTERM] {
        let record = NoncopyableFunction::<fn()>::new(|| STOP.store(true, Ordering::SeqCst));
        handle_signal(signo, record, false)
            .map_err(|e| io::Error::other(format!("signal {signo}: {e:?}")))?;
    }
    Ok(())
}
