//! Event selector for a core's loop over non-blocking `std::net` sockets, with the same behaviour
//! on Linux and macOS: one event per token and wait, level-triggered.

use std::io;
use std::os::fd::AsRawFd;
use std::time::Duration;

pub use teseladb_sys::poll::{Event, Interest, WAKE_TOKEN, Waker};

/// Per-thread selector.
#[derive(Debug)]
pub struct Poller {
    sys: teseladb_sys::poll::Poller,
    raw: Vec<Event>,
    events: Vec<Event>,
}

impl Poller {
    /// New selector.
    pub fn new() -> io::Result<Poller> {
        Ok(Poller { sys: teseladb_sys::poll::Poller::new()?, raw: Vec::new(), events: Vec::new() })
    }

    /// Watches `source` (which must be in non-blocking mode) with a token.
    pub fn register(
        &self,
        source: &impl AsRawFd,
        token: u64,
        interest: Interest,
    ) -> io::Result<()> {
        self.sys.register(source.as_raw_fd(), token, interest)
    }

    /// Changes the interest (e.g. add writing when there is data pending to send).
    pub fn reregister(
        &self,
        source: &impl AsRawFd,
        token: u64,
        interest: Interest,
    ) -> io::Result<()> {
        self.sys.modify(source.as_raw_fd(), token, interest)
    }

    /// Stops watching `source` (before closing it).
    pub fn deregister(&self, source: &impl AsRawFd) -> io::Result<()> {
        self.sys.deregister(source.as_raw_fd())
    }

    /// Waits at most `timeout`. Returns one event per token, in arrival order.
    pub fn poll(&mut self, timeout: Option<Duration>) -> io::Result<&[Event]> {
        self.sys.wait(&mut self.raw, timeout)?;
        self.events.clear();
        for e in &self.raw {
            match self.events.iter_mut().find(|x| x.token == e.token) {
                Some(x) => {
                    x.readable |= e.readable;
                    x.writable |= e.writable;
                    x.hangup |= e.hangup;
                }
                None => self.events.push(*e),
            }
        }
        Ok(&self.events)
    }

    /// Waker for other threads.
    pub fn waker(&self) -> Waker {
        self.sys.waker()
    }
}
