//! Stop signals (`SIGINT`, `SIGTERM`) turned into a flag the node polls: the handler only stores
//! an atomic, which is async-signal-safe.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

/// Same numbers on Linux and macOS.
const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;
/// `SIG_ERR` (`(void (*)(int)) -1`).
const SIG_ERR: usize = usize::MAX;

static STOP: AtomicBool = AtomicBool::new(false);

unsafe extern "C" {
    fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
    fn raise(sig: i32) -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
}

/// A stop signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// `SIGINT` (Ctrl-C).
    Interrupt,
    /// `SIGTERM`.
    Terminate,
}

impl Stop {
    fn number(self) -> i32 {
        match self {
            Stop::Interrupt => SIGINT,
            Stop::Terminate => SIGTERM,
        }
    }
}

/// Sends a stop signal to process `pid` (tests and tools that stop a node cleanly).
pub fn send_stop(pid: u32, sig: Stop) -> io::Result<()> {
    let pid = i32::try_from(pid).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `kill` takes two integers and keeps no pointers.
    if unsafe { kill(pid, sig.number()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

extern "C" fn on_stop(_sig: i32) {
    STOP.store(true, Ordering::SeqCst);
}

/// Makes `sig` set the stop flag.
fn install(sig: i32) -> io::Result<()> {
    // SAFETY: `on_stop` is a valid `extern "C" fn(i32)` for the whole program and only does an
    // atomic store.
    if unsafe { signal(sig, on_stop) } == SIG_ERR {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Makes `SIGINT` and `SIGTERM` set the stop flag instead of killing the process.
pub fn install_stop_handler() -> io::Result<()> {
    [SIGINT, SIGTERM].into_iter().try_for_each(install)
}

/// Whether a stop signal arrived.
pub fn stop_requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

fn raise_signal(sig: i32) -> io::Result<()> {
    // SAFETY: `raise` takes a signal number and keeps no pointers.
    if unsafe { raise(sig) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Sends `SIGTERM` to the current process (tests of the clean stop).
pub fn raise_stop() -> io::Result<()> {
    raise_signal(SIGTERM)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// A number no system uses as a signal.
    const NO_SIGNAL: i32 = 1000;
    /// `SIGKILL` cannot be caught.
    const SIGKILL: i32 = 9;

    #[test]
    fn invalid_signals_are_errors() {
        assert!(install(SIGKILL).is_err());
        assert!(install(NO_SIGNAL).is_err());
        assert!(raise_signal(NO_SIGNAL).is_err());
    }
}
