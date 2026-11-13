//! Bounded channels with one sender and one receiver between threads (012 FR-005): a ring of
//! `teseladb-sys` with no lock, a full ring that gives the message back to its sender, and a
//! wake-up of the receiver's poller on the first message after each drain.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use teseladb_sys::ring::{Consumer, Producer, ring};

use crate::mailbox::Notify;

/// Why a message did not go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError<T> {
    /// The ring is full: the receiver is behind and the sender has to push back.
    Full(T),
    /// The receiver is gone.
    Closed(T),
}

/// On a cache line of its own (012 FR-031), apart from what the two threads write elsewhere.
#[repr(align(64))]
struct Wake {
    /// Set by the send that woke the receiver; cleared by the receiver as it drains.
    notified: AtomicBool,
    notify: Arc<dyn Notify>,
}

/// The sending side, owned by one thread.
pub struct SpscSender<T> {
    tx: Producer<T>,
    wake: Arc<Wake>,
}

/// The receiving side, owned by the receiving core's thread.
pub struct SpscReceiver<T> {
    rx: Consumer<T>,
    wake: Arc<Wake>,
}

/// A channel holding at least `capacity` messages whose receiver is woken with `notify`.
pub fn channel<T: Send>(
    capacity: usize,
    notify: Arc<dyn Notify>,
) -> (SpscSender<T>, SpscReceiver<T>) {
    let (tx, rx) = ring(capacity);
    let wake = Arc::new(Wake { notified: AtomicBool::new(false), notify });
    (SpscSender { tx, wake: wake.clone() }, SpscReceiver { rx, wake })
}

impl<T> SpscSender<T> {
    /// Queues `msg` and wakes the receiver if nothing woke it since its last drain.
    pub fn send(&mut self, msg: T) -> Result<(), SendError<T>> {
        if self.tx.is_abandoned() {
            return Err(SendError::Closed(msg));
        }
        self.tx.push(msg).map_err(SendError::Full)?;
        // After the push: a receiver that cleared the flag before it either drains this message
        // or is woken by this swap; one that clears it later synchronizes with it and sees it.
        if !self.wake.notified.swap(true, Ordering::AcqRel) {
            self.wake.notify.notify();
        }
        Ok(())
    }

    /// Whether the receiver is gone.
    pub fn is_closed(&self) -> bool {
        self.tx.is_abandoned()
    }
}

impl<T> SpscReceiver<T> {
    /// Moves every queued message to the end of `out`, in the order they were sent. After it
    /// starts, the next send wakes the receiver again.
    pub fn drain_into(&mut self, out: &mut Vec<T>) {
        self.wake.notified.swap(false, Ordering::AcqRel);
        while let Some(m) = self.rx.pop() {
            out.push(m);
        }
    }

    /// Whether no message is queued (a snapshot).
    pub fn is_empty(&self) -> bool {
        self.rx.is_empty()
    }

    /// Whether the sender is gone: what is queued is all there will be.
    pub fn is_closed(&self) -> bool {
        self.rx.is_abandoned()
    }
}

impl<T> fmt::Debug for SpscSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpscSender").field("closed", &self.is_closed()).finish()
    }
}

impl<T> fmt::Debug for SpscReceiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpscReceiver").field("empty", &self.is_empty()).finish()
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod alignment_tests {
    #[test]
    fn the_wake_flag_has_its_own_cache_line() {
        assert_eq!(std::mem::align_of::<super::Wake>(), 64);
    }
}
