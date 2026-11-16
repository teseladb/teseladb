//! The queue that hands blocking system calls from a reactor to its syscall thread.
//!
//! Each shard has a helper thread for system calls that may block (for example `fsync` or
//! file metadata calls on file systems without asynchronous support). The reactor pushes
//! [`WorkItem`]s onto a bounded single-producer single-consumer ring ([`SpscQueue`]) and wakes
//! the syscall thread through an eventfd; the syscall thread runs each item and pushes it onto
//! a second ring, from which the reactor completes the item's promise on its own thread.
//! A semaphore limits the items in flight to [`SyscallWorkQueue::QUEUE_LENGTH`], so neither
//! ring can overflow.
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::engine::core::future::{ExceptionPtr, Future, Promise};
use crate::engine::core::internal::pollable_fd::WriteableEventfd;
use crate::engine::core::semaphore::Semaphore;
use crate::engine::util::noncopyable_function::NoncopyableFunction;

/// An index on its own cache line, so the producer's and the consumer's indices do not share
/// one (avoids false sharing).
#[repr(C, align(64))]
struct PaddedIndex(AtomicUsize);

/// A wait-free bounded ring buffer for one producer thread and one consumer thread, holding up
/// to `N` elements. One slot is kept empty to tell a full ring from an empty one, so the
/// storage has `N + 1` slots.
pub struct SpscQueue<T: Copy, const N: usize> {
    write_index: PaddedIndex,
    read_index: PaddedIndex,
    buffer: [UnsafeCell<MaybeUninit<T>>; N],
    extra: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY: the ring is designed for one producer and one consumer on different threads; the
// indices are published with release/acquire ordering.
unsafe impl<T: Copy + Send, const N: usize> Sync for SpscQueue<T, N> {}
// SAFETY: as above.
unsafe impl<T: Copy + Send, const N: usize> Send for SpscQueue<T, N> {}

impl<T: Copy, const N: usize> SpscQueue<T, N> {
    const SLOTS: usize = N + 1;

    /// An empty queue.
    pub fn new() -> Self {
        SpscQueue {
            write_index: PaddedIndex(AtomicUsize::new(0)),
            read_index: PaddedIndex(AtomicUsize::new(0)),
            buffer: [const { UnsafeCell::new(MaybeUninit::uninit()) }; N],
            extra: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    #[inline]
    fn slot(&self, i: usize) -> *mut MaybeUninit<T> {
        if i < N {
            // SAFETY: `i < N`.
            unsafe { self.buffer.get_unchecked(i).get() }
        } else {
            self.extra.get()
        }
    }

    #[inline]
    fn next(i: usize) -> usize {
        let n = i + 1;
        if n == Self::SLOTS { 0 } else { n }
    }

    /// Appends `t`; returns `false` if the queue is full. Producer side only.
    #[inline]
    pub fn push(&self, t: T) -> bool {
        let w = self.write_index.0.load(Ordering::Relaxed);
        let next = Self::next(w);
        if next == self.read_index.0.load(Ordering::Acquire) {
            return false;
        }
        // SAFETY: the slot at the write index is owned by the producer until published.
        unsafe { (*self.slot(w)).write(t) };
        self.write_index.0.store(next, Ordering::Release);
        true
    }

    /// Takes the oldest element, or `None` if the queue is empty. Consumer side only.
    #[inline]
    pub fn pop(&self) -> Option<T> {
        let r = self.read_index.0.load(Ordering::Relaxed);
        if r == self.write_index.0.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: the slot was written and published by the producer.
        let t = unsafe { (*self.slot(r)).assume_init() };
        self.read_index.0.store(Self::next(r), Ordering::Release);
        Some(t)
    }

    /// Hands every available element to `f`, oldest first; returns how many.
    /// Consumer side only.
    #[inline]
    pub fn consume_all<F: FnMut(T)>(&self, mut f: F) -> usize {
        let mut r = self.read_index.0.load(Ordering::Relaxed);
        let w = self.write_index.0.load(Ordering::Acquire);
        let mut n = 0;
        while r != w {
            // SAFETY: slots in [r, w) were written and published by the producer.
            f(unsafe { (*self.slot(r)).assume_init() });
            r = Self::next(r);
            n += 1;
        }
        self.read_index.0.store(r, Ordering::Release);
        n
    }

    /// Whether nothing is available to the consumer.
    #[inline]
    pub fn empty(&self) -> bool {
        self.read_index.0.load(Ordering::Acquire) == self.write_index.0.load(Ordering::Acquire)
    }
}

impl<T: Copy, const N: usize> Default for SpscQueue<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

/// A unit of blocking work run on the syscall thread.
pub trait WorkItem {
    /// Runs the work; called on the syscall thread.
    fn process(&mut self);
    /// Delivers the result; called on the reactor thread after [`process`](Self::process).
    fn complete(&mut self);
    /// Fails the item with `eptr` instead of running it; called on the reactor thread.
    fn set_exception(&mut self, eptr: ExceptionPtr);
}

/// A [`WorkItem`] that runs a function and resolves a future with its result.
///
/// The function runs on the syscall thread and its result is handed back to the reactor,
/// so both are `Send`: the constructor checks it on the closure, and the item itself only
/// crosses the queues as a [`WorkItemPtr`].
pub struct WorkItemReturning<T: 'static> {
    _func: NoncopyableFunction<fn() -> T>,
    _promise: Promise<T>,
    _result: Option<T>,
}

impl<T: Send + 'static> WorkItemReturning<T> {
    /// Wraps `func` (one allocation).
    pub fn new<F: FnMut() -> T + Send + 'static>(func: F) -> Self {
        WorkItemReturning {
            _func: NoncopyableFunction::<fn() -> T>::new(func),
            _promise: Promise::new(),
            _result: None,
        }
    }
    /// The future resolved with the function's result once the item completes. Call once.
    pub fn get_future(&mut self) -> Future<T> {
        self._promise.get_future()
    }
}

impl<T: Send + 'static> WorkItem for WorkItemReturning<T> {
    fn process(&mut self) {
        self._result = Some(self._func.call());
    }
    fn complete(&mut self) {
        if let Some(r) = self._result.take() {
            self._promise.set_value(r);
        }
    }
    fn set_exception(&mut self, eptr: ExceptionPtr) {
        self._promise.set_exception(eptr);
    }
}

/// A queued work item: the pointer that crosses the queues.
#[derive(Clone, Copy)]
pub struct WorkItemPtr(pub *mut dyn WorkItem);

// SAFETY: a work item is touched by one thread at a time: the reactor queues it, the
// syscall thread processes it, and the reactor completes it, with the queues ordering them.
unsafe impl Send for WorkItemPtr {}

/// Hands blocking system calls to the syscall thread.
pub struct SyscallWorkQueue {
    /// Items submitted by the reactor, waiting for the syscall thread.
    pub(crate) _pending: SpscQueue<WorkItemPtr, { SyscallWorkQueue::QUEUE_LENGTH }>,
    /// Items processed by the syscall thread, waiting for the reactor to complete them.
    pub(crate) _completed: SpscQueue<WorkItemPtr, { SyscallWorkQueue::QUEUE_LENGTH }>,
    /// Signalled by the reactor to wake the syscall thread.
    pub(crate) _start_eventfd: WriteableEventfd,
    /// Used by the reactor thread only (the syscall thread touches the two rings and the
    /// eventfd), hence the cell: the queue is shared between the two threads by `&`.
    pub(crate) _queue_has_room: UnsafeCell<Semaphore>,
}

// SAFETY: the syscall thread only uses `_pending`, `_completed` and `_start_eventfd`, which
// are made for one producer and one consumer on different threads; `_queue_has_room` is
// used by the reactor thread only.
unsafe impl Sync for SyscallWorkQueue {}

impl SyscallWorkQueue {
    /// The maximum number of items in flight.
    pub const QUEUE_LENGTH: usize = 128;

    /// An empty queue.
    ///
    /// # Errors
    ///
    /// Returns the error raised when the wake-up eventfd cannot be created.
    pub fn new() -> Result<Self, ExceptionPtr> {
        Ok(SyscallWorkQueue {
            _pending: SpscQueue::new(),
            _completed: SpscQueue::new(),
            _start_eventfd: WriteableEventfd::new(0)?,
            _queue_has_room: UnsafeCell::new(Semaphore::new(Self::QUEUE_LENGTH)),
        })
    }

    /// Runs `func` on the syscall thread and returns a future for its result.
    ///
    /// Called on the reactor thread.
    pub fn submit<T: Send + 'static, F: FnMut() -> T + Send + 'static>(
        &self,
        func: F,
    ) -> Future<T> {
        let mut wi = Box::new(WorkItemReturning::new(func));
        let fut = wi.get_future();
        self.submit_item(wi);
        fut
    }

    /// Scans the completed queue, which holds the requests already handled by the syscall
    /// thread, completing each one and opening up space for more submissions. From the
    /// reactor's point of view a request is not handled until it is removed from that queue.
    ///
    /// Returns the number of requests handled. Allocation-free.
    pub(crate) fn complete(&self) -> u32 {
        let mut tmp_buf: [MaybeUninit<WorkItemPtr>; Self::QUEUE_LENGTH] =
            [MaybeUninit::uninit(); Self::QUEUE_LENGTH];
        let mut end = 0usize;
        let nr = self._completed.consume_all(|wi| {
            // The producer pushes at most QUEUE_LENGTH items (the semaphore bounds them).
            // SAFETY: `end < QUEUE_LENGTH` because the ring holds at most QUEUE_LENGTH items.
            unsafe { tmp_buf.get_unchecked_mut(end).write(wi) };
            end += 1;
        });
        for slot in &tmp_buf[..end] {
            // SAFETY: the first `end` slots were written above.
            let wi = unsafe { slot.assume_init() };
            // SAFETY: the item was allocated by `submit_item` and is owned by us again.
            unsafe {
                (*wi.0).complete();
                drop(Box::from_raw(wi.0));
            }
        }
        // SAFETY: the semaphore is used by the reactor thread only (this one), and the
        // reference ends with the call.
        unsafe { (*self._queue_has_room.get()).signal(nr) };
        nr as u32
    }

    /// Queues `item` for the syscall thread once a slot is free (waiting on the semaphore if
    /// [`QUEUE_LENGTH`](Self::QUEUE_LENGTH) items are in flight). If the wait fails, the item is
    /// failed with that error instead.
    pub(crate) fn submit_item(&self, item: Box<dyn WorkItem>) {
        let this = self as *const SyscallWorkQueue;
        let mut item = Some(item);
        // SAFETY: the semaphore is used by the reactor thread only (this one), and the
        // reference ends with the call.
        let wait = unsafe { (*self._queue_has_room.get()).wait(1) };
        let _ = wait.then_wrapped(move |f: Future<()>| {
            let Some(mut item) = item.take() else { return };
            // propagate wait failure via work_item
            if f.failed() {
                item.set_exception(f.get_exception());
                return;
            }
            // SAFETY: the queue lives in the thread pool, which outlives its pending submissions.
            let me = unsafe { &*this };
            let pushed = me._pending.push(WorkItemPtr(Box::into_raw(item)));
            debug_assert!(pushed, "the semaphore bounds the queue");
            me._start_eventfd.signal(1);
        });
    }
}
