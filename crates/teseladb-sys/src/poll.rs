//! Waiting for I/O events: epoll (Linux) or kqueue (macOS), level-triggered, with a waker that can
//! be used from other threads.

use std::io;
use std::os::fd::RawFd;
use std::time::Duration;

use crate::os;

/// What matters about a descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interest {
    /// There is data to read (or a connection to accept).
    pub read: bool,
    /// It can be written.
    pub write: bool,
}

impl Interest {
    /// Read only.
    pub const READ: Interest = Interest { read: true, write: false };
    /// Write only.
    pub const WRITE: Interest = Interest { read: false, write: true };
    /// Both.
    pub const BOTH: Interest = Interest { read: true, write: true };
}

/// Event of a registered descriptor. On macOS reading and writing of the same descriptor may
/// arrive as two events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    /// Token it was registered with.
    pub token: u64,
    /// It can be read.
    pub readable: bool,
    /// It can be written.
    pub writable: bool,
    /// The peer closed or the descriptor has an error.
    pub hangup: bool,
}

/// Token reserved for the waker.
pub const WAKE_TOKEN: u64 = u64::MAX;

/// Per-thread event selector.
#[derive(Debug)]
pub struct Poller {
    inner: os::Poller,
}

/// Wakes a [`Poller`] from any thread.
#[derive(Clone, Debug)]
pub struct Waker {
    inner: os::Waker,
}

impl Poller {
    /// New selector with its waker registered.
    pub fn new() -> io::Result<Poller> {
        Ok(Poller { inner: os::Poller::new()? })
    }

    /// Registers `fd` with a token (other than [`WAKE_TOKEN`]).
    pub fn register(&self, fd: RawFd, token: u64, interest: Interest) -> io::Result<()> {
        check_token(token)?;
        self.inner.register(fd, token, interest)
    }

    /// Changes the interest of an already registered descriptor.
    pub fn modify(&self, fd: RawFd, token: u64, interest: Interest) -> io::Result<()> {
        check_token(token)?;
        self.inner.modify(fd, token, interest)
    }

    /// Stops watching `fd` (before closing it).
    pub fn deregister(&self, fd: RawFd) -> io::Result<()> {
        self.inner.deregister(fd)
    }

    /// Waits for events at most `timeout` (`None` = no limit) and stores them in `events`
    /// (cleared first). A wake-up arrives as an event with [`WAKE_TOKEN`]. A signal that
    /// interrupts the wait returns with no events.
    pub fn wait(&self, events: &mut Vec<Event>, timeout: Option<Duration>) -> io::Result<()> {
        events.clear();
        self.inner.wait(events, timeout)
    }

    /// Waker of this selector.
    pub fn waker(&self) -> Waker {
        Waker { inner: self.inner.waker() }
    }
}

impl std::os::fd::AsRawFd for Poller {
    /// The selector's own descriptor: readable while it has events, so another loop (an
    /// io_uring ring, 012 FR-024) can wait for it.
    fn as_raw_fd(&self) -> RawFd {
        self.inner.raw_fd()
    }
}

impl Waker {
    /// Makes the current wait (or the next one) return with a [`WAKE_TOKEN`] event. Several
    /// wake-ups in a row may arrive as one.
    pub fn wake(&self) -> io::Result<()> {
        self.inner.wake()
    }
}

fn check_token(token: u64) -> io::Result<()> {
    if token == WAKE_TOKEN {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "reserved token"));
    }
    Ok(())
}

/// Pins the current thread to CPU `cpu`. Returns `false` where the system does not allow it (macOS).
pub fn pin_to_cpu(cpu: usize) -> io::Result<bool> {
    os::pin_to_cpu(cpu)
}

/// Milliseconds for the wait (rounded up so it never wakes early). Used by epoll; kqueue takes
/// the exact duration.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn timeout_ms(timeout: Option<Duration>) -> i32 {
    match timeout {
        None => -1,
        Some(d) => {
            let ms = d.as_nanos().div_ceil(1_000_000);
            i32::try_from(ms).unwrap_or(i32::MAX)
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::os::fd::AsRawFd;
    use std::time::Instant;

    #[test]
    fn timeouts_round_up() {
        assert_eq!(timeout_ms(None), -1);
        assert_eq!(timeout_ms(Some(Duration::ZERO)), 0);
        assert_eq!(timeout_ms(Some(Duration::from_nanos(1))), 1);
        assert_eq!(timeout_ms(Some(Duration::from_millis(5))), 5);
        assert_eq!(timeout_ms(Some(Duration::from_secs(u64::MAX))), i32::MAX);
    }

    #[test]
    fn wake_token_is_reserved() {
        let p = Poller::new().unwrap();
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(p.register(l.as_raw_fd(), WAKE_TOKEN, Interest::READ).is_err());
        assert!(p.modify(l.as_raw_fd(), WAKE_TOKEN, Interest::READ).is_err());
    }

    #[test]
    fn readiness_wakeups_and_timeouts() {
        let p = Poller::new().unwrap();
        let mut ev = Vec::new();
        // Nothing registered: the timeout expires (a signal may end a wait early, with no events,
        // so the wait is repeated until the time is up).
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(20) {
            p.wait(&mut ev, Some(Duration::from_millis(20) - t0.elapsed())).unwrap();
            assert!(ev.is_empty());
        }
        // A connection to accept.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.set_nonblocking(true).unwrap();
        p.register(l.as_raw_fd(), 1, Interest::READ).unwrap();
        let mut c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        p.wait(&mut ev, Some(Duration::from_secs(5))).unwrap();
        assert!(ev.iter().any(|e| e.token == 1 && e.readable));
        let (mut s, _) = l.accept().unwrap();
        s.set_nonblocking(true).unwrap();
        // Writing is possible right away; reading once the peer writes.
        p.register(s.as_raw_fd(), 2, Interest::WRITE).unwrap();
        p.wait(&mut ev, Some(Duration::from_secs(5))).unwrap();
        assert!(ev.iter().any(|e| e.token == 2 && e.writable && !e.readable));
        p.modify(s.as_raw_fd(), 2, Interest::READ).unwrap();
        p.wait(&mut ev, Some(Duration::from_millis(10))).unwrap();
        assert!(!ev.iter().any(|e| e.token == 2), "{ev:?}");
        c.write_all(b"ping").unwrap();
        p.wait(&mut ev, Some(Duration::from_secs(5))).unwrap();
        assert!(ev.iter().any(|e| e.token == 2 && e.readable));
        let mut buf = [0u8; 4];
        s.read_exact(&mut buf).unwrap();
        // Level-triggered: with no pending data there is no event.
        p.wait(&mut ev, Some(Duration::from_millis(10))).unwrap();
        assert!(!ev.iter().any(|e| e.token == 2));
        // Both at once.
        p.modify(s.as_raw_fd(), 2, Interest::BOTH).unwrap();
        p.wait(&mut ev, Some(Duration::from_secs(5))).unwrap();
        assert!(ev.iter().any(|e| e.token == 2 && e.writable));
        // The peer closes.
        drop(c);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            p.wait(&mut ev, Some(Duration::from_millis(50))).unwrap();
            if ev.iter().any(|e| e.token == 2 && (e.hangup || e.readable))
                || Instant::now() > deadline
            {
                break;
            }
        }
        assert!(ev.iter().any(|e| e.token == 2 && (e.hangup || e.readable)));
        p.deregister(s.as_raw_fd()).unwrap();
        p.deregister(l.as_raw_fd()).unwrap();
        // What is not registered cannot be removed or changed.
        assert!(p.deregister(s.as_raw_fd()).is_err());
        assert!(p.register(-1, 3, Interest::READ).is_err());
    }

    #[test]
    fn waker_from_another_thread() {
        let p = Poller::new().unwrap();
        let w = p.waker();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            w.wake().unwrap();
            w.wake().unwrap();
        });
        let mut ev = Vec::new();
        // A signal (another test raises one) may end a wait early with no events.
        let woken = |p: &Poller, ev: &mut Vec<Event>| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                p.wait(ev, Some(Duration::from_secs(5))).unwrap();
                if ev.iter().any(|e| e.token == WAKE_TOKEN) {
                    return true;
                }
            }
            false
        };
        assert!(woken(&p, &mut ev));
        h.join().unwrap();
        // Accumulated wake-ups are consumed: the next wait times out.
        p.wait(&mut ev, Some(Duration::from_millis(5))).unwrap();
        let again = ev.iter().filter(|e| e.token == WAKE_TOKEN).count();
        p.wait(&mut ev, Some(Duration::from_millis(5))).unwrap();
        assert!(again <= 1 && ev.is_empty(), "{ev:?}");
        // A wake-up before waiting is not lost.
        p.waker().wake().unwrap();
        assert!(woken(&p, &mut ev));
    }

    #[test]
    fn pinning_does_not_fail() {
        // On Linux it pins the thread to CPU 0; on macOS it is not available.
        let r = std::thread::spawn(|| pin_to_cpu(0)).join().unwrap().unwrap();
        assert_eq!(r, cfg!(target_os = "linux"));
        assert!(
            std::thread::spawn(|| pin_to_cpu(1 << 20)).join().unwrap().is_err()
                || cfg!(target_os = "macos")
        );
    }
}
