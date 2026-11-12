//! Mailboxes of a core (012 FR-005): any thread sends, one core receives, without a lock. Each
//! sender owns a bounded ring of one producer and one consumer (`teseladb_sys::ring`), made when the
//! sender is made or cloned and handed to the receiver through a lock-free list
//! (`teseladb_sys::handoff`): every pair of a sending thread and a core has its own ring. A full
//! ring gives the message back to its sender (push back) and holds up no other sender. The
//! receiving core sleeps in its poller; the first message after a drain wakes it once, further
//! messages before the next drain do not wake it again.
//!
//! A sender is `Send` but not `Sync`: a thread that needs one clones its own, so no ring ever has
//! two producers. Code shared between threads holds a [`MailboxSenders`], which makes them.

use std::cell::RefCell;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use teseladb_sys::handoff::Handoff;
use teseladb_sys::ring::{Consumer, Producer, ring};

use crate::poller::Waker;
pub use crate::spsc::SendError;

/// Something that makes the receiving core leave its wait.
pub trait Notify: Send + Sync {
    /// Wakes the receiver. Called at most once between two drains.
    fn notify(&self);
}

impl Notify for Waker {
    fn notify(&self) {
        // The only failure of a wake-up is a saturated counter, which already means "awake".
        let _ = self.wake();
    }
}

/// Messages each sender can have queued before it gets them back (`SendError::Full`).
pub const CAPACITY: usize = 256;

/// A message with the instant it was queued: the receiver counts how long it waited (012
/// FR-015).
type Timed<T> = (Instant, T);

struct Shared<T> {
    /// Rings of the senders made since the receiver last looked.
    new_rings: Handoff<Consumer<Timed<T>>>,
    capacity: usize,
    /// Set by the sender that notified; cleared by the receiver before draining.
    notified: AtomicBool,
    closed: AtomicBool,
    notify: Arc<dyn Notify>,
}

impl<T: Send> Shared<T> {
    /// A sender with a ring of its own, handed to the receiver before anything is sent on it.
    fn sender(self: &Arc<Self>) -> MailboxSender<T> {
        let (tx, rx) = ring(self.capacity);
        self.new_rings.push(rx);
        MailboxSender { tx: RefCell::new(tx), shared: self.clone() }
    }
}

/// Sending half, owned by one thread; [`Clone`] gives another sender with a ring of its own.
pub struct MailboxSender<T> {
    tx: RefCell<Producer<Timed<T>>>,
    shared: Arc<Shared<T>>,
}

/// Makes senders of one mailbox; it can be shared between threads.
pub struct MailboxSenders<T> {
    shared: Arc<Shared<T>>,
}

/// Receiving half, owned by the core's thread.
pub struct Mailbox<T> {
    rings: RefCell<Vec<Consumer<Timed<T>>>>,
    shared: Arc<Shared<T>>,
}

/// New mailbox whose receiver is woken with `notify`, [`CAPACITY`] messages per sender.
pub fn channel<T: Send>(notify: Arc<dyn Notify>) -> (MailboxSender<T>, Mailbox<T>) {
    channel_with(CAPACITY, notify)
}

/// New mailbox whose receiver is woken with `notify`, `capacity` messages per sender.
pub fn channel_with<T: Send>(
    capacity: usize,
    notify: Arc<dyn Notify>,
) -> (MailboxSender<T>, Mailbox<T>) {
    let shared = Arc::new(Shared {
        new_rings: Handoff::new(),
        capacity,
        notified: AtomicBool::new(false),
        closed: AtomicBool::new(false),
        notify,
    });
    (shared.sender(), Mailbox { rings: RefCell::new(Vec::new()), shared })
}

impl<T: Send> MailboxSender<T> {
    /// Queues `msg` and wakes the receiver if it has not been woken since its last drain. Gives
    /// the message back if this sender's ring is full (the receiver is behind: push back) or the
    /// receiver no longer exists.
    pub fn send(&self, msg: T) -> Result<(), SendError<T>> {
        if self.shared.closed.load(Ordering::Acquire) {
            return Err(SendError::Closed(msg));
        }
        let timed = (Instant::now(), msg);
        self.tx.borrow_mut().push(timed).map_err(|(_, back)| SendError::Full(back))?;
        // After the push: a receiver that cleared the flag before it either drains this message
        // or is woken by this swap; one that clears it later synchronizes with it and sees it.
        if !self.shared.notified.swap(true, Ordering::AcqRel) {
            self.shared.notify.notify();
        }
        Ok(())
    }

    /// [`MailboxSender::send`] that waits for room while the ring is full; the message back only
    /// if the receiver no longer exists. For threads that are not cores (the disk thread, the
    /// acceptors, the client workers): a core never waits for another one.
    pub fn send_waiting(&self, msg: T) -> Result<(), T> {
        let mut msg = msg;
        loop {
            match self.send(msg) {
                Ok(()) => return Ok(()),
                Err(SendError::Closed(back)) => return Err(back),
                Err(SendError::Full(back)) => {
                    msg = back;
                    std::thread::yield_now();
                }
            }
        }
    }

    /// Whether the receiver has been dropped.
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    /// What makes more senders of this mailbox, to share between threads.
    pub fn senders(&self) -> MailboxSenders<T> {
        MailboxSenders { shared: self.shared.clone() }
    }
}

impl<T: Send> Clone for MailboxSender<T> {
    fn clone(&self) -> Self {
        self.shared.sender()
    }
}

impl<T: Send> MailboxSenders<T> {
    /// A new sender with a ring of its own.
    pub fn sender(&self) -> MailboxSender<T> {
        self.shared.sender()
    }
}

impl<T> Clone for MailboxSenders<T> {
    fn clone(&self) -> Self {
        MailboxSenders { shared: self.shared.clone() }
    }
}

impl<T: Send> Mailbox<T> {
    /// Takes the rings of the senders made since the last look.
    fn take_new_rings(&self) {
        let new = self.shared.new_rings.take_all();
        if !new.is_empty() {
            self.rings.borrow_mut().extend(new);
        }
    }

    /// Moves every queued message to the end of `out` (per sender, in the order of its sends).
    /// After it starts, the next send wakes the receiver again. A ring whose sender is gone goes
    /// once it is empty.
    pub fn drain_into(&mut self, out: &mut Vec<T>) {
        // Cleared before looking at the rings: a message queued after this point either lands in
        // this drain or notifies again, so none is left asleep; a sender made before its first
        // send pushed its ring before that send's swap, so this drain sees the ring.
        self.shared.notified.swap(false, Ordering::AcqRel);
        self.take_new_rings();
        let now = Instant::now();
        self.rings.get_mut().retain_mut(|rx| {
            // Checked before draining: a sender gone now pushes nothing after this drain.
            let gone = rx.is_abandoned();
            while let Some((queued, m)) = rx.pop() {
                let waited = now.saturating_duration_since(queued).as_nanos();
                let waited = u64::try_from(waited).unwrap_or(u64::MAX);
                teseladb_stages::record(teseladb_stages::Stage::MailboxWait, waited);
                out.push(m);
            }
            !gone
        });
    }

    /// Number of queued messages (a snapshot).
    pub fn len(&self) -> usize {
        self.take_new_rings();
        self.rings.borrow().iter().map(Consumer::len).sum()
    }

    /// Whether no message is queued (a snapshot).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A new sender for this mailbox.
    pub fn sender(&self) -> MailboxSender<T> {
        self.shared.sender()
    }

    /// What makes senders of this mailbox, to share between threads.
    pub fn senders(&self) -> MailboxSenders<T> {
        MailboxSenders { shared: self.shared.clone() }
    }
}

impl<T> Drop for Mailbox<T> {
    fn drop(&mut self) {
        // A sender made after this sees `closed` before it sends; its ring stays in the list
        // until the last sender goes and is dropped with it.
        self.shared.closed.store(true, Ordering::Release);
    }
}

impl<T: Send> fmt::Debug for MailboxSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MailboxSender").field("closed", &self.is_closed()).finish()
    }
}

impl<T> fmt::Debug for MailboxSenders<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MailboxSenders").finish_non_exhaustive()
    }
}

impl<T: Send> fmt::Debug for Mailbox<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mailbox").field("len", &self.len()).finish()
    }
}
