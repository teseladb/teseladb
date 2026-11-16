//! The contract between a core's logic and the task of its shard that drives it ([`CoreLogic`],
//! `contracts/core-logic.md` of 023, unchanged from the loop it replaces), and the way other
//! threads reach a core: [`CoreSender`] hands a message to the core's shard with the engine's
//! `smp::submit_to` from another shard and with `alien` from any other thread (023 FR-011).
//!
//! A sender can also be bound to a plain queue ([`core_channel`]), which the tests and the logic's
//! own tests read by hand.

use std::fmt;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use teseladb_raftlog::{IoDone, IoOp};
use teseladb_types::{Nanos, NodeId};

use crate::file_send::FileSend;
use crate::peer::PeerMsg;
use crate::service::ServiceClass;

/// Frames queued for a node that is not connected yet beyond this are dropped (Raft resends).
pub const MAX_PENDING: usize = 8 << 20;
/// First redial delay after a failure.
pub const BACKOFF_MIN_MS: u64 = 10;
/// Maximum redial delay.
pub const BACKOFF_MAX_MS: u64 = 1_000;
/// How long a stop waits for the logic's writes to reach the disk ([`CoreLogic::io_settled`]): a
/// disk that takes longer (full, or hung) is left as a crash would leave it.
pub const STOP_SETTLE_LIMIT: Duration = Duration::from_secs(10);
/// How long a core goes without work, with its logic at rest, before it ticks every
/// [`REST_TICK`].
pub const REST_AFTER: Nanos = Nanos::from_secs(1);
/// The tick of a core at rest. Linux stops the tick of an idle CPU the same way
/// (`NO_HZ_IDLE`); the first work ends the rest with a tick, so a mark still comes from a wall
/// clock read in the same tick that checked it for jumps (005 FR-007).
pub const REST_TICK: Duration = Duration::from_millis(100);

/// What a core does; the runtime only moves bytes and time.
pub trait CoreLogic {
    /// Messages other threads send to this core.
    type Msg: Send + 'static;
    /// Periodic tick; `wall_ms` is the node's wall clock.
    fn tick(&mut self, now: Nanos, wall_ms: u64);
    /// The core leaves its rest for work that arrived: the clock part of a tick, so that what
    /// the work stamps comes from a wall clock read now (005 FR-007); the rest of the tick comes
    /// at its time (H-044).
    fn wake(&mut self, now: Nanos, wall_ms: u64);
    /// A turn with work starts, on a core that is not at rest: `wall_ms` is the node's wall
    /// clock read now, for what the turn stamps and compares (005 FR-032). Between two ticks
    /// the logic had the reading of the last one, up to a tick old.
    fn clock(&mut self, now: Nanos, wall_ms: u64);
    /// Next disk operation, if any, with its class (012 FR-023).
    fn poll_io(&mut self, now: Nanos) -> Option<(IoOp, crate::IoClass)>;
    /// Result of a disk operation of `class`, in submission order within its class.
    fn on_io_done(&mut self, done: IoDone, class: crate::IoClass, now: Nanos);
    /// Earliest instant at which `poll_io` may have something new without any input.
    fn next_deadline(&self) -> Option<Nanos>;
    /// Frame received from `from`.
    fn on_peer(&mut self, from: NodeId, msg: PeerMsg, now: Nanos);
    /// Message from another thread.
    fn on_message(&mut self, msg: Self::Msg, now: Nanos);
    /// The core is overloaded (012 FR-029): answers `msg` with `Overloaded` if it is a new
    /// request and returns `None`, or gives it back to run as usual (the default, for messages
    /// that are not requests).
    fn refuse_overloaded(&mut self, msg: Self::Msg) -> Option<Self::Msg> {
        Some(msg)
    }
    /// The service class of `msg` (012 FR-030): a background one runs only when the core has
    /// nothing else to do. Everything is in the foreground by default.
    fn service_class(&self, _msg: &Self::Msg) -> ServiceClass {
        ServiceClass::Foreground
    }
    /// Moves the frames to send to `out`.
    fn take_outbound(&mut self, out: &mut Vec<(NodeId, PeerMsg)>);
    /// Whether the runtime sends answers that end with a file's bytes itself (012 FR-026): told
    /// once, before the first turn. It does on a core with an io_uring disk whose links to other
    /// nodes are plaintext; otherwise the logic reads the bytes and answers them whole.
    fn file_sends(&mut self, _on: bool) {}
    /// Moves the answers that end with a file's bytes to `out`, after [`CoreLogic::take_outbound`]
    /// (only after `file_sends(true)`).
    fn take_file_sends(&mut self, _out: &mut Vec<(NodeId, FileSend)>) {}
    /// Inter-node address of `node`, if known.
    fn peer_addr(&self, node: NodeId) -> Option<String>;
    /// The connection to `node` failed or closed: what was in flight may be lost.
    fn peer_unreachable(&mut self, node: NodeId);
    /// Whether every write the logic has taken is on disk: none waits to be handed out and none
    /// is in flight. A stop waits for it, so that a clean stop loses nothing the logic
    /// acknowledged (005 FR-010: a `w: 1` write is in the leader's log once confirmed).
    fn io_settled(&self) -> bool;
    /// Whether the logic has nothing to do until something arrives: its groups are at rest and
    /// none of its work waits for a tick. A core whose logic rests and that had no work for
    /// [`REST_AFTER`] ticks every [`REST_TICK`] instead of every tick (H-037).
    fn resting(&self) -> bool;
}

/// What reaches a core from other threads.
#[derive(Debug)]
pub enum CoreInput<M> {
    /// Message for the logic.
    App(M),
    /// Ends the core's task (it settles its writes first).
    Stop,
}

/// Why a message did not reach a core; it comes back with the error.
#[derive(Debug, PartialEq, Eq)]
pub enum SendError<T> {
    /// The queue of a bounded test channel has no room ([`core_channel_with`]).
    Full(T),
    /// The core is gone, or its node's engine stopped.
    Closed(T),
}

impl<T> SendError<T> {
    /// The message that did not go.
    pub fn into_inner(self) -> T {
        match self {
            SendError::Full(t) | SendError::Closed(t) => t,
        }
    }
}

/// How a sender reaches its core.
enum Route<M> {
    /// The core's shard in its node's engine.
    #[cfg(target_os = "linux")]
    Shard(super::post::ShardPost),
    /// A queue read by hand.
    Queue(SyncSender<(Instant, CoreInput<M>)>),
}

/// Hands messages to one core. Cheap to clone; usable from any thread.
pub struct CoreSender<M> {
    route: Route<M>,
}

/// What makes senders of a core's mailbox: since the engine, a sender itself, kept as a name
/// for the places that hand one to each thread.
pub type CoreSenders<M> = CoreSender<M>;

impl<M: Send + 'static> CoreSender<M> {
    /// The sender of the core running on `post`'s shard.
    #[cfg(target_os = "linux")]
    pub(crate) fn on_shard(post: super::post::ShardPost) -> CoreSender<M> {
        CoreSender { route: Route::Shard(post) }
    }

    /// Hands `input` to the core. Never waits: a core reached through its engine takes every
    /// message; a bounded test channel may be full.
    ///
    /// # Errors
    ///
    /// [`SendError::Closed`] when the core or its engine stopped, [`SendError::Full`] when a
    /// bounded test channel has no room.
    pub fn send(&self, input: CoreInput<M>) -> Result<(), SendError<CoreInput<M>>> {
        match &self.route {
            #[cfg(target_os = "linux")]
            Route::Shard(post) => {
                let at = Instant::now();
                post.post_input(at, input)
            }
            Route::Queue(q) => match q.try_send((Instant::now(), input)) {
                Ok(()) => Ok(()),
                Err(TrySendError::Full((_, m))) => Err(SendError::Full(m)),
                Err(TrySendError::Disconnected((_, m))) => Err(SendError::Closed(m)),
            },
        }
    }

    /// Like [`CoreSender::send`], waiting for room in a full bounded test channel.
    ///
    /// # Errors
    ///
    /// Gives `input` back when the core or its engine stopped.
    pub fn send_waiting(&self, input: CoreInput<M>) -> Result<(), CoreInput<M>> {
        match &self.route {
            Route::Queue(q) => q.send((Instant::now(), input)).map_err(|e| e.0.1),
            #[cfg(target_os = "linux")]
            Route::Shard(_) => self.send(input).map_err(SendError::into_inner),
        }
    }

    /// Whether the core can no longer be reached.
    pub fn is_closed(&self) -> bool {
        match &self.route {
            #[cfg(target_os = "linux")]
            Route::Shard(post) => post.is_closed(),
            // A queue says it when a send fails.
            Route::Queue(_) => false,
        }
    }

    /// A sender for another thread (the same as a clone).
    pub fn senders(&self) -> CoreSenders<M> {
        self.clone()
    }

    /// A sender for this thread (the same as a clone).
    pub fn sender(&self) -> CoreSender<M> {
        self.clone()
    }
}

impl<M> Clone for CoreSender<M> {
    fn clone(&self) -> Self {
        let route = match &self.route {
            #[cfg(target_os = "linux")]
            Route::Shard(p) => Route::Shard(p.clone()),
            Route::Queue(q) => Route::Queue(q.clone()),
        };
        CoreSender { route }
    }
}

impl<M> fmt::Debug for CoreSender<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.route {
            #[cfg(target_os = "linux")]
            Route::Shard(p) => write!(f, "CoreSender(shard {})", p.shard()),
            Route::Queue(_) => f.write_str("CoreSender(queue)"),
        }
    }
}

/// The reading end of a [`core_channel`].
#[derive(Debug)]
pub struct CoreReceiver<M> {
    rx: Receiver<(Instant, CoreInput<M>)>,
}

impl<M> CoreReceiver<M> {
    /// Moves every input waiting to `out`.
    pub fn drain_into(&mut self, out: &mut Vec<CoreInput<M>>) {
        out.extend(self.rx.try_iter().map(|(_, m)| m));
    }

    /// Moves every input waiting to `out`, with the instant it was sent.
    pub fn drain_timed_into(&mut self, out: &mut Vec<(Instant, CoreInput<M>)>) {
        out.extend(self.rx.try_iter());
    }

    /// The next input, waiting up to `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<CoreInput<M>> {
        self.rx.recv_timeout(timeout).ok().map(|(_, m)| m)
    }
}

/// Room of an unbounded test channel's queue.
const UNBOUNDED: usize = 1 << 20;

/// A sender bound to a queue that is read by hand instead of a core (for tests).
pub fn core_channel<M: Send + 'static>() -> (CoreSender<M>, CoreReceiver<M>) {
    core_channel_with(UNBOUNDED)
}

/// [`core_channel`] with room for `cap` inputs: past them, [`CoreSender::send`] answers
/// [`SendError::Full`].
pub fn core_channel_with<M: Send + 'static>(cap: usize) -> (CoreSender<M>, CoreReceiver<M>) {
    let (tx, rx) = mpsc::sync_channel(cap);
    (CoreSender { route: Route::Queue(tx) }, CoreReceiver { rx })
}

impl<M: PartialEq> PartialEq for CoreInput<M> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (CoreInput::App(a), CoreInput::App(b)) => a == b,
            (CoreInput::Stop, CoreInput::Stop) => true,
            _ => false,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_queue_channel_delivers_in_order_and_says_when_it_is_full_or_closed() {
        let (tx, mut rx) = core_channel_with::<u32>(2);
        assert!(tx.send(CoreInput::App(1)).is_ok());
        assert!(tx.senders().sender().send(CoreInput::App(2)).is_ok());
        assert_eq!(tx.send(CoreInput::App(3)), Err(SendError::Full(CoreInput::App(3))));
        let mut got = Vec::new();
        rx.drain_into(&mut got);
        assert!(matches!(got.as_slice(), [CoreInput::App(1), CoreInput::App(2)]));
        assert!(tx.send_waiting(CoreInput::Stop).is_ok());
        let mut timed = Vec::new();
        rx.drain_timed_into(&mut timed);
        assert!(matches!(timed.as_slice(), [(_, CoreInput::Stop)]));
        assert!(rx.recv_timeout(Duration::from_millis(1)).is_none());
        assert!(!tx.is_closed());
        assert_eq!(format!("{tx:?}"), "CoreSender(queue)");
        drop(rx);
        assert_eq!(tx.send(CoreInput::App(4)).unwrap_err().into_inner(), CoreInput::App(4));
        assert!(matches!(tx.send_waiting(CoreInput::App(5)), Err(CoreInput::App(5))));
    }

    #[test]
    fn an_unbounded_channel_takes_many() {
        let (tx, rx) = core_channel::<u32>();
        for i in 0..1000 {
            tx.send(CoreInput::App(i)).unwrap();
        }
        assert!(matches!(rx.recv_timeout(Duration::from_millis(1)), Some(CoreInput::App(0))));
    }
}
