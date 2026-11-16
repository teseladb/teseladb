//! Running work on the shards from threads that are not shards ("alien" threads).
//!
//! An alien thread has no reactor, so it cannot use the cross-shard message queues or futures of
//! the runtime. Instead each shard owns one [`MessageQueue`]: a lock-free multi-producer queue that
//! any thread may push closures into, and that the shard drains from its poll loop, waking itself
//! up if it was sleeping. An [`Instance`] holds one queue per shard and stands for one running set
//! of shards; [`run_on`] sends a fire-and-forget closure to a shard, and [`submit_to`] sends a
//! closure that returns a future and gives the alien thread a blocking channel for its result.

use std::cell::{Cell, UnsafeCell};
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering, compiler_fence};

use crate::engine::core::future::{ExceptionPtr, Future};
use crate::engine::core::metrics;
use crate::engine::core::reactor::Reactor;

/// Header of every queued item: the intrusive link and a type-erased function that runs the item
/// and frees it.
///
/// It is the first field of each concrete item (`#[repr(C)]`), so a pointer to the item is a
/// pointer to its header.
#[repr(C)]
pub struct WorkItem {
    next: AtomicPtr<WorkItem>,
    process_and_dispose: Option<unsafe fn(*mut WorkItem)>,
}

/// A queued closure: the header followed by the closure itself.
#[repr(C)]
struct AsyncWorkItem<Func: FnOnce() + Send> {
    header: WorkItem,
    _func: Func,
}

/// Runs the closure of the [`AsyncWorkItem<Func>`] whose header is `wi`, then frees the item.
///
/// # Safety
/// `wi` must be the header of an `AsyncWorkItem<Func>` allocated with `Box` and not yet run.
unsafe fn async_process_and_dispose<Func: FnOnce() + Send>(wi: *mut WorkItem) {
    // SAFETY: the contract above; the header is at offset 0 of the `#[repr(C)]` item, so the
    // cast gives back the box it was allocated as.
    let item = unsafe { Box::from_raw(wi.cast::<AsyncWorkItem<Func>>()) };
    (item._func)();
}

/// A lock-free multi-producer, single-consumer queue (alien threads push, the owning shard pops),
/// intrusive through `WorkItem::next`, with a stub node (Vyukov's MPSC queue).
///
/// A push is one atomic swap and one store, and never fails or allocates.
struct LfQueueBase {
    head: AtomicPtr<WorkItem>,
    tail: std::cell::Cell<*mut WorkItem>,
    /// The stub node, in a node of its own that moves with nothing (`head` and `tail` keep
    /// its root pointer while the queue itself moves).
    stub: crate::engine::net::node_ptr::NodePtr<WorkItem>,
}

// SAFETY: producers only touch `head` and the `next` links atomically; `tail` is used by the
// single consumer (the shard that owns the queue).
unsafe impl Sync for LfQueueBase {}
// SAFETY: as above.
unsafe impl Send for LfQueueBase {}

impl LfQueueBase {
    fn new() -> Self {
        let stub = crate::engine::net::node_ptr::NodePtr::new(WorkItem {
            next: AtomicPtr::new(std::ptr::null_mut()),
            process_and_dispose: None,
        });
        let s = stub.as_ptr();
        LfQueueBase { head: AtomicPtr::new(s), tail: std::cell::Cell::new(s), stub }
    }

    #[inline]
    fn stub_ptr(&self) -> *mut WorkItem {
        self.stub.as_ptr()
    }

    /// Appends `n`. Callable from any thread; never fails.
    #[inline]
    fn push(&self, n: *mut WorkItem) {
        // SAFETY: `n` is a live item that no other thread can reach yet (it is either fresh or the
        // stub, which the consumer re-inserts only after taking it out).
        unsafe { (*n).next.store(std::ptr::null_mut(), Ordering::Relaxed) };
        let prev = self.head.swap(n, Ordering::AcqRel);
        // SAFETY: `prev` was the head, i.e. the stub or an item still in the queue; the consumer
        // does not free an item until its `next` link is set, which happens here.
        unsafe { (*prev).next.store(n, Ordering::Release) };
    }

    /// Takes the oldest item, or returns `None` if the queue is empty or the next item is not fully
    /// linked yet (a producer is between its swap and its link). Owning shard only.
    fn pop(&self) -> Option<*mut WorkItem> {
        let stub = self.stub_ptr();
        let mut tail = self.tail.get();
        // SAFETY: `tail` is the stub or an item in the queue.
        let mut next = unsafe { (*tail).next.load(Ordering::Acquire) };
        if tail == stub {
            if next.is_null() {
                return None;
            }
            self.tail.set(next);
            tail = next;
            // SAFETY: as above.
            next = unsafe { (*next).next.load(Ordering::Acquire) };
        }
        if !next.is_null() {
            self.tail.set(next);
            return Some(tail);
        }
        if tail != self.head.load(Ordering::Acquire) {
            // a producer is between its swap and its link: not yet visible
            return None;
        }
        self.push(stub);
        // SAFETY: as above.
        next = unsafe { (*tail).next.load(Ordering::Acquire) };
        if !next.is_null() {
            self.tail.set(next);
            return Some(tail);
        }
        None
    }

    /// Returns whether the queue holds no item. Owning shard only.
    #[inline]
    fn empty(&self) -> bool {
        let tail = self.tail.get();
        // SAFETY: as in pop().
        tail == self.stub_ptr() && unsafe { (*tail).next.load(Ordering::Acquire) }.is_null()
    }
}

/// The queue of one shard together with the reactor to wake when an item is pushed.
struct LfQueue {
    // The reactor pointer is placed first, before the queue's hot atomic fields.
    remote: *mut Reactor,
    base: LfQueueBase,
}

impl LfQueue {
    fn new(remote: *mut Reactor) -> Self {
        LfQueue { remote, base: LfQueueBase::new() }
    }

    /// Wakes the owning reactor if it sleeps, after the push is visible (same protocol as the
    /// cross-shard queues of the SMP layer).
    fn maybe_wakeup(&self) {
        compiler_fence(Ordering::SeqCst);
        // SAFETY: the reactors are created before the alien queues and destroyed after them, so the
        // pointer is live.
        unsafe { Reactor::wakeup_from(self.remote) };
    }
}

/// Producer-side statistics, on a cache line of their own.
#[repr(C, align(64))]
struct Sent {
    value: AtomicUsize,
}

/// Consumer-side statistics, on a cache line of their own.
#[repr(C, align(64))]
struct Received {
    _received: Cell<usize>,
    _last_rcv_batch: Cell<usize>,
}

/// The alien-to-shard queue of one shard, with its statistics and metrics.
///
/// Alien threads only call [`submit`](Self::submit); everything else runs on the owning shard.
#[repr(C)]
pub struct MessageQueue {
    _pending: LfQueue,
    _sent: Sent,
    // Kept between the two statistics blocks so that at least one cache line separates them,
    // which stops the hardware prefetcher from pulling in a line used by another CPU.
    /// Used by the owning shard only (`start`, `stop`), hence the cell: alien threads share
    /// the queue by `&`.
    _metrics: UnsafeCell<metrics::MetricGroups>,
    stats: Received,
}

// SAFETY: alien threads only call `submit`, which touches the lock-free queue and `_sent`;
// the statistics, the metrics and the consumer side of the queue are used by the owning
// shard only.
unsafe impl Sync for MessageQueue {}
// SAFETY: as above.
unsafe impl Send for MessageQueue {}

impl MessageQueue {
    const BATCH_SIZE: usize = 128;
    const PREFETCH_CNT: usize = 2;

    /// Creates the empty queue of the shard whose reactor is `to`.
    pub fn new(to: *mut Reactor) -> Self {
        MessageQueue {
            _pending: LfQueue::new(to),
            _sent: Sent { value: AtomicUsize::new(0) },
            _metrics: UnsafeCell::new(metrics::MetricGroups::new()),
            stats: Received { _received: Cell::new(0), _last_rcv_batch: Cell::new(0) },
        }
    }

    /// Unregisters the queue's metrics. Called on the owning shard when it stops.
    pub fn stop(&self) {
        // SAFETY: the metrics are used by the owning shard only, and not re-entrantly.
        unsafe { (*self._metrics.get()).clear() };
    }

    /// Pushes `item`, wakes the shard, and counts the message.
    fn submit_item(&self, item: *mut WorkItem) {
        self._pending.base.push(item);
        self._pending.maybe_wakeup();
        self._sent.value.fetch_add(1, Ordering::Relaxed);
    }

    /// Queues `func` to run on the owning shard. Callable from any thread; allocates the item.
    pub fn submit<Func: FnOnce() + Send + 'static>(&self, func: Func) {
        let wi = Box::new(AsyncWorkItem {
            header: WorkItem {
                next: AtomicPtr::new(std::ptr::null_mut()),
                process_and_dispose: Some(async_process_and_dispose::<Func>),
            },
            _func: func,
        });
        self.submit_item(Box::into_raw(wi).cast::<WorkItem>());
    }

    /// Returns whether items are waiting, without taking any. Owning shard only.
    pub fn pure_poll_rx(&self) -> bool {
        !self._pending.base.empty()
    }

    /// Pops up to `BATCH_SIZE + 1` items from `q` and calls `process` on each, prefetching the
    /// next items ahead of it; returns how many were processed.
    fn process_queue<F: FnMut(*mut WorkItem)>(q: &LfQueue, mut process: F) -> usize {
        // Copy the batch to local memory first, to shorten the time cross-CPU data is accessed.
        let Some(mut wi) = q.base.pop() else {
            return 0;
        };
        // Left uninitialized (this is a hot path); the first `nr + PREFETCH_CNT` slots are written
        // below before they are read.
        let mut items = [std::mem::MaybeUninit::<*mut WorkItem>::uninit();
            MessageQueue::BATCH_SIZE + MessageQueue::PREFETCH_CNT];
        // Start prefetching the first item before popping the rest, to overlap its memory access
        // with the cache miss the second pop may cause.
        crate::engine::core::prefetch::prefetch::<2>(wi as *const u8);
        let mut nr = 0usize;
        while nr < MessageQueue::BATCH_SIZE {
            match q.base.pop() {
                Some(p) => {
                    // SAFETY: `nr < BATCH_SIZE`.
                    unsafe { items.get_unchecked_mut(nr).write(p) };
                    nr += 1;
                }
                None => break,
            }
        }
        // SAFETY: slot `nr - 1` was written when `nr != 0`.
        let fill = if nr != 0 { unsafe { items.get_unchecked(nr - 1).assume_init() } } else { wi };
        for slot in &mut items[nr..nr + MessageQueue::PREFETCH_CNT] {
            slot.write(fill);
        }
        // Every slot in [0, nr + PREFETCH_CNT) is written now.
        let mut i = 0usize;
        loop {
            for k in i..i + MessageQueue::PREFETCH_CNT {
                // SAFETY: `k < nr + PREFETCH_CNT` (i <= nr), so the slot is written.
                crate::engine::core::prefetch::prefetch::<2>(unsafe {
                    items.get_unchecked(k).assume_init()
                } as *const u8);
            }
            process(wi);
            // SAFETY: `i <= nr`, so the slot is written.
            wi = unsafe { items.get_unchecked(i).assume_init() };
            i += 1;
            if i > nr {
                break;
            }
        }
        nr + 1
    }

    /// Runs the waiting items (a batch at most) and returns how many ran. Owning shard only, from
    /// its poll loop.
    pub fn process_incoming(&self) -> usize {
        if self._pending.base.empty() {
            return 0;
        }
        let nr = Self::process_queue(&self._pending, |wi| {
            // SAFETY: every queued item has its process function (only the stub has none).
            unsafe {
                if let Some(f) = (*wi).process_and_dispose {
                    f(wi);
                }
            }
        });
        self.stats._received.set(self.stats._received.get() + nr);
        self.stats._last_rcv_batch.set(nr);
        nr
    }

    /// Registers the queue's metrics in the `alien` group. Called on the owning shard.
    ///
    /// # Errors
    /// Fails if the metrics cannot be registered (for example, the names are already taken).
    pub fn start(&self) -> Result<(), ExceptionPtr> {
        use crate::engine::core::metrics as sm;
        let this = self as *const MessageQueue;
        // SAFETY: the metrics are used by the owning shard only, and not re-entrantly. The
        // closures dereference `this`, which stays valid while they are registered: the metrics are
        // owned by the queue and cleared in stop(), before the queue is dropped.
        unsafe { &mut *self._metrics.get() }.add_group(
            "alien",
            vec![
                // Absolute value of num packets in last tx batch.
                sm::make_queue_length(
                    "receive_batch_queue_length",
                    // SAFETY: `this` is the queue, live while the metric is registered (see above); the
                    // closure runs on the owning shard and only reads a `Cell` there.
                    move || unsafe { (*this).stats._last_rcv_batch.get() } as u64,
                    sm::description("Current receive batch queue length"),
                    vec![],
                ),
                sm::make_counter(
                    "total_received_messages",
                    // SAFETY: `this` is the queue, live while the metric is registered (see above); the
                    // closure runs on the owning shard and only reads a `Cell` there.
                    move || unsafe { (*this).stats._received.get() } as u64,
                    sm::description("Total number of received messages"),
                    vec![],
                ),
                sm::make_counter(
                    "total_sent_messages",
                    // SAFETY: `this` is the queue, live while the metric is registered (see above); the
                    // closure only loads an atomic counter.
                    move || unsafe { (*this)._sent.value.load(Ordering::Relaxed) } as u64,
                    sm::description("Total number of sent messages"),
                    vec![],
                ),
            ],
        )?;
        Ok(())
    }
}

/// Storage of the queues and of the process's default instance.
pub mod internal {
    use super::*;

    /// The queues of an instance, indexed by shard id.
    pub type Qs = Box<[MessageQueue]>;

    /// The instance of the running application, set by the SMP layer at start-up; null when no
    /// shards run.
    pub static DEFAULT_INSTANCE: AtomicPtr<Instance> = AtomicPtr::new(std::ptr::null_mut());

    /// Returns the default instance, or null if no shards run.
    #[inline]
    pub fn default_instance() -> *mut Instance {
        DEFAULT_INSTANCE.load(Ordering::Acquire)
    }
}

/// One running set of shards, as seen from alien threads.
///
/// An application normally has one instance, but in-process cluster tests may run several.
/// Functions such as [`run_on`] address a message to an (instance, shard) pair. The instance is
/// shared by `&` between the shards and the alien threads.
#[derive(Default)]
pub struct Instance {
    /// The per-shard queues, installed by the SMP layer once the reactors exist.
    pub _qs: Option<internal::Qs>,
}

impl Instance {
    /// Creates one empty queue per reactor, in shard order.
    pub fn create_qs(reactors: &[*mut Reactor]) -> internal::Qs {
        reactors.iter().map(|&r| MessageQueue::new(r)).collect::<Vec<_>>().into_boxed_slice()
    }

    /// Installs the queues; done once by the SMP layer during start-up.
    pub fn set_qs(&mut self, qs: internal::Qs) {
        self._qs = Some(qs);
    }

    /// Returns whether the queues have been installed.
    #[inline]
    pub fn has_queues(&self) -> bool {
        self._qs.is_some()
    }

    /// Returns the queue of `shard`.
    ///
    /// # Panics
    /// Aborts with an assertion failure if the queues are not installed, and panics if `shard` is
    /// out of range.
    #[inline]
    pub fn queue(&self, shard: u32) -> &MessageQueue {
        match self._qs.as_ref() {
            // The shard comes from the caller (any thread, through `submit_to`), so the index is
            // bounds-checked, at the cost of one comparison per message.
            Some(q) => &q[shard as usize],
            None => crate::engine::util::assert::internal::assert_fail(
                "_qs",
                file!(),
                line!() as i32,
                "alien::instance::queue",
            ),
        }
    }

    /// Runs the items waiting in the current shard's queue; returns whether any ran. Called from
    /// the reactor's poll loop.
    #[inline]
    pub fn poll_queues(&self) -> bool {
        let queue = self.queue(crate::engine::core::shard_id::this_shard_id());
        queue.process_incoming() != 0
    }

    /// Returns whether items wait in the current shard's queue, without running them.
    #[inline]
    pub fn pure_poll_queues(&self) -> bool {
        let queue = self.queue(crate::engine::core::shard_id::this_shard_id());
        queue.pure_poll_rx()
    }
}

/// Runs `func` on shard `shard` of `instance`, from any thread, including one with no reactor.
///
/// The closure is queued and runs later on the shard; nothing reports its completion or result,
/// because the alien thread cannot receive a promise. Use [`submit_to`] when the result or a
/// failure matters. The closure must not panic.
///
/// # Panics
/// Aborts with an assertion failure if the instance has no queues, and panics if `shard` is out of
/// range.
pub fn run_on<Func: FnOnce() + Send + 'static>(instance: &Instance, shard: u32, func: Func) {
    match instance._qs.as_ref() {
        Some(q) => q[shard as usize].submit(func),
        None => crate::engine::util::assert::internal::assert_fail(
            "_qs",
            file!(),
            line!() as i32,
            "alien::run_on",
        ),
    }
}

/// Same as [`run_on`] on the default instance of the process.
///
/// # Panics
/// Aborts with an assertion failure if no shards run.
#[deprecated(note = "Use run_on(instance, shard, func) instead")]
pub fn run_on_default<Func: FnOnce() + Send + 'static>(shard: u32, func: Func) {
    run_on(default_instance_ref(), shard, func);
}

/// The receiving side of [`submit_to`]: a channel that yields the result once.
///
/// A blocking `recv` waits for the shard to finish.
pub type StdFuture<T> = std::sync::mpsc::Receiver<Result<T, ExceptionPtr>>;

/// Runs `func` on shard `shard` of `instance`, from any thread, and returns a channel that yields
/// the result of the future `func` returns.
///
/// The value or error is sent once the future resolves. If the receiver is dropped first, the
/// result is discarded.
///
/// # Panics
/// Aborts with an assertion failure if the instance has no queues, and panics if `shard` is out of
/// range.
pub fn submit_to<T: Send + 'static, Func: FnOnce() -> Future<T> + Send + 'static>(
    instance: &Instance,
    shard: u32,
    func: Func,
) -> StdFuture<T> {
    let (pr, fut) = std::sync::mpsc::sync_channel::<Result<T, ExceptionPtr>>(1);
    run_on(instance, shard, move || {
        let _ = func().then_wrapped(move |result: Future<T>| {
            let _ = pr.send(result.get());
        });
    });
    fut
}

/// Same as [`submit_to`] on the default instance of the process.
///
/// # Panics
/// Aborts with an assertion failure if no shards run.
#[deprecated(note = "Use submit_to(instance, shard, func) instead.")]
pub fn submit_to_default<T: Send + 'static, Func: FnOnce() -> Future<T> + Send + 'static>(
    shard: u32,
    func: Func,
) -> StdFuture<T> {
    submit_to(default_instance_ref(), shard, func)
}

/// Returns the default instance; aborts with an assertion failure if no shards run (the pointer is
/// null).
#[inline]
fn default_instance_ref() -> &'static Instance {
    let p = internal::default_instance();
    if p.is_null() {
        crate::engine::util::assert::internal::assert_fail(
            "internal::default_instance",
            file!(),
            line!() as i32,
            "alien::default_instance",
        );
    }
    // SAFETY: non-null, checked above: the default instance lives while the shards run, and alien
    // threads share it by `&` (its queues are lock-free on the producer side).
    unsafe { &*p }
}
