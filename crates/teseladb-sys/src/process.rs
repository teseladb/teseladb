//! Signals between processes (023 FR-031, FR-032): blocking the stop signals in the calling
//! thread, so that the threads it starts inherit the mask and only the engine's shard 0, which
//! unblocks them when it registers its handlers, receives them; and sending a stop signal to
//! another process (the parent of `teseladb dev` to its nodes, the tests to theirs). Handling
//! them is the engine's.

use std::io;

/// `SIGINT`.
const SIGINT: i32 = 2;
/// `SIGTERM`.
const SIGTERM: i32 = 15;
/// `SIG_BLOCK` of `pthread_sigmask`.
const SIG_BLOCK: i32 = 0;
/// `SIG_UNBLOCK` of `pthread_sigmask`.
const SIG_UNBLOCK: i32 = 1;

/// `sigset_t` of glibc and musl: 1024 bits.
#[repr(C)]
struct SigSet([u64; 16]);

unsafe extern "C" {
    fn sigemptyset(set: *mut SigSet) -> i32;
    fn sigaddset(set: *mut SigSet, signo: i32) -> i32;
    fn sigismember(set: *const SigSet, signo: i32) -> i32;
    fn pthread_sigmask(how: i32, set: *const SigSet, old: *mut SigSet) -> i32;
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
    /// Its number.
    pub fn number(self) -> i32 {
        match self {
            Stop::Interrupt => SIGINT,
            Stop::Terminate => SIGTERM,
        }
    }
}

/// A set with `SIGINT` and `SIGTERM`.
fn stop_set() -> SigSet {
    let mut set = SigSet([0; 16]);
    // SAFETY: `set` is a `sigset_t` of this system's size, valid during the calls; both numbers
    // are valid signals, so the calls cannot fail.
    unsafe {
        sigemptyset(&mut set);
        sigaddset(&mut set, SIGINT);
        sigaddset(&mut set, SIGTERM);
    }
    set
}

/// Changes the calling thread's mask for the stop signals.
fn mask(how: i32) -> io::Result<()> {
    let set = stop_set();
    // SAFETY: `set` is a valid `sigset_t` during the call; the old mask is not asked for.
    let rc = unsafe { pthread_sigmask(how, &set, std::ptr::null_mut()) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(())
}

/// Blocks `SIGINT` and `SIGTERM` in the calling thread, and in every thread it starts from now
/// on: they stay pending until a thread that unblocks them (the engine's shard 0) takes them.
///
/// # Errors
///
/// The error of `pthread_sigmask`.
pub fn block_stop_signals() -> io::Result<()> {
    mask(SIG_BLOCK)
}

/// Unblocks `SIGINT` and `SIGTERM` in the calling thread (a program that will not run an
/// engine, and the tests).
///
/// # Errors
///
/// The error of `pthread_sigmask`.
pub fn unblock_stop_signals() -> io::Result<()> {
    mask(SIG_UNBLOCK)
}

/// Whether the calling thread blocks `sig`.
pub fn blocks(sig: Stop) -> bool {
    let mut now = SigSet([0; 16]);
    // SAFETY: a null set leaves the mask as it is and `now` receives it; both are valid during
    // the calls.
    unsafe {
        pthread_sigmask(SIG_BLOCK, std::ptr::null(), &mut now);
        sigismember(&now, sig.number()) == 1
    }
}

/// Sends a stop signal to process `pid`.
///
/// # Errors
///
/// A `pid` that is not one, or the error of `kill` (no such process, not allowed).
pub fn send_stop(pid: u32, sig: Stop) -> io::Result<()> {
    let pid = i32::try_from(pid).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    if pid <= 0 {
        // 0 and negative numbers name process groups, not a process.
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    // SAFETY: `kill` takes two integers and keeps no pointers.
    if unsafe { kill(pid, sig.number()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_blocked_thread_starts_threads_that_block_too() {
        let inherited = std::thread::spawn(|| {
            block_stop_signals().unwrap();
            assert!(blocks(Stop::Interrupt) && blocks(Stop::Terminate));
            let child = std::thread::spawn(|| blocks(Stop::Terminate)).join().unwrap();
            unblock_stop_signals().unwrap();
            (child, blocks(Stop::Terminate))
        })
        .join()
        .unwrap();
        assert_eq!(inherited, (true, false));
    }

    #[test]
    fn a_stop_signal_ends_a_process_that_does_not_handle_it() {
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        send_stop(child.id(), Stop::Terminate).unwrap();
        let status = child.wait().unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(SIGTERM));
    }

    #[test]
    fn stop_signals_go_only_to_processes() {
        assert_eq!(send_stop(0, Stop::Interrupt).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        let too_big = u32::MAX;
        assert_eq!(
            send_stop(too_big, Stop::Terminate).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        // A process that does not exist (the highest pid Linux gives is below 2^22).
        assert!(send_stop(1 << 22, Stop::Terminate).is_err());
        assert_eq!(Stop::Interrupt.number(), 2);
        assert_eq!(Stop::Terminate.number(), 15);
    }
}
