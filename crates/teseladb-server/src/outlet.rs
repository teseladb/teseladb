//! One writer for each destination of messages that every shard produces: the node's log lines
//! and the audit log's messages (013 FR-016, 023 FR-018). No shard waits for a write.
//!
//! A message is pushed whole. On a shard it goes to that shard's buffer for the outlet; the first
//! message of a buffer schedules a task that, once the shard's current task yields, hands the
//! whole buffer to the home shard ([`HOME`]) with `smp::submit_to`. The home shard queues what
//! arrives and runs one writer task per outlet, which hands the queue to the destination's
//! [`ShardWriter`] a batch at a time: engine files, a pollable descriptor, or a datagram socket,
//! none of which blocks the shard. A thread that is not a shard posts its message to the home
//! shard through the engine's `alien` queue; where no engine runs (a tool, a unit test, the last
//! lines after the node stopped), the message is written at once with [`Dest::write_now`].
//!
//! Order: the messages of one shard, and those of one thread, reach the destination in the order
//! they were pushed (a buffer is shipped whole, and `submit_to` keeps the order between two
//! shards). Messages are never split or interleaved: a batch is a run of whole messages.
//!
//! An outlet may bound what it holds ([`Outlet::new`]): past the bound, on a shard's buffer or on
//! the home queue, a message is dropped and counted ([`Outlet::take_dropped`]); the node's log
//! does so, the audit log keeps every message.
//!
//! [`Outlet::flush`] waits until what the caller pushed is written (an engine thread parks, a
//! thread that is not a shard blocks; a task of a shard cannot wait and only ships its buffer).
//! [`flush_all`] does it for every outlet of the process, before the node's engine stops.

pub mod dest;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::reactor::internal;
use teseladb::engine::core::reactor::internal_misc::run_in_background;
use teseladb::engine::core::reactor::later;
use teseladb::engine::core::smp::Smp;
use teseladb_io::engine::{answer, wait};

/// The shard that writes every outlet.
pub const HOME: u32 = 0;
/// How long [`Outlet::flush`] waits at most.
pub const FLUSH_LIMIT: Duration = Duration::from_secs(5);

/// Whole messages, one after another.
#[derive(Debug, Default)]
pub struct Batch {
    bytes: Vec<u8>,
    /// Where each message ends in `bytes`.
    ends: Vec<usize>,
}

impl Batch {
    /// A batch of one message.
    #[must_use]
    pub fn one(msg: &[u8]) -> Batch {
        let mut b = Batch::default();
        b.push(msg);
        b
    }

    /// Adds `msg` at the end.
    pub fn push(&mut self, msg: &[u8]) {
        self.bytes.extend_from_slice(msg);
        self.ends.push(self.bytes.len());
    }

    /// Adds the messages of `other` at the end.
    pub fn append(&mut self, other: Batch) {
        if self.ends.is_empty() {
            *self = other;
            return;
        }
        let base = self.bytes.len();
        self.bytes.extend_from_slice(&other.bytes);
        self.ends.extend(other.ends.iter().map(|e| e + base));
    }

    /// Whether it holds no message.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// Its bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// How many messages it holds.
    #[must_use]
    pub fn count(&self) -> usize {
        self.ends.len()
    }

    /// The bytes of its messages, one after another.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Its bytes, taken.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Its messages, in order.
    pub fn messages(&self) -> impl Iterator<Item = &[u8]> {
        let mut start = 0;
        self.ends.iter().map(move |&end| {
            let m = &self.bytes[start..end];
            start = end;
            m
        })
    }
}

/// Where an outlet's messages go.
pub trait Dest: Send + Sync + 'static {
    /// Its writer on the home shard, made there the first time something is written there.
    fn writer(&self) -> Rc<dyn ShardWriter>;
    /// Writes `batch` at once, from a thread where no engine runs.
    fn write_now(&self, batch: &Batch);
}

/// What writes a destination's batches on the home shard, one at a time.
pub trait ShardWriter {
    /// Writes `batch`; resolves once it is written, or lost with a destination that fails (the
    /// node goes on, as it always did with a message it could not write).
    fn write(self: Rc<Self>, batch: Batch) -> Future<()>;
}

struct Inner {
    id: u64,
    dest: Box<dyn Dest>,
    /// Most bytes a shard's buffer and the home queue hold; `None` keeps everything.
    bound: Option<usize>,
    dropped: AtomicU64,
}

/// One destination's writer, fed by every shard. Clones push to the same one.
#[derive(Clone)]
pub struct Outlet {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Outlet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Outlet").field("id", &self.inner.id).finish_non_exhaustive()
    }
}

/// The ids of the outlets of the process.
static NEXT: AtomicU64 = AtomicU64::new(1);
/// Every outlet of the process, for [`flush_all`].
static OUTLETS: Mutex<Vec<Weak<Inner>>> = Mutex::new(Vec::new());

thread_local! {
    /// This shard's buffer of each outlet.
    static BUFFERS: RefCell<HashMap<u64, Batch>> = RefCell::new(HashMap::new());
    /// On the home shard: the queue and writer of each outlet.
    static HOMES: RefCell<HashMap<u64, Home>> = RefCell::new(HashMap::new());
}

/// An outlet on the home shard.
struct Home {
    writer: Rc<dyn ShardWriter>,
    queue: Batch,
    /// Whether its writer task runs.
    busy: bool,
    /// Who waits until the queue is written.
    waiters: Vec<answer::Sender<()>>,
}

impl Outlet {
    /// An outlet to `dest` that holds at most `bound` bytes per buffer (`None`: no bound).
    #[must_use]
    pub fn new(dest: Box<dyn Dest>, bound: Option<usize>) -> Outlet {
        let inner = Arc::new(Inner {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            dest,
            bound,
            dropped: AtomicU64::new(0),
        });
        let mut all = OUTLETS.lock().unwrap_or_else(PoisonError::into_inner);
        all.retain(|w| w.strong_count() > 0);
        all.push(Arc::downgrade(&inner));
        Outlet { inner }
    }

    /// Whether `other` is this outlet.
    #[must_use]
    pub fn same(&self, other: &Outlet) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Messages dropped past the bound since the last call.
    pub fn take_dropped(&self) -> u64 {
        if self.inner.dropped.load(Ordering::Relaxed) == 0 {
            return 0;
        }
        self.inner.dropped.swap(0, Ordering::Relaxed)
    }

    fn fits(&self, held: usize, more: usize) -> bool {
        self.inner.bound.is_none_or(|max| held + more <= max)
    }

    fn drop_messages(&self, n: usize) {
        self.inner.dropped.fetch_add(u64::try_from(n).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// Pushes the whole message `msg`.
    pub fn push(&self, msg: &[u8]) {
        if wait::this_shard().is_some() {
            self.push_here(msg);
            return;
        }
        let me = self.clone();
        let batch = Batch::one(msg);
        if !wait::post(HOME, Box::new(move || home_receive(&me, batch, None))) {
            self.inner.dest.write_now(&Batch::one(msg));
        }
    }

    /// Adds `msg` to this shard's buffer; the first message of a buffer schedules its shipping.
    fn push_here(&self, msg: &[u8]) {
        let id = self.inner.id;
        let first = BUFFERS.with(|b| {
            let mut b = b.borrow_mut();
            let buf = b.entry(id).or_default();
            if !self.fits(buf.len(), msg.len()) {
                return None;
            }
            let first = buf.is_empty();
            buf.push(msg);
            Some(first)
        });
        match first {
            None => self.drop_messages(1),
            Some(true) => {
                let me = self.clone();
                run_in_background(later().then(move |()| {
                    me.ship_here(None);
                    Ok::<(), ExceptionPtr>(())
                }));
            }
            Some(false) => {}
        }
    }

    /// Hands this shard's buffer to the home shard, with `waiter` to tell once it is written.
    fn ship_here(&self, waiter: Option<answer::Sender<()>>) {
        let id = self.inner.id;
        let batch =
            BUFFERS.with(|b| b.borrow_mut().get_mut(&id).map(std::mem::take)).unwrap_or_default();
        if batch.is_empty() && waiter.is_none() {
            return;
        }
        let me = self.clone();
        drop(Smp::submit_to(HOME, move || home_receive(&me, batch, waiter)));
    }

    /// Waits until what the caller pushed is written, at most [`FLUSH_LIMIT`]. A task of a shard
    /// that is not an engine thread cannot wait: its buffer only leaves now.
    pub fn flush(&self) {
        let on_shard = wait::this_shard().is_some();
        if on_shard && !wait::in_engine_thread() {
            self.ship_here(None);
            return;
        }
        let (tx, rx) = answer::channel();
        let waiting = if on_shard {
            self.ship_here(Some(tx));
            true
        } else {
            let me = self.clone();
            wait::post(HOME, Box::new(move || home_receive(&me, Batch::default(), Some(tx))))
        };
        // Without an engine every message was written when it was pushed.
        if waiting {
            let _ = rx.recv_timeout(FLUSH_LIMIT);
        }
    }
}

/// Flushes every outlet of the process ([`Outlet::flush`]): before the node's engine stops.
pub fn flush_all() {
    let all: Vec<Outlet> = OUTLETS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter_map(Weak::upgrade)
        .map(|inner| Outlet { inner })
        .collect();
    for o in all {
        o.flush();
    }
}

/// On the home shard: queues `batch` and starts the outlet's writer task if it is not running.
fn home_receive(o: &Outlet, batch: Batch, waiter: Option<answer::Sender<()>>) {
    let start = HOMES.with(|h| {
        let mut h = h.borrow_mut();
        if h.is_empty() {
            // The writers hold the engine's descriptors (a pipe's), whose drop needs the shard's
            // reactor: they go when it ends, not with the thread after it (a crash at stop).
            internal::at_destroy(|| drop(HOMES.with(|h| std::mem::take(&mut *h.borrow_mut()))));
        }
        let home = h.entry(o.inner.id).or_insert_with(|| Home {
            writer: o.inner.dest.writer(),
            queue: Batch::default(),
            busy: false,
            waiters: Vec::new(),
        });
        if o.fits(home.queue.len(), batch.len()) {
            home.queue.append(batch);
        } else {
            o.drop_messages(batch.count());
        }
        home.waiters.extend(waiter);
        if home.busy {
            return false;
        }
        home.busy = true;
        true
    });
    if start {
        let o = o.clone();
        run_in_background(make_coroutine(async move {
            drain(&o).await;
            Ok(())
        }));
    }
}

/// The writer task of an outlet on the home shard: writes its queue a batch at a time until it
/// is empty, then tells who waits.
async fn drain(o: &Outlet) {
    let id = o.inner.id;
    loop {
        let next = HOMES.with(|h| {
            let mut h = h.borrow_mut();
            let home = h.get_mut(&id)?;
            if home.queue.is_empty() {
                home.busy = false;
                for w in home.waiters.drain(..) {
                    let _ = w.send(());
                }
                return None;
            }
            Some((Rc::clone(&home.writer), std::mem::take(&mut home.queue)))
        });
        let Some((writer, batch)) = next else { return };
        let _ = writer.write(batch).await;
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// A destination that keeps what is written at once.
    #[derive(Default)]
    struct Kept(Arc<Mutex<Vec<u8>>>);

    impl Dest for Kept {
        fn writer(&self) -> Rc<dyn ShardWriter> {
            unreachable!("no engine runs in a unit test")
        }
        fn write_now(&self, batch: &Batch) {
            self.0.lock().unwrap().extend_from_slice(batch.bytes());
        }
    }

    #[test]
    fn a_batch_keeps_its_messages_whole_and_in_order() {
        let mut a = Batch::one(b"one\n");
        a.push(b"two\n");
        let mut b = Batch::default();
        b.push(b"three\n");
        a.append(b);
        let mut empty = Batch::default();
        empty.append(Batch::one(b"x"));
        assert_eq!(empty.count(), 1);
        assert_eq!(a.count(), 3);
        assert_eq!(a.len(), 14);
        let msgs: Vec<&[u8]> = a.messages().collect();
        assert_eq!(msgs, vec![&b"one\n"[..], b"two\n", b"three\n"]);
        assert_eq!(a.into_bytes(), b"one\ntwo\nthree\n");
    }

    #[test]
    fn without_an_engine_a_message_is_written_when_pushed() {
        // In its turn: no engine runs in the process meanwhile.
        let _turn = crate::outbound::test_engine::exclusive();
        let kept = Arc::new(Mutex::new(Vec::new()));
        let o = Outlet::new(Box::new(Kept(Arc::clone(&kept))), Some(8));
        o.push(b"a\n");
        o.push(b"b\n");
        assert_eq!(kept.lock().unwrap().as_slice(), b"a\nb\n");
        // Nothing waits, and nothing was dropped.
        o.flush();
        flush_all();
        assert_eq!(o.take_dropped(), 0);
        assert!(o.same(&o.clone()));
        assert!(format!("{o:?}").contains("Outlet"));
    }
}
