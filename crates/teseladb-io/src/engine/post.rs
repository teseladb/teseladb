//! Reaching a core's shard from any thread (023 FR-011): from another shard with the engine's
//! `smp::submit_to`, from any other thread with `alien`, and the shard-local slot where the
//! running core takes what arrives.
//!
//! The engine of a node is the process's only one (D4), so a thread with a reactor is a shard of
//! that engine and `alien` goes through the process's default instance. A [`Gate`] keeps threads
//! that are not shards out once the engine stops: a stop closes it and waits for the senders
//! already inside before the engine's queues go away.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::poll_fn;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Poll, Waker};
use std::time::Instant;

use teseladb::engine::core::alien;
use teseladb::engine::core::reactor::engine_is_ready;
use teseladb::engine::core::smp::Smp;
use teseladb_raftlog::IoDone;

use super::core_loop::{CoreInput, SendError};
use super::peers::{NetEvent, Relayed};

/// The bit of [`Gate`]'s state that says it is closed; the rest counts the senders inside.
const CLOSED: u64 = 1 << 63;

/// Lets threads that are not shards into the engine's `alien` queues while it runs.
#[derive(Debug, Default)]
pub(crate) struct Gate {
    state: AtomicU64,
}

impl Gate {
    /// Enters, unless the gate is closed.
    fn enter(&self) -> bool {
        if self.state.fetch_add(1, Ordering::Acquire) & CLOSED != 0 {
            self.leave();
            return false;
        }
        true
    }

    fn leave(&self) {
        self.state.fetch_sub(1, Ordering::Release);
    }

    /// Whether it is closed.
    pub(crate) fn is_closed(&self) -> bool {
        self.state.load(Ordering::Acquire) & CLOSED != 0
    }

    /// Closes it and waits for the senders inside, which only queue a closure each.
    pub(crate) fn close(&self) {
        self.state.fetch_or(CLOSED, Ordering::AcqRel);
        while self.state.load(Ordering::Acquire) & !CLOSED != 0 {
            std::hint::spin_loop();
        }
    }
}

/// Runs closures on one shard of the node's engine, from any thread.
#[derive(Clone, Debug)]
pub(crate) struct ShardPost {
    shard: u32,
    gate: Arc<Gate>,
}

impl ShardPost {
    /// Posts to `shard` while `gate` is open.
    pub(crate) fn new(shard: u32, gate: Arc<Gate>) -> ShardPost {
        ShardPost { shard, gate }
    }

    /// The shard.
    pub(crate) fn shard(&self) -> u32 {
        self.shard
    }

    /// Whether the engine stopped.
    pub(crate) fn is_closed(&self) -> bool {
        self.gate.is_closed()
    }

    /// Runs `f` on the shard; `false` (and `f` dropped) once the engine stopped.
    pub(crate) fn post(&self, f: impl FnOnce() + Send + 'static) -> bool {
        if engine_is_ready() {
            // A shard of the engine: it runs until the engine's end, after every gate closed.
            drop(Smp::submit_to(self.shard, f));
            return true;
        }
        if !self.gate.enter() {
            return false;
        }
        // The process's default instance is the node's engine's while the gate is open.
        #[allow(deprecated)]
        alien::run_on_default(self.shard, f);
        self.gate.leave();
        true
    }

    /// Hands `input` to the core of the shard.
    pub(crate) fn post_input<M: Send + 'static>(
        &self,
        at: Instant,
        input: CoreInput<M>,
    ) -> Result<(), SendError<CoreInput<M>>> {
        if self.gate.is_closed() {
            return Err(SendError::Closed(input));
        }
        // An engine that stops meanwhile drops the input with the closure, as a stopping core
        // drops what it has not taken.
        self.post(move || deliver_input(at, input));
        Ok(())
    }

    /// Hands a result of the node's device log to the core of the shard.
    pub(crate) fn post_log(&self, at: Instant, done: IoDone) -> bool {
        self.post(move || with_slot(|s| s.io.push_log(at, done)))
    }

    /// Hands an event of a connection that another shard holds to the core of the shard.
    pub(crate) fn post_relay(&self, ev: Relayed) -> bool {
        self.post(move || with_slot(|s| s.io.push_net(NetEvent::Relayed(ev))))
    }
}

/// Wakes a shard's task from anything that runs on the shard: a timer, a delivery, a socket.
#[derive(Default)]
pub(crate) struct Wake {
    set: Cell<bool>,
    waker: Cell<Option<Waker>>,
}

impl Wake {
    /// Wakes the task (once, until it waits again).
    pub(crate) fn wake(&self) {
        self.set.set(true);
        if let Some(w) = self.waker.take() {
            w.wake();
        }
    }

    /// Waits until woken since the last wait returned.
    pub(crate) async fn wait(&self) {
        poll_fn(|cx| {
            if self.set.replace(false) {
                return Poll::Ready(());
            }
            self.waker.set(Some(cx.waker().clone()));
            Poll::Pending
        })
        .await;
    }
}

/// What reaches a core's task besides its messages: the results of the node's device log and what
/// happens on its connections to other nodes. The disk executor runs on the shard and wakes the
/// task itself.
#[derive(Default)]
pub(crate) struct CoreIo {
    pub(crate) wake: Wake,
    log: RefCell<Vec<(Instant, IoDone)>>,
    net: RefCell<VecDeque<NetEvent>>,
}

impl CoreIo {
    /// Queues an event of the core's connections and wakes the core.
    pub(crate) fn push_net(&self, ev: NetEvent) {
        self.net.borrow_mut().push_back(ev);
        self.wake.wake();
    }

    /// Moves the events of the core's connections to `out`.
    pub(crate) fn take_net(&self, out: &mut VecDeque<NetEvent>) {
        out.append(&mut self.net.borrow_mut());
    }

    fn push_log(&self, at: Instant, done: IoDone) {
        self.log.borrow_mut().push((at, done));
        self.wake.wake();
    }

    /// Moves the device log's results that arrived to `out`.
    pub(crate) fn take_log(&self, out: &mut Vec<(Instant, IoDone)>) {
        out.append(&mut self.log.borrow_mut());
    }
}

/// The messages of a core whose logic takes `M`, each with the instant it was sent.
pub(crate) struct Inbox<M> {
    queue: RefCell<VecDeque<(Instant, CoreInput<M>)>>,
}

impl<M> Inbox<M> {
    pub(crate) fn new() -> Inbox<M> {
        Inbox { queue: RefCell::new(VecDeque::new()) }
    }

    /// Moves what arrived to `out`.
    pub(crate) fn drain_into(&self, out: &mut Vec<(Instant, CoreInput<M>)>) {
        out.extend(self.queue.borrow_mut().drain(..));
    }

    /// Whether nothing waits.
    pub(crate) fn is_empty(&self) -> bool {
        self.queue.borrow().is_empty()
    }
}

/// What a shard's running core shares with the deliveries.
pub(crate) struct Slot {
    pub(crate) io: Rc<CoreIo>,
    /// The core's [`Inbox`], typed by its logic's messages.
    inbox: Rc<dyn Any>,
}

thread_local! {
    /// The core running on this shard, if any.
    static SLOT: RefCell<Option<Rc<Slot>>> = const { RefCell::new(None) };
}

/// Installs a core on this shard: what is delivered to the shard from now on goes to it.
pub(crate) fn install<M: 'static>(io: Rc<CoreIo>, inbox: Rc<Inbox<M>>) {
    SLOT.with(|s| *s.borrow_mut() = Some(Rc::new(Slot { io, inbox })));
}

/// Removes the shard's core: what arrives afterwards is dropped.
pub(crate) fn uninstall() {
    SLOT.with(|s| s.borrow_mut().take());
}

fn with_slot(f: impl FnOnce(&Slot)) {
    // Cloned out, so that a delivery may run while the core's task holds nothing borrowed.
    if let Some(slot) = SLOT.with(|s| s.borrow().clone()) {
        f(&slot);
    }
}

/// Runs `f` with the I/O of the core installed on this shard, if any.
pub(crate) fn with_io(f: impl FnOnce(&CoreIo)) {
    with_slot(|s| f(&s.io));
}

fn deliver_input<M: 'static>(at: Instant, input: CoreInput<M>) {
    with_slot(|slot| {
        let Some(inbox) = slot.inbox.downcast_ref::<Inbox<M>>() else { return };
        inbox.queue.borrow_mut().push_back((at, input));
        slot.io.wake.wake();
    });
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_closed_gate_lets_nobody_in_and_waits_for_those_inside() {
        let g = Gate::default();
        assert!(g.enter());
        assert!(!g.is_closed());
        g.leave();
        g.close();
        assert!(g.is_closed());
        assert!(!g.enter());
    }

    #[test]
    fn a_post_after_the_stop_drops_the_input() {
        let gate = Arc::new(Gate::default());
        gate.close();
        let p = ShardPost::new(0, gate);
        assert!(p.is_closed());
        assert_eq!(p.shard(), 0);
        assert!(!p.post(|| unreachable!()));
        assert!(matches!(
            p.post_input(Instant::now(), CoreInput::App(1u8)),
            Err(SendError::Closed(CoreInput::App(1)))
        ));
        assert!(
            !p.post_log(Instant::now(), IoDone::Read { token: 1, data: Ok(Default::default()) })
        );
    }

    #[test]
    fn deliveries_reach_the_installed_core_and_nothing_else() {
        let io = Rc::new(CoreIo::default());
        let inbox = Rc::new(Inbox::<u8>::new());
        // Nothing installed: dropped.
        deliver_input(Instant::now(), CoreInput::App(1u8));
        install(io.clone(), inbox.clone());
        deliver_input(Instant::now(), CoreInput::App(2u8));
        // Another type of message is not this core's.
        deliver_input(Instant::now(), CoreInput::App(3u16));
        with_slot(|s| {
            s.io.push_log(Instant::now(), IoDone::Read { token: 4, data: Ok(Default::default()) })
        });
        assert!(!inbox.is_empty());
        let mut got = Vec::new();
        inbox.drain_into(&mut got);
        assert!(matches!(got.as_slice(), [(_, CoreInput::App(2))]));
        let mut log = Vec::new();
        io.take_log(&mut log);
        assert_eq!(log.len(), 1);
        assert!(io.wake.set.get());
        uninstall();
        deliver_input(Instant::now(), CoreInput::App(6u8));
        assert!(inbox.is_empty());
    }
}
