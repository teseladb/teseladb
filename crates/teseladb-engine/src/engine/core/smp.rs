//! The multicore layer: starting the shards and sending work between them.
//!
//! [`Smp`] starts one reactor thread per shard ([`Smp::configure`]) and tears them down. Shards
//! share nothing; they talk through [`SmpMessageQueue`]s, one per ordered pair of shards, each made
//! of wait-free single-producer single-consumer rings ([`SpscQueue`]). [`Smp::submit_to`] runs a
//! function on another shard and returns a future of its result on the calling shard; the function
//! runs as a task of the target shard, and its result travels back through the same queue.
//!
//! [`SmpServiceGroup`]s bound how many cross-shard calls of a kind may run on a target shard at
//! once, so that a flood of requests cannot exhaust it; [`SmpSubmitToOptions`] chooses the group
//! and a timeout for waiting on it. The queue operations that need the reactor are implemented in
//! [`reactor`](crate::engine::core::reactor).

use std::cell::{Cell, UnsafeCell};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use crate::engine::core::alien;
use crate::engine::core::cacheline::CacheLineAligned;
use crate::engine::core::future::{
    ExceptionPtr, Future, Futurize, HostBox, HostedPromise, InvalidArgument, OutOfRange,
    PromiseHost, make_exception_future, make_exception_ptr, make_ready_future,
};
use crate::engine::core::loop_::parallel_for_each;
use crate::engine::core::lowres_clock::LowresClock;
use crate::engine::core::memory;
use crate::engine::core::metrics;
use crate::engine::core::on_internal_error::on_fatal_internal_error;
use crate::engine::core::posix::PosixThread;
use crate::engine::core::reactor::{Reactor, engine_logger};
use crate::engine::core::resource;
use crate::engine::core::semaphore::{
    BasicSemaphore, NamedSemaphoreExceptionFactory, SemaphoreUnits, with_semaphore,
};
pub use crate::engine::core::shard_id::{ShardId, this_shard_id};
use crate::engine::core::sstring::Sstring;
use crate::engine::core::task::{Task, schedule};

/// Low-level access for the reactor.
pub mod internal {
    use super::SmpServiceGroup;

    /// Returns the index of `ssg` among the service groups.
    #[inline]
    pub fn smp_service_group_id(ssg: SmpServiceGroup) -> u32 {
        ssg._id
    }

    pub use crate::engine::core::prefault::internal::MemoryPrefaulter;
}

/// Configuration of an [`SmpServiceGroup`], for [`create_smp_service_group`].
#[derive(Debug, Clone, Default)]
pub struct SmpServiceGroupConfig {
    /// The maximum number of requests from other shards that execute on a shard concurrently.
    ///
    /// Raised if needed to allow at least one request per other shard.
    pub max_nonlocal_requests: u32,
    /// An optional name for the group; when set, the timeout errors of the group's semaphores
    /// name it.
    pub group_name: Option<Sstring>,
}

/// A resource controller for cross-shard calls.
///
/// A service group limits the concurrency of [`Smp::submit_to`] and similar calls at the target
/// shard. Limiting the caller's concurrency is easy (with a semaphore, say), but at the remote end
/// it is multiplied by the number of other shards, which can be large.
///
/// It is a *group* because it gathers similar calls that share resource usage, need not be
/// isolated from each other, but must be isolated from other groups. Calls in one group should not
/// nest, or they can deadlock.
///
/// Nested `submit_to` calls must form a directed acyclic graph when their service groups are taken
/// as nodes: if a call in group 1 makes a call in group 2, the inner call may not call through
/// group 1 or group 2 again, or it may form a cycle and deadlock. Create a new group instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmpServiceGroup {
    _id: u32,
    #[cfg(engine_debug)]
    _version: u32,
}

impl SmpServiceGroup {
    /// Returns the group with index `id`.
    const fn new(id: u32) -> Self {
        Self {
            _id: id,
            #[cfg(engine_debug)]
            _version: 0,
        }
    }
}

/// Returns the default service group, which imposes no limit on concurrency at the target shard.
///
/// That makes it deadlock-safe, but it can consume unbounded resources, so use it only when the
/// callers' concurrency is very low (administrative tasks, for example).
#[inline]
pub const fn default_smp_service_group() -> SmpServiceGroup {
    SmpServiceGroup::new(0)
}

/// Clock of the cross-shard call timeouts (low resolution, cheap to read).
pub type SmpTimeoutClock = LowresClock;
/// A deadline on [`SmpTimeoutClock`].
pub type SmpTimeoutTimePoint = <LowresClock as crate::engine::core::timer::Clock>::TimePoint;
/// The semaphore that limits one service group's calls into one shard.
pub type SmpServiceGroupSemaphore = BasicSemaphore<NamedSemaphoreExceptionFactory, SmpTimeoutClock>;
/// Units held from an [`SmpServiceGroupSemaphore`] while a call runs.
pub type SmpServiceGroupSemaphoreUnits =
    SemaphoreUnits<NamedSemaphoreExceptionFactory, SmpTimeoutClock>;

/// Returns the deadline that never expires.
pub fn smp_no_timeout() -> SmpTimeoutTimePoint {
    <LowresClock as crate::engine::core::timer::Clock>::max_time_point()
}

/// Options of [`Smp::submit_to_with`]: the service group and a timeout.
#[derive(Debug, Clone, Copy)]
pub struct SmpSubmitToOptions {
    /// The service group that limits the call's concurrency at the target shard.
    pub service_group: SmpServiceGroup,
    /// Deadline for waiting on the service group before the call is processed by the remote shard;
    /// it does not bound the time the call takes to run there.
    pub timeout: SmpTimeoutTimePoint,
}

impl SmpSubmitToOptions {
    /// Returns options with `service_group` and `timeout`.
    pub fn new(service_group: SmpServiceGroup, timeout: SmpTimeoutTimePoint) -> Self {
        Self { service_group, timeout }
    }
}

impl Default for SmpSubmitToOptions {
    fn default() -> Self {
        Self::new(default_smp_service_group(), smp_no_timeout())
    }
}

impl From<SmpServiceGroup> for SmpSubmitToOptions {
    fn from(service_group: SmpServiceGroup) -> Self {
        Self::new(service_group, smp_no_timeout())
    }
}

/// A wait-free single-producer single-consumer ring buffer of capacity `N`.
///
/// It has `N + 1` slots, one always kept empty to tell full from empty. The two indices sit on
/// separate cache lines. Producer methods (`push*`) must be called from one thread and consumer
/// methods (`pop*`, `consume_all`) from one other thread.
#[repr(C)]
pub struct SpscQueue<T: Copy, const N: usize> {
    write_index: CacheLineAligned<AtomicUsize>,
    read_index: CacheLineAligned<AtomicUsize>,
    // N + 1 slots: `spare` is the (N+1)-th, laid out right after the others.
    buffer: [std::cell::UnsafeCell<std::mem::MaybeUninit<T>>; N],
    spare: std::cell::UnsafeCell<std::mem::MaybeUninit<T>>,
}

// SAFETY: one producer and one consumer synchronize through the acquire/release indices
// (the element type is `Send`: `WorkItemPtr` is, as the items it hands over cross shards).
unsafe impl<T: Copy + Send, const N: usize> Sync for SpscQueue<T, N> {}
// SAFETY: as above.
unsafe impl<T: Copy + Send, const N: usize> Send for SpscQueue<T, N> {}

impl<T: Copy, const N: usize> Default for SpscQueue<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy, const N: usize> SpscQueue<T, N> {
    const MAX_SIZE: usize = N + 1;

    /// Returns an empty queue.
    pub fn new() -> Self {
        Self {
            write_index: CacheLineAligned(AtomicUsize::new(0)),
            read_index: CacheLineAligned(AtomicUsize::new(0)),
            buffer: [const { std::cell::UnsafeCell::new(std::mem::MaybeUninit::uninit()) }; N],
            spare: std::cell::UnsafeCell::new(std::mem::MaybeUninit::uninit()),
        }
    }

    /// Returns the slot after `i`, wrapping around.
    fn next_index(i: usize) -> usize {
        let r = i + 1;
        if r >= Self::MAX_SIZE { r - Self::MAX_SIZE } else { r }
    }

    /// Returns a pointer to slot `i` of the `N + 1`, indexed as one array.
    ///
    /// # Safety
    /// `i < MAX_SIZE` (every index is reduced by `next_index`).
    #[inline(always)]
    unsafe fn slot(&self, i: usize) -> *mut T {
        debug_assert!(i < Self::MAX_SIZE);
        // SAFETY: `#[repr(C)]` lays `spare` right after `buffer` with no padding (the same
        // element type), so the N + 1 cells form one array; the pointer is derived from the
        // whole object, and `UnsafeCell<MaybeUninit<T>>` has the layout of `T`.
        unsafe { (&raw const (*(self as *const Self)).buffer).cast::<T>().cast_mut().add(i) }
    }

    /// Number of elements ready to pop.
    fn read_available_(write_index: usize, read_index: usize) -> usize {
        if write_index >= read_index {
            write_index - read_index
        } else {
            write_index + Self::MAX_SIZE - read_index
        }
    }

    /// Number of free slots.
    fn write_available_(write_index: usize, read_index: usize) -> usize {
        let mut ret = read_index as isize - write_index as isize - 1;
        if write_index >= read_index {
            ret += Self::MAX_SIZE as isize;
        }
        ret as usize
    }

    /// Pushes as many elements of `items` as fit, in order; returns how many were pushed.
    /// Producer side.
    pub fn push_range(&self, items: &[T]) -> usize {
        let write_index = self.write_index.0.load(Ordering::Relaxed);
        let read_index = self.read_index.0.load(Ordering::Acquire);
        let avail = Self::write_available_(write_index, read_index).min(items.len());
        let mut w = write_index;
        for &item in &items[..avail] {
            // SAFETY: the slot is free: the consumer has read past it.
            unsafe { self.slot(w).write(item) };
            w = Self::next_index(w);
        }
        self.write_index.0.store(w, Ordering::Release);
        avail
    }

    /// Pushes `t`; returns false if the queue is full. Producer side.
    pub fn push(&self, t: T) -> bool {
        self.push_range(std::slice::from_ref(&t)) == 1
    }

    /// Pops up to `out.len()` elements into `out`; returns how many. Consumer side.
    pub fn pop_into(&self, out: &mut [T]) -> usize {
        let write_index = self.write_index.0.load(Ordering::Acquire);
        let read_index = self.read_index.0.load(Ordering::Relaxed);
        let avail = Self::read_available_(write_index, read_index).min(out.len());
        let mut r = read_index;
        for o in out.iter_mut().take(avail) {
            // SAFETY: the slot was published by the producer's release store.
            *o = unsafe { self.slot(r).read() };
            r = Self::next_index(r);
        }
        self.read_index.0.store(r, Ordering::Release);
        avail
    }

    /// Pops one element, if any. Consumer side.
    #[inline]
    pub fn pop(&self) -> Option<T> {
        let write_index = self.write_index.0.load(Ordering::Acquire);
        let read_index = self.read_index.0.load(Ordering::Relaxed);
        if write_index == read_index {
            return None;
        }
        // SAFETY: the slot was published by the producer's release store.
        let t = unsafe { self.slot(read_index).read() };
        self.read_index.0.store(Self::next_index(read_index), Ordering::Release);
        Some(t)
    }

    /// Returns whether the queue is empty. Consumer side.
    pub fn empty(&self) -> bool {
        self.write_index.0.load(Ordering::Acquire) == self.read_index.0.load(Ordering::Relaxed)
    }

    /// Pops every element and calls `f` on each; returns how many. Consumer side.
    pub fn consume_all(&self, mut f: impl FnMut(T)) -> usize {
        let mut n = 0;
        while let Some(t) = self.pop() {
            f(t);
            n += 1;
        }
        n
    }
}

/// The work-item operations beyond those of a task: the table that [`WorkItemBase`] points to.
pub(crate) struct WorkItemVTable {
    /// Fails the caller's promise with an error (the call could not be delivered).
    pub fail_with: unsafe fn(WorkItemPtr, ExceptionPtr),
    /// Completes the caller's promise with the call's result.
    pub complete: unsafe fn(WorkItemPtr),
    /// Frees the item.
    pub destroy: unsafe fn(WorkItemPtr),
}

impl WorkItemVTable {
    /// The table of the work item type `T`.
    #[inline]
    pub const fn of<T: WorkItem>() -> Self {
        Self {
            fail_with: fail_with_thunk::<T>,
            complete: complete_thunk::<T>,
            destroy: destroy_thunk::<T>,
        }
    }
}

/// Calls [`WorkItem::fail_with`] of `T` on `w`.
///
/// # Safety
/// `w` is the header of a live `T`, derived from a pointer to the whole object.
unsafe fn fail_with_thunk<T: WorkItem>(w: WorkItemPtr, ex: ExceptionPtr) {
    // SAFETY: by the caller; the header is at the start of the repr(C) `T`.
    unsafe { T::fail_with(w.0.cast::<T>(), ex) }
}

/// Calls [`WorkItem::complete`] of `T` on `w`.
///
/// # Safety
/// As for [`fail_with_thunk`].
unsafe fn complete_thunk<T: WorkItem>(w: WorkItemPtr) {
    // SAFETY: as in fail_with_thunk.
    unsafe { T::complete(w.0.cast::<T>()) }
}

/// Calls [`WorkItem::destroy`] of `T` on `w`.
///
/// # Safety
/// As for [`fail_with_thunk`], and the item is not used again.
unsafe fn destroy_thunk<T: WorkItem>(w: WorkItemPtr) {
    // SAFETY: as in fail_with_thunk.
    unsafe { T::destroy(w.0.cast::<T>()) }
}

/// The header every cross-shard work item starts with: a task header, the work-item operation
/// table and the item's service group.
///
/// The task header keeps its own table ([`TaskBase`](crate::engine::core::task::TaskBase)) and
/// this header adds the work item's, so the pointer the queues carry stays one word
/// ([`WorkItemPtr`]).
#[repr(C)]
pub(crate) struct WorkItemBase {
    _task: crate::engine::core::task::TaskBase,
    vtable: &'static WorkItemVTable,
    ssg: SmpServiceGroup,
}

impl WorkItemBase {
    /// Returns the header of a work item of type `T` (whose first field it is) in service group
    /// `ssg`, scheduled in the current scheduling group.
    #[inline]
    pub fn new<T: WorkItem>(ssg: SmpServiceGroup) -> Self {
        Self {
            _task: crate::engine::core::task::TaskBase::new::<T>(
                crate::engine::core::scheduling::current_scheduling_group(),
            ),
            vtable: const { &WorkItemVTable::of::<T>() },
            ssg,
        }
    }
}

/// The operations of a concrete work item type.
///
/// # Safety
/// The implementor is `#[repr(C)]` with a [`WorkItemBase`] built by `WorkItemBase::new::<Self>`
/// as its first field (so its task header is at offset 0 as well, for [`Task`]).
pub(crate) unsafe trait WorkItem: Task {
    /// Fails the caller's promise with `ex`.
    ///
    /// # Safety
    /// `this` is a live item on its origin shard, with no reference to it live.
    unsafe fn fail_with(this: std::ptr::NonNull<Self>, ex: ExceptionPtr);
    /// Completes the caller's promise with the result the target shard stored.
    ///
    /// # Safety
    /// As for [`WorkItem::fail_with`].
    unsafe fn complete(this: std::ptr::NonNull<Self>);
    /// Drops and frees the item.
    ///
    /// # Safety
    /// As for [`WorkItem::fail_with`], and the item is not used again.
    unsafe fn destroy(this: std::ptr::NonNull<Self>);
}

/// A thin pointer to the [`WorkItemBase`] header of a heap-allocated work item, one word, so the
/// queues between shards and the batches hold and copy a single word. It is the owning pointer
/// while the item is queued.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct WorkItemPtr(std::ptr::NonNull<WorkItemBase>);

// SAFETY: a work item crosses to its target shard and back by design: its function and its
// result are `Send` (see `AsyncWorkItem`), its promise is only touched on the origin shard
// (`fail_with`, `complete`, `destroy`), and the queues hand the pointer over with
// release/acquire ordering.
unsafe impl Send for WorkItemPtr {}

impl WorkItemPtr {
    /// Returns the pointer of a concrete heap-allocated item.
    #[inline]
    pub fn from_item<T: WorkItem>(t: std::ptr::NonNull<T>) -> Self {
        WorkItemPtr(t.cast::<WorkItemBase>())
    }

    /// Returns the raw pointer, for prefetching.
    #[inline]
    pub fn as_ptr(self) -> *mut WorkItemBase {
        self.0.as_ptr()
    }

    /// Returns the item as a task (the task header is at offset 0).
    #[inline]
    pub fn task_ptr(self) -> crate::engine::core::task::TaskPtr {
        // SAFETY: the pointer is non-null and the task header starts the item.
        unsafe {
            crate::engine::core::task::TaskPtr::from_raw(
                self.0.as_ptr().cast::<crate::engine::core::task::TaskBase>(),
            )
        }
    }

    /// Returns the item's service group.
    ///
    /// # Safety
    /// The item is live and not being written on another shard.
    #[inline]
    pub unsafe fn ssg(self) -> SmpServiceGroup {
        // SAFETY: by the caller; `ssg` is set at construction and never written again.
        unsafe { (*self.0.as_ptr()).ssg }
    }

    /// Fails the caller's promise with `ex`.
    ///
    /// # Safety
    /// As for [`WorkItem::fail_with`].
    #[inline]
    pub unsafe fn fail_with(self, ex: ExceptionPtr) {
        // SAFETY: by the caller; the table is set at construction.
        unsafe { ((*self.0.as_ptr()).vtable.fail_with)(self, ex) }
    }

    /// Completes the caller's promise.
    ///
    /// # Safety
    /// As for [`WorkItem::complete`].
    #[inline]
    pub unsafe fn complete(self) {
        // SAFETY: as in fail_with.
        unsafe { ((*self.0.as_ptr()).vtable.complete)(self) }
    }

    /// Frees the item.
    ///
    /// # Safety
    /// The item is live, on its origin shard, and not used again.
    #[inline]
    pub unsafe fn destroy(self) {
        // SAFETY: as in fail_with.
        unsafe { ((*self.0.as_ptr()).vtable.destroy)(self) }
    }
}

/// The reactor a ring hands its items to, woken when items are pushed.
pub struct LfQueueRemote {
    /// The receiving shard's reactor.
    pub remote: *mut Reactor,
}

/// One direction of a message queue: the ring and the reactor that consumes it. The reactor
/// pointer comes first so that it is not on the ring's cache lines.
pub struct LfQueue {
    /// The consuming side.
    pub remote: LfQueueRemote,
    /// The ring of work items.
    pub(crate) base: SpscQueue<WorkItemPtr, { SmpMessageQueue::QUEUE_LENGTH }>,
}

impl LfQueue {
    /// Returns an empty ring consumed by `remote`.
    pub fn new(remote: *mut Reactor) -> Self {
        Self { remote: LfQueueRemote { remote }, base: SpscQueue::new() }
    }
    // `maybe_wakeup` and Drop are implemented in the reactor module.
}

/// Sending-side statistics of a message queue: plain counters, written only by the sending
/// shard (no atomics) and read by the queue's metrics (`total_sent_messages`,
/// `total_completed_messages`, `send_batch_queue_length`, `complete_batch_queue_length`,
/// `send_queue_length`).
#[derive(Default)]
pub struct TxStats {
    /// Total requests pushed to the request ring. A request still in the sending side's
    /// pending FIFO, or failed before it was queued, is not counted.
    pub _sent: usize,
    /// Total requests finished on the sending side: completions taken back from the response
    /// ring, plus requests failed before they were queued (no service group unit before the
    /// timeout).
    pub _compl: usize,
    /// Number of requests the last push to the request ring moved (pushes that move none leave
    /// it unchanged).
    pub _last_snt_batch: usize,
    /// Number of completions the last poll of the response ring processed, 0 when it found
    /// none; a request failed before it was queued adds 1 to it.
    pub _last_cmpl_batch: usize,
    /// Requests pushed to the request ring whose completion has not been processed yet
    /// (`_sent` minus the completions taken back): those queued for, running on or returning
    /// from the target shard.
    pub _current_queue_length: usize,
}

/// Receiving-side statistics of a message queue: plain counters, written only by the receiving
/// shard and read by its metrics (`total_received_messages`, `receive_batch_queue_length`).
#[derive(Default)]
pub struct RxStats {
    /// Total requests taken from the request ring and run on the receiving shard.
    pub _received: usize,
    /// Number of requests the last poll of the request ring ran, 0 when it found none.
    pub _last_rcv_batch: usize,
}

/// The work item of one [`Smp::submit_to`]: created on the calling shard, run on the target shard,
/// completed and destroyed back on the calling shard.
///
/// The function runs once, so it is held as `Option<F>` and taken when run: it may consume what
/// it captures. It and the result cross shards, hence `F: Send` and `R::Output: Send`.
///
/// The promise is a [`HostedPromise`] and the item lives in a [`HostBox`]: the promise's node is
/// part of the item, so a message allocates once.
#[repr(C)]
pub struct AsyncWorkItem<F, R>
where
    F: FnOnce() -> R,
    R: Futurize,
{
    // First field: the work item header, which starts with the task header (see `WorkItem`).
    _base: WorkItemBase,
    _queue: *mut SmpMessageQueue,
    _func: Option<F>,
    _result: Option<<R as Futurize>::Output>,
    _ex: Option<ExceptionPtr>, // set when the call failed
    _promise: HostedPromise<<R as Futurize>::Output>, // used on the calling shard
}

// SAFETY: hosted_promise returns the `_promise` field, derived from `this`.
unsafe impl<F, R> PromiseHost for AsyncWorkItem<F, R>
where
    F: FnOnce() -> R,
    R: Futurize,
{
    type Value = <R as Futurize>::Output;
    #[inline]
    unsafe fn hosted_promise(this: *mut Self) -> *mut HostedPromise<Self::Value> {
        // SAFETY: the caller guarantees `this` points at a live item.
        unsafe { &raw mut (*this)._promise }
    }
}

impl<F, R> AsyncWorkItem<F, R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Futurize + 'static,
    R::Output: Send,
{
    /// Returns the item that runs `func` through `queue` in service group `ssg`.
    pub fn new(queue: *mut SmpMessageQueue, ssg: SmpServiceGroup, func: F) -> Self {
        Self {
            _base: WorkItemBase::new::<Self>(ssg),
            _queue: queue,
            _func: Some(func),
            _result: None,
            _ex: None,
            _promise: HostedPromise::new(),
        }
    }

    /// Returns the future of the item's promise.
    ///
    /// # Safety
    /// `this` must be a live item owned by a [`HostBox`], with no reference to it live.
    #[inline]
    pub unsafe fn get_future(this: *mut Self) -> Future<<R as Futurize>::Output> {
        // SAFETY: guaranteed by the caller; the promise pointer is derived from `this`.
        unsafe { HostedPromise::get_future(Self::hosted_promise(this)) }
    }
}

// SAFETY: AsyncWorkItem is repr(C) with the work item header first, whose first field is the
// task header built by TaskBase::new::<Self> (in WorkItemBase::new::<Self>).
unsafe impl<F, R> Task for AsyncWorkItem<F, R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Futurize + 'static,
    R::Output: Send,
{
    unsafe fn waiting_task(
        _this: std::ptr::NonNull<Self>,
    ) -> Option<crate::engine::core::task::TaskPtr> {
        // Not reported across shards: reading another shard's task without synchronization would
        // be unsound.
        None
    }

    unsafe fn run_and_dispose(this: std::ptr::NonNull<Self>) {
        // respond() below sends the item back to the calling shard, which completes the
        // continuation chain there.
        let this: *mut Self = this.as_ptr();
        // Invariant: the target shard runs a work item exactly once.
        // SAFETY: the work item is live until the origin shard deletes it after complete().
        // The `FnOnce` is consumed by its call, so what it captures is released here, on the
        // target shard (it is `Send`).
        let Some(func) = (unsafe { (*this)._func.take() }) else { return };
        // futurize::invoke of the original turns a throw of the function into a failed
        // future, sent back like any result; a panic here does the same (as the local path
        // of submit_to_with), instead of unwinding through the reactor of this shard, where
        // nothing catches it and the process aborts.
        let invoked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            crate::engine::core::future::futurize_invoke(func)
        }))
        .unwrap_or_else(|e| make_exception_future(ExceptionPtr::from_panic(e)));
        let _ = invoked.then_wrapped(move |f: Future<<R as Futurize>::Output>| {
            // SAFETY: the work item stays alive until the origin shard deletes it after
            // complete(); the target shard alone touches it until respond() hands it back.
            unsafe {
                match f.get() {
                    Ok(v) => (*this)._result = Some(v),
                    Err(ex) => (*this)._ex = Some(ex),
                }
                // The queue outlives every work item in flight; this is its receiving side.
                SmpMessageQueue::respond(
                    (*this)._queue,
                    WorkItemPtr::from_item(std::ptr::NonNull::new_unchecked(this)),
                );
            }
        });
        // The item is not freed here: its creator frees it on the origin shard.
    }
}

// SAFETY: AsyncWorkItem is repr(C) with the header built by WorkItemBase::new::<Self> first.
unsafe impl<F, R> WorkItem for AsyncWorkItem<F, R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Futurize + 'static,
    R::Output: Send,
{
    unsafe fn fail_with(this: std::ptr::NonNull<Self>, ex: ExceptionPtr) {
        // SAFETY: by the caller, the item is live on its origin shard with no reference live;
        // the promise is reached through the raw pointer (its future points into it).
        unsafe { HostedPromise::set_exception(Self::hosted_promise(this.as_ptr()), ex) };
    }

    unsafe fn complete(this: std::ptr::NonNull<Self>) {
        let this = this.as_ptr();
        // SAFETY: as in fail_with; each field is reached through the raw pointer.
        unsafe {
            if let Some(r) = (*this)._result.take() {
                HostedPromise::set_value(Self::hosted_promise(this), r);
            } else {
                // The error was allocated on the target shard and is released here.
                // Invariant: run_and_dispose() stored either a result or an exception before respond().
                if let Some(ex) = (*this)._ex.take() {
                    HostedPromise::set_exception(Self::hosted_promise(this), ex);
                }
            }
        }
    }

    unsafe fn destroy(this: std::ptr::NonNull<Self>) {
        // SAFETY: by the caller; `submit` released the item from its HostBox, taken back once.
        drop(unsafe { HostBox::from_raw(this.as_ptr()) });
    }
}

/// The sending side of a message queue: requests waiting for room in the ring.
pub struct TxSide {
    pub(crate) pending_fifo: VecDeque<WorkItemPtr>,
}

/// The message queue from one shard to another: requests go through `_pending` to the target
/// shard, and the finished items come back through `_completed`.
///
/// Each side is used by its own shard only; requests and responses are batched to amortize the
/// cross-core traffic. Its constructor, its Drop and most of its operations (start, processing of
/// incoming requests and completions, flushing the batches, polling) are implemented in the
/// reactor module.
pub struct SmpMessageQueue {
    pub(crate) _pending: LfQueue,
    pub(crate) _completed: LfQueue,
    pub(crate) tx_stats: CacheLineAligned<TxStats>,
    // Kept between the two statistics blocks so that at least one cache line separates them,
    // which stops the hardware prefetcher from pulling in a line used by another CPU.
    pub(crate) _metrics: metrics::MetricGroups,
    pub(crate) rx_stats: CacheLineAligned<RxStats>,
    // The sending side, set up by start().
    pub(crate) _tx: Option<TxSide>,
    pub(crate) _completed_fifo: Vec<WorkItemPtr>,
}

impl SmpMessageQueue {
    /// Capacity of each ring.
    pub const QUEUE_LENGTH: usize = 128;
    /// How many items ahead are prefetched while processing a batch.
    pub const PREFETCH_CNT: usize = 2;

    /// Sends `func` to shard `t` and returns the future of its result, on the sending side.
    ///
    /// Allocates one work item; waits on the service group's semaphore of the target, up to
    /// `options.timeout`, before the item is queued.
    ///
    /// # Safety
    /// `this` is a started queue whose sending side is the current shard (`_qs[t][this
    /// shard]`). The queue is used by two shards at once, each on its own side, so its
    /// methods take `*mut Self` and touch only their side's fields.
    pub unsafe fn submit<F, R>(
        this: *mut Self,
        t: ShardId,
        options: SmpSubmitToOptions,
        func: F,
    ) -> Future<<R as Futurize>::Output>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Futurize + 'static,
        R::Output: Send,
    {
        let _guard =
            crate::engine::util::critical_alloc_section::memory::ScopedCriticalAllocSection::new();
        let wi =
            HostBox::into_raw(HostBox::new(AsyncWorkItem::new(this, options.service_group, func)));
        // SAFETY: `wi` was just allocated and is not shared yet; the sending side is this
        // shard's by the caller.
        unsafe {
            let fut = AsyncWorkItem::get_future(wi);
            Self::submit_item(
                this,
                t,
                options.timeout,
                WorkItemPtr::from_item(std::ptr::NonNull::new_unchecked(wi)),
            );
            fut
        }
    }

    // The other operations (start, process_queue, process_incoming, process_completions, stop,
    // work, submit_item, respond, move_pending, the batch flushes and the polls) are implemented
    // in the reactor module.
}

/// Marker type of the owner of the message queue matrix; the queues are freed by `Smp::cleanup`.
pub struct QsDeleter;

/// The multicore runtime: the reactor threads, the message queues between them, the
/// pre-faulter and the shard-to-NUMA-node map.
///
/// It is shared (`Arc`) by the application and every reactor thread. The members that
/// `configure` and the reactor setup write after it is shared are cells with one writer at a
/// time: `configure` writes them on the main thread before it starts the reactor threads that read
/// them (or, for `_threads`, `_thread_loops` and `_prefaulter`, while those threads use only other
/// members), `join_all` runs on shard 0 alone and `cleanup` after every reactor thread is joined.
/// Each access through a cell is justified where it happens, in the reactor module.
pub struct Smp {
    pub(crate) _alien: *mut alien::Instance,
    pub(crate) _threads: UnsafeCell<Vec<PosixThread>>,
    pub(crate) _thread_loops: UnsafeCell<Vec<Box<dyn Fn() + Send>>>, // for dpdk
    pub(crate) _all_event_loops_done: UnsafeCell<Option<std::sync::Barrier>>,
    pub(crate) _prefaulter: UnsafeCell<Option<MemoryPrefaulterPtr>>,
    // Owner of the queue matrix: one row per target shard, one queue per sending shard.
    pub(crate) _qs_owner: UnsafeCell<Option<Box<[*mut SmpMessageQueue]>>>,
    pub(crate) _using_dpdk: Cell<bool>,
    pub(crate) _shard_to_numa_node_mapping: UnsafeCell<Vec<u32>>,
}

// SAFETY: the cells follow the single-writer contract documented on the type; the alien
// instance is shared by `&` between the shards.
unsafe impl Send for Smp {}
// SAFETY: as above.
unsafe impl Sync for Smp {}

thread_local! {
    /// The queue matrix, indexed `[target][sender]`; set on every reactor thread at start-up.
    pub(crate) static SMP_QS: std::cell::Cell<*mut *mut SmpMessageQueue> = const { std::cell::Cell::new(std::ptr::null_mut()) };
    /// The `pthread_t` of the main thread, recorded by `configure`.
    pub(crate) static SMP_TMAIN: std::cell::Cell<Option<std::ffi::c_ulong>> = const { std::cell::Cell::new(None) };
}

/// Returns the `pthread_t` of the calling thread.
#[inline]
pub(crate) fn current_thread_id() -> std::ffi::c_ulong {
    // SAFETY: pthread_self has no preconditions.
    unsafe { crate::engine::core::reactor::pthread_self() as std::ffi::c_ulong }
}

/// The number of shards, set once by `configure`; read with [`Smp::count`].
pub static SMP_COUNT: AtomicU32 = AtomicU32::new(0);

impl Smp {
    /// Returns the number of shards (0 before the shards are configured).
    #[inline]
    pub fn count() -> u32 {
        SMP_COUNT.load(Ordering::Relaxed)
    }

    /// Returns whether the calling thread is the one that configured the shards.
    pub fn main_thread() -> bool {
        SMP_TMAIN.with(|t| t.get()) == Some(current_thread_id())
    }

    /// Returns, for each shard in order, the id of its NUMA node.
    pub fn shard_to_numa_node_mapping(&self) -> &[u32] {
        // SAFETY: written only by configure() before the reactors run and by cleanup() after
        // they are joined.
        unsafe { &*self._shard_to_numa_node_mapping.get() }
    }

    /// Returns the queue from the current shard to shard `t`.
    fn remote_queue(t: u32) -> *mut SmpMessageQueue {
        // SAFETY: _qs is set up by start_all_queues() for every shard before any submit_to.
        unsafe { (*SMP_QS.with(|q| q.get()).add(t as usize)).add(this_shard_id() as usize) }
    }

    /// Runs `func` on shard `t` (which may be the current one) and returns a future of its result
    /// on the calling shard.
    ///
    /// If `func` returns a plain value it is wrapped in a ready future. On the current shard
    /// `func` runs at once, and a panic in it becomes a failed future. On another shard it runs as
    /// a task there, limited by the service group of `options` (whose timeout bounds the wait for
    /// the group, not the run). The function and its result cross shards, so both must be `Send`;
    /// wrap non-`Send` pointers in a [`ForeignPtr`](crate::engine::core::sharded::ForeignPtr).
    pub fn submit_to_with<F, R>(
        t: u32,
        options: impl Into<SmpSubmitToOptions>,
        func: F,
    ) -> Future<<R as Futurize>::Output>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Futurize + 'static,
        R::Output: Send,
    {
        let options = options.into();
        if t == this_shard_id() {
            // The call consumes `func`, and whatever the returned future needs was moved into
            // it, so nothing has to be kept alive across the call.
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                crate::engine::core::future::futurize_invoke(func)
            }));
            match r {
                Ok(f) => f,
                // Return a failed future rather than unwinding, so callers handle one path.
                Err(e) => make_exception_future(ExceptionPtr::from_panic(e)),
            }
        } else {
            // SAFETY: see remote_queue(); `_qs[t][this shard]` is sent by this shard.
            unsafe { SmpMessageQueue::submit(Self::remote_queue(t), t, options, func) }
        }
    }

    /// Like [`submit_to_with`](Self::submit_to_with), in the default service group with no
    /// timeout.
    pub fn submit_to<F, R>(t: u32, func: F) -> Future<<R as Futurize>::Output>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Futurize + 'static,
        R::Output: Send,
    {
        Self::submit_to_with(t, default_smp_service_group(), func)
    }

    // `poll_queues` and `pure_poll_queues` are implemented in the reactor module.

    /// Returns the range of all shard ids.
    pub fn all_cpus() -> std::ops::Range<u32> {
        0..Self::count()
    }

    /// Runs a clone of `func` on every shard, with `options` for each submission.
    ///
    /// `func` returns nothing or a future of nothing. The returned future resolves when every
    /// invocation has finished, and fails if any failed.
    pub fn invoke_on_all_with<F, R>(options: SmpSubmitToOptions, func: F) -> Future<()>
    where
        F: FnOnce() -> R + Clone + Send + 'static,
        R: Futurize<Output = ()> + 'static,
    {
        parallel_for_each(Self::all_cpus(), move |id: u32| {
            Smp::submit_to_with(id, options, func.clone())
        })
    }

    /// Like [`invoke_on_all_with`](Self::invoke_on_all_with), with the default options.
    pub fn invoke_on_all<F, R>(func: F) -> Future<()>
    where
        F: FnOnce() -> R + Clone + Send + 'static,
        R: Futurize<Output = ()> + 'static,
    {
        Self::invoke_on_all_with(SmpSubmitToOptions::default(), func)
    }

    /// Runs a clone of `func` on every shard except `cpu_id`, with `options` for each submission.
    ///
    /// The returned future resolves when every invocation has finished.
    pub fn invoke_on_others_with<F, R>(
        cpu_id: u32,
        options: SmpSubmitToOptions,
        func: F,
    ) -> Future<()>
    where
        F: FnOnce() -> R + Clone + Send + 'static,
        R: Futurize<Output = ()> + 'static,
    {
        parallel_for_each(Self::all_cpus(), move |id: u32| {
            if id != cpu_id {
                Smp::submit_to_with(id, options, func.clone())
            } else {
                make_ready_future(())
            }
        })
    }

    /// Like [`invoke_on_others_with`](Self::invoke_on_others_with), with the default options.
    pub fn invoke_on_others_of<F, R>(cpu_id: u32, func: F) -> Future<()>
    where
        F: FnOnce() -> R + Clone + Send + 'static,
        R: Futurize<Output = ()> + 'static,
    {
        Self::invoke_on_others_with(cpu_id, SmpSubmitToOptions::default(), func)
    }

    /// Runs a clone of `func` on every shard except the current one, with the default options.
    pub fn invoke_on_others<F, R>(func: F) -> Future<()>
    where
        F: FnOnce() -> R + Clone + Send + 'static,
        R: Futurize<Output = ()> + 'static,
    {
        Self::invoke_on_others_of(this_shard_id(), func)
    }

    // Start-up and teardown (start_all_queues, pin, allocate_reactor, create_thread, the AIO
    // control block accounting, configure, cleanup, cleanup_cpu, arrive_at_event_loop_end,
    // join_all) are implemented in the reactor module.
}

/// Returns a pointer to this thread's shard id, in builds where the id is kept here.
#[cfg(engine_build_shared_libs)]
pub fn this_shard_id_ptr() -> *mut ShardId {
    thread_local! {
        static G_THIS_SHARD_ID: std::cell::UnsafeCell<ShardId> = const { std::cell::UnsafeCell::new(0) };
    }
    G_THIS_SHARD_ID.with(|s| s.get())
}

/// Runs a received work item: schedules it as a task of the current (target) shard.
///
/// # Safety
/// `this` is a live item popped from a queue whose receiving side is this shard.
#[inline]
pub(crate) unsafe fn work_item_process(this: WorkItemPtr) {
    schedule(this.task_ptr());
}

/// One service group on one shard: a semaphore per target shard.
#[derive(Default)]
struct SmpServiceGroupImpl {
    clients: Vec<SmpServiceGroupSemaphore>, // one per target shard
    #[cfg(engine_debug)]
    version: u32,
}

thread_local! {
    static SMP_SERVICE_GROUP_MANAGEMENT_SEM: std::cell::UnsafeCell<SmpServiceGroupSemaphore> =
        std::cell::UnsafeCell::new(SmpServiceGroupSemaphore::with_factory(1,
            NamedSemaphoreExceptionFactory { name: Sstring::from("smp_service_group_management_sem") }));
    static SMP_SERVICE_GROUPS: std::cell::UnsafeCell<Vec<SmpServiceGroupImpl>> =
        const { std::cell::UnsafeCell::new(Vec::new()) };
}

/// Returns this thread's semaphore that serializes service group creation and destruction; it
/// lives as long as the thread.
#[inline]
fn smp_service_group_management_sem() -> *mut SmpServiceGroupSemaphore {
    SMP_SERVICE_GROUP_MANAGEMENT_SEM.with(|s| s.get())
}

/// Returns this thread's vector of service groups, which lives as long as the thread. A reference
/// formed from it must end before any other access to the vector (each use below is one
/// statement or one closure that runs to completion).
#[inline]
fn smp_service_groups() -> *mut Vec<SmpServiceGroupImpl> {
    SMP_SERVICE_GROUPS.with(|g| g.get())
}

/// Returns the error factory of the semaphore of group `id` for calls from `client_cpu` to
/// `this_cpu`, whose messages name the group and both shards.
fn make_service_group_semaphore_exception_factory(
    id: u32,
    client_cpu: ShardId,
    this_cpu: ShardId,
    smp_group_name: Option<Sstring>,
) -> NamedSemaphoreExceptionFactory {
    if let Some(name) = smp_group_name {
        NamedSemaphoreExceptionFactory {
            name: Sstring::from(format!(
                "smp_service_group:'{}' (#{}) {}->{} semaphore",
                name, id, client_cpu, this_cpu
            )),
        }
    } else {
        NamedSemaphoreExceptionFactory {
            name: Sstring::from(format!(
                "smp_service_group#{} {}->{} semaphore",
                id, client_cpu, this_cpu
            )),
        }
    }
}

// The service group and the submission options are plain copyable values.
const _: () = {
    const fn assert_copy<T: Copy>() {}
    assert_copy::<SmpServiceGroup>();
    assert_copy::<SmpSubmitToOptions>();
};

/// Creates a service group with the configuration `ssgc`.
///
/// The group is global: once the returned future resolves, the group can be used on any shard.
/// `max_nonlocal_requests` is raised to at least one per other shard and split evenly among the
/// other shards. Creation is serialized on shard 0.
///
/// # Errors
/// The future fails if the per-shard state cannot be allocated.
pub fn create_smp_service_group(mut ssgc: SmpServiceGroupConfig) -> Future<SmpServiceGroup> {
    ssgc.max_nonlocal_requests = ssgc.max_nonlocal_requests.max(Smp::count() - 1);
    Smp::submit_to(0, move || {
        let body = move || {
            // SAFETY: shard 0's vector; the reference ends with the statement.
            let id = unsafe {
                let g = &*smp_service_groups();
                g.iter().position(|ssgi| ssgi.clients.is_empty()).unwrap_or(g.len())
            };
            parallel_for_each(Smp::all_cpus(), move |cpu: u32| {
                let ssgc = ssgc.clone();
                Smp::submit_to(cpu, move || {
                    // SAFETY: this shard's vector, used only by this closure while it runs.
                    let g = unsafe { &mut *smp_service_groups() };
                    if id >= g.len() {
                        g.resize_with(id + 1, SmpServiceGroupImpl::default); // may fail to allocate
                    }
                    g[id].clients.reserve(Smp::count() as usize); // may fail to allocate
                    let per_client = if Smp::count() > 1 {
                        ssgc.max_nonlocal_requests / (Smp::count() - 1)
                    } else {
                        0
                    };
                    for i in 0..Smp::count() {
                        g[id].clients.push(SmpServiceGroupSemaphore::with_factory(
                            per_client as usize,
                            make_service_group_semaphore_exception_factory(
                                id as u32,
                                i,
                                cpu,
                                ssgc.group_name.clone(),
                            ),
                        ));
                    }
                })
            })
            .handle_exception(move |e: ExceptionPtr| {
                // rollback
                Smp::invoke_on_all(move || {
                    // SAFETY: this shard's vector, used only by this closure while it runs.
                    let g = unsafe { &mut *smp_service_groups() };
                    if g.len() > id {
                        g[id].clients.clear();
                    }
                })
                .then(move |()| -> Future<()> { make_exception_future(e) })
            })
            .then(move |()| {
                #[allow(unused_mut)]
                let mut ret = SmpServiceGroup::new(id as u32);
                #[cfg(engine_debug)]
                {
                    // SAFETY: shard 0's vector; the reference ends with the statement.
                    ret._version = unsafe { (&*smp_service_groups())[id].version };
                }
                Ok::<SmpServiceGroup, ExceptionPtr>(ret)
            })
        };
        // SAFETY: the semaphore is shard 0's thread-local, which outlives the returned future
        // and never moves; no other reference to it is live during the call.
        unsafe { with_semaphore(&mut *smp_service_group_management_sem(), 1, body) }
    })
}

/// Destroys a service group, freeing its resources on every shard.
///
/// The group must not be used again once this function is called.
pub fn destroy_smp_service_group(ssg: SmpServiceGroup) -> Future<()> {
    Smp::submit_to(0, move || {
        let body = move || {
            let id = internal::smp_service_group_id(ssg) as usize;
            // SAFETY: shard 0's vector; the reference ends with the statement.
            let len = unsafe { (&*smp_service_groups()).len() };
            if id >= len {
                on_fatal_internal_error(
                    engine_logger(),
                    &format!("destroy_smp_service_group id={}: out of range", id),
                );
            }
            #[cfg(engine_debug)]
            {
                // SAFETY: as above.
                let cur = unsafe { (&*smp_service_groups())[id].version };
                if ssg._version != cur {
                    on_fatal_internal_error(
                        engine_logger(),
                        &format!(
                            "destroy_smp_service_group id={}: stale version={}: current_version={}",
                            id, ssg._version, cur
                        ),
                    );
                }
            }
            Smp::invoke_on_all(move || {
                // SAFETY: this shard's vector, used only by this closure while it runs.
                let g = unsafe { &mut *smp_service_groups() };
                g[id].clients.clear();
                #[cfg(engine_debug)]
                {
                    g[id].version += 1;
                }
            })
        };
        // SAFETY: as in create_smp_service_group().
        unsafe { with_semaphore(&mut *smp_service_group_management_sem(), 1, body) }
    })
}

/// Sets up the default service group (group 0, unlimited) on shard `cpu`, discarding any groups
/// left from an earlier run. Called on each shard when it starts.
pub fn init_default_smp_service_group(cpu: ShardId) {
    // The default group is group 0, so the vector is expected to be empty here. It is not when
    // reactors are started more than once in a process (tests do), possibly with a different
    // shard count; the old groups are cleared so that no group is left sized for the wrong
    // number of shards.
    // SAFETY: this shard's vector, used only by this function while it runs.
    let g = unsafe { &mut *smp_service_groups() };
    g.clear();
    g.push(SmpServiceGroupImpl::default());
    let ssg0 = g.last_mut().expect("just pushed");
    ssg0.clients.reserve(Smp::count() as usize);
    for i in 0..Smp::count() {
        ssg0.clients.push(SmpServiceGroupSemaphore::with_factory(
            SmpServiceGroupSemaphore::max_counter(),
            make_service_group_semaphore_exception_factory(0, i, cpu, Some("default".into())),
        ));
    }
}

/// Returns the semaphore limiting the requests of group `ssg_id` from shard `t` to this shard.
///
/// # Safety
/// The reference points into this shard's vector of groups: no smp service group may be
/// created or destroyed on this shard (which may reallocate or clear the vector) while it
/// is used, no other reference to the same semaphore may be live, and it must not be sent
/// to another thread.
pub unsafe fn get_smp_service_groups_semaphore<'a>(
    ssg_id: u32,
    t: ShardId,
) -> &'a mut SmpServiceGroupSemaphore {
    // SAFETY: the element lives in the thread-local vector, which by the caller is not
    // resized or cleared while the reference is used, and is not otherwise referenced. The
    // indexes are a live group's id and a shard id, both in range by construction
    // (init_default_smp_service_group and create_smp_service_group size every group for
    // smp::count shards), so the unchecked accesses are in bounds.
    unsafe {
        let g = &mut *smp_service_groups();
        debug_assert!((ssg_id as usize) < g.len());
        let clients = &mut g.get_unchecked_mut(ssg_id as usize).clients;
        debug_assert!((t as usize) < clients.len());
        clients.get_unchecked_mut(t as usize)
    }
}

impl Smp {
    /// Creates the runtime, not yet configured; `alien` must outlive it (alien threads reach the
    /// shards through it).
    pub fn new(alien: &mut alien::Instance) -> Self {
        Self {
            _alien: alien,
            _threads: UnsafeCell::new(Vec::new()),
            _thread_loops: UnsafeCell::new(Vec::new()),
            _all_event_loops_done: UnsafeCell::new(None),
            _prefaulter: UnsafeCell::new(None),
            _qs_owner: UnsafeCell::new(None),
            _using_dpdk: Cell::new(false),
            _shard_to_numa_node_mapping: UnsafeCell::new(Vec::new()),
        }
    }

    /// Starts the memory pre-faulter for the shards' memory (`layout`), unless thread stack guards
    /// are enabled (they make the pre-faulter take hard faults).
    ///
    /// # Errors
    /// Fails if `/proc/meminfo` cannot be read or a worker thread cannot be started.
    ///
    /// Called by configure() on the main thread; the reactor threads never use `_prefaulter`.
    pub(crate) fn setup_prefaulter(
        &self,
        res: &resource::Resources,
        layout: memory::internal::NumaLayout,
    ) -> Result<(), ExceptionPtr> {
        // Stack guards mprotect() pages at random places, so the pre-faulter would hard-fault.
        #[cfg(not(engine_thread_stack_guards))]
        {
            // SAFETY: the alien instance outlives the smp object; it is shared by `&` (alien
            // threads and the prefaulter's workers use it that way).
            let p = internal::MemoryPrefaulter::new(unsafe { &*self._alien }, res, layout)?;
            // SAFETY: the only writer, on the main thread (see the method's documentation).
            unsafe { *self._prefaulter.get() = Some(p) };
        }
        #[cfg(engine_thread_stack_guards)]
        {
            let _ = (res, layout);
        }
        Ok(())
    }
}

/// Returns the huge page size from `/proc/meminfo` (`Hugepagesize:`), or `None` if not listed.
///
/// # Errors
/// Fails if the file cannot be read or the number does not fit in an `i32`.
fn get_huge_page_size() -> Result<Option<usize>, ExceptionPtr> {
    let meminfo_fd = crate::engine::core::posix::FileDesc::open(
        "/proc/meminfo",
        crate::engine::core::reactor::O_RDONLY | crate::engine::core::reactor::O_CLOEXEC,
        0,
    )?;
    let mut meminfo: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    // SAFETY: the buffer is valid for writes of the length passed.
    while let Some(size) = unsafe { meminfo_fd.read(buf.as_mut_ptr().cast(), buf.len()) }? {
        if size == 0 {
            break;
        }
        meminfo.extend_from_slice(&buf[..size]);
    }
    // Find the first "Hugepagesize:" followed by blanks, digits and " kB".
    const KEY: &[u8] = b"Hugepagesize:";
    let mut from = 0;
    while let Some(off) = meminfo[from..].windows(KEY.len()).position(|w| w == KEY) {
        let start = from + off + KEY.len();
        let rest = &meminfo[start..];
        let blanks = rest.iter().take_while(|c| c.is_ascii_whitespace()).count();
        let rest = &rest[blanks..];
        let ndigits = rest.iter().take_while(|c| c.is_ascii_digit()).count();
        if ndigits != 0 && rest[ndigits..].starts_with(b" kB") {
            // The digits are ASCII, so the slice is valid UTF-8.
            let digits = std::str::from_utf8(&rest[..ndigits])
                .map_err(|_| make_exception_ptr(InvalidArgument::new("stoi")))?;
            let v =
                digits.parse::<i32>().map_err(|_| make_exception_ptr(OutOfRange::new("stoi")))?;
            return Ok(Some(v as usize * 1024));
        }
        from = start;
    }
    Ok(None)
}

unsafe extern "C" {
    fn getpagesize() -> std::ffi::c_int;
    fn pthread_setschedparam(
        thread: usize,
        policy: std::ffi::c_int,
        param: *const SchedParam,
    ) -> std::ffi::c_int;
    #[cfg(target_os = "linux")]
    fn madvise(addr: *mut std::ffi::c_void, len: usize, advice: std::ffi::c_int)
    -> std::ffi::c_int;
}

#[repr(C)]
struct SchedParam {
    sched_priority: std::ffi::c_int,
}

const SCHED_IDLE: std::ffi::c_int = 5;
#[cfg(target_os = "linux")]
const MADV_POPULATE_WRITE: std::ffi::c_int = 23;
#[cfg(target_os = "linux")]
const MADV_COLLAPSE: std::ffi::c_int = 25;

impl internal::MemoryPrefaulter {
    /// Starts one idle-priority worker per NUMA node that faults in the node's ranges. The
    /// workers point back at the prefaulter, so it is built in a node that never moves
    /// ([`MemoryPrefaulterPtr`]), whose root pointer the workers keep; the prefaulter is
    /// only reached by places of that pointer while they run (the workers read its atomics,
    /// the owner writes `_worker_threads`), never by a reference to the whole object.
    ///
    /// # Errors
    /// Fails if `/proc/meminfo` cannot be read or a worker thread cannot be started.
    // The object is built at its final heap address, which the threads keep.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        alien: &alien::Instance,
        res: &resource::Resources,
        layout: memory::internal::NumaLayout,
    ) -> Result<MemoryPrefaulterPtr, ExceptionPtr> {
        let mut layout_by_node_id: HashMap<u32, Vec<memory::internal::MemoryRange>> =
            HashMap::new();
        for range in layout.ranges {
            layout_by_node_id.entry(range.numa_node_id).or_default().push(range);
        }
        let owner = MemoryPrefaulterPtr {
            node: crate::engine::net::node_ptr::NodePtr::new(Self {
                _stop_request: AtomicBool::new(false),
                _worker_threads: Vec::new(),
                _layout_by_node_id: layout_by_node_id,
                _active_threads: AtomicU32::new(0),
            }),
        };
        let this = owner.node.as_ptr();
        // SAFETY: getpagesize has no preconditions.
        let page_size = unsafe { getpagesize() } as usize;
        let huge_page_size_opt = get_huge_page_size()?;
        let this_addr = this as usize;
        let alien_addr = alien as *const alien::Instance as usize;
        // The ranges of each node, taken before any worker starts: the map is not changed
        // again until the workers are joined, so the vectors stay where they are.
        // SAFETY: no thread runs yet; the reference to the map ends with the collect.
        let nodes: Vec<(u32, usize)> =
            unsafe { (*std::ptr::addr_of_mut!((*this)._layout_by_node_id)).iter_mut() }
                .map(|(&id, ranges)| {
                    (id, ranges as *mut Vec<memory::internal::MemoryRange> as usize)
                })
                .collect();
        for (numa_node_id, ranges_addr) in nodes {
            let a = crate::engine::core::posix::PosixThreadAttr::default();
            if let Some(cpus) = res.numa_node_id_to_cpuset.get(&numa_node_id) {
                let mut cpuset = resource::CpuSetT::default();
                for &cpu in cpus {
                    cpuset.set(cpu as usize);
                }
                a.set_affinity(cpuset);
            }
            let t = PosixThread::with_attr(
                a,
                Box::new(move || {
                    // The worker only references the two atomic fields (shared, while the owner
                    // keeps writing `_worker_threads`) and its own ranges vector; it never forms a
                    // reference to the whole prefaulter.
                    let this = this_addr as *const Self;
                    // SAFETY: the prefaulter is in its node before the threads start and its owner
                    // joins them before it is freed, so its fields are live; the atomics are only
                    // used through shared references.
                    let (stop_request, active_threads) = unsafe {
                        (
                            &*std::ptr::addr_of!((*this)._stop_request),
                            &*std::ptr::addr_of!((*this)._active_threads),
                        )
                    };
                    // SAFETY: each worker gets its own node's vector, which nothing else touches
                    // until the workers are joined (the map is not modified meanwhile).
                    let ranges =
                        unsafe { &mut *(ranges_addr as *mut Vec<memory::internal::MemoryRange>) };
                    active_threads.fetch_add(1, Ordering::SeqCst);
                    Self::work(stop_request, ranges, page_size, huge_page_size_opt);
                    if active_threads.fetch_sub(1, Ordering::SeqCst) == 1 {
                        // SAFETY: the alien instance outlives the prefaulter, and alien threads
                        // share it by `&`.
                        let alien = unsafe { &*(alien_addr as *const alien::Instance) };
                        alien::run_on(alien, 0, move || {
                            // SAFETY: the prefaulter belongs to shard 0, where this runs as a task;
                            // its owner frees it there too, after this task (the owner joins the
                            // threads again, a no-op then).
                            unsafe { Self::join_threads(this_addr as *mut Self) };
                        });
                    }
                }),
            )?;
            // SAFETY: a place of the root pointer; the workers never touch this field.
            unsafe { (*std::ptr::addr_of_mut!((*this)._worker_threads)).push(t) };
        }
        Ok(owner)
    }

    /// Joins the worker threads and drops the ranges they used.
    ///
    /// # Safety
    /// `this` is the root pointer of a live prefaulter (see `new`); the field accesses are
    /// places of it, so they overlap the workers' reads of the atomics.
    pub(crate) unsafe fn join_threads(this: *mut Self) {
        // SAFETY: the contract above; the workers never touch `_worker_threads`.
        let threads =
            unsafe { std::mem::take(&mut *std::ptr::addr_of_mut!((*this)._worker_threads)) };
        for t in threads {
            t.join();
        }
        // SAFETY: as above; the workers are joined, so nothing uses the ranges any more.
        unsafe { (*std::ptr::addr_of_mut!((*this)._layout_by_node_id)).clear() };
    }

    /// Body of a worker: at idle priority, faults in `ranges` batch by batch (huge-page sized,
    /// alternating from both ends of each range), with `MADV_POPULATE_WRITE` where the kernel
    /// supports it, until done or asked to stop.
    fn work(
        stop_request: &AtomicBool,
        ranges: &mut Vec<memory::internal::MemoryRange>,
        page_size: usize,
        huge_page_size_opt: Option<usize>,
    ) {
        let param = SchedParam { sched_priority: 0 };
        // SCHED_IDLE cannot be set through thread attributes, so it is set here.
        // SAFETY: setting the policy of the calling thread.
        unsafe {
            pthread_setschedparam(crate::engine::core::reactor::pthread_self(), SCHED_IDLE, &param)
        };
        let mut current_range: usize = 0;
        let batch_size = huge_page_size_opt.unwrap_or(512 * 4096);
        let mut works = true;
        let mut populate_memory_madvise = |start: *mut u8, end: *mut u8| -> bool {
            #[cfg(target_os = "linux")]
            {
                if works {
                    // SAFETY: the range belongs to the shard's mapped memory.
                    let result = unsafe {
                        madvise(start as *mut _, end as usize - start as usize, MADV_POPULATE_WRITE)
                    };
                    if result == -1
                        && std::io::Error::last_os_error().raw_os_error()
                            == Some(22 /* EINVAL */)
                    {
                        // Unsupported by kernel
                        works = false;
                        return false;
                    }
                    // Ignore other errors. This is just an optimization anyway.
                    // SAFETY: as above.
                    unsafe {
                        madvise(start as *mut _, end as usize - start as usize, MADV_COLLAPSE)
                    };
                    // Also ignore problems with MADV_COLLAPSE, it's just an optimization.
                    return true;
                }
            }
            let _ = (start, end, &mut works); // suppress warning if block above is elided
            false
        };
        let fault_in_memory = |p: *mut u8| {
            // Touch the page for write, but be sure not to modify anything
            // The compilers tend to optimize things, so prefer assembly
            #[cfg(target_arch = "x86_64")]
            // SAFETY: `p` is a byte of mapped shard memory; `lock orb $0` rewrites it with its
            // own value atomically, outside the language's memory model, so it races with
            // nothing the shard does.
            unsafe {
                std::arch::asm!("lock orb $0, ({0})", in(reg) p, options(nostack, att_syntax));
            }
            #[cfg(target_arch = "aarch64")]
            // SAFETY: as above, with an exclusive load/store pair that writes back the byte
            // read, retried until the store succeeds.
            unsafe {
                std::arch::asm!(
                    "2:",
                    "ldxrb {t:w}, [{p}]",
                    "stxrb {s:w}, {t:w}, [{p}]",
                    "cbnz {s:w}, 2b",
                    p = in(reg) p, t = out(reg) _, s = out(reg) _,
                    options(nostack),
                );
            }
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            // SAFETY: `p` is a byte of mapped shard memory, valid for reads and writes; the byte
            // read is written back. Unlike the sequences above this is not atomic: it is only the
            // fallback for architectures that have no such sequence here.
            unsafe {
                p.write_volatile(p.read_volatile());
            }
        };
        let mut populate_memory = |mut start: *mut u8, end: *mut u8| {
            if populate_memory_madvise(start, end) {
                return;
            }
            while start < end {
                fault_in_memory(start);
                start = start.wrapping_add(page_size);
            }
        };
        while !stop_request.load(Ordering::Relaxed) && !ranges.is_empty() {
            let range = &mut ranges[current_range];

            let batch_end =
                crate::engine::core::align::align_up_ptr(range.start.wrapping_add(1), batch_size)
                    .min(range.end);
            populate_memory(range.start, batch_end);
            range.start = batch_end;

            // An end-to-start scan for applications that manage two heaps that
            // grow towards each other.
            let batch_start =
                crate::engine::core::align::align_down_ptr(range.end.wrapping_sub(1), batch_size)
                    .max(range.start);
            populate_memory(batch_start, range.end);
            range.end = batch_start;

            if range.start >= range.end {
                ranges.remove(current_range);
                current_range = 0;
            }
            current_range += 1;
            if current_range >= ranges.len() {
                current_range = 0;
            }
        }
    }
}

/// The owner of a [`internal::MemoryPrefaulter`], kept in a heap node that never moves; dropping it
/// stops and joins the workers through the root pointer before the node is freed.
pub struct MemoryPrefaulterPtr {
    node: crate::engine::net::node_ptr::NodePtr<internal::MemoryPrefaulter>,
}

impl Drop for MemoryPrefaulterPtr {
    fn drop(&mut self) {
        let this = self.node.as_ptr();
        // SAFETY: the node's root pointer; the flag is an atomic, reached by a shared place.
        unsafe { (*std::ptr::addr_of!((*this)._stop_request)).store(true, Ordering::Relaxed) };
        // SAFETY: as above.
        unsafe { internal::MemoryPrefaulter::join_threads(this) };
        // The node is freed after this, with the workers joined.
    }
}
