//! The per-core reactor: the event loop that runs on every shard.
//!
//! Each shard is one thread pinned to a CPU, with its own [`Reactor`]; shards share nothing
//! and talk through message queues (see `core::smp`). The reactor owns the shard's task
//! queues (one per scheduling group, scheduled by virtual runtime), its timers (steady,
//! low-resolution and manual clocks), its pollers, its I/O queues and sink, its signal table,
//! its syscall thread and the I/O backend ([`ReactorBackend`]) that talks to the kernel.
//!
//! The main loop runs tasks until the task quota expires ([`need_preempt`](crate::engine::core::preempt::need_preempt)), then polls every
//! poller (I/O completions, timers, inter-shard queues, signals); when nothing is runnable and
//! every poller agrees, it sleeps in the backend until the kernel or another shard wakes it.
//! [`engine`] returns the current thread's reactor; it is reached as a raw pointer for one
//! call at a time, because everything the reactor runs may re-enter it.
//!
//! The module also holds the process-level glue around the reactors: start-up and
//! configuration of the shards from the options, signal and stall handling, backtrace
//! printing, and the implementation of operations declared by sibling modules (descriptor
//! operations, file system calls, process spawning).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use crate::engine::core::alien;
use crate::engine::core::api::FollowSymlink;
use crate::engine::core::cacheline::CACHE_LINE_SIZE;
use crate::engine::core::circular_buffer::CircularBuffer;
use crate::engine::core::circular_buffer_fixed_capacity::CircularBufferFixedCapacity;
use crate::engine::core::condition_variable::ConditionVariable;
use crate::engine::core::file::{
    AccessFlags, DirectoryEntryType, File, FileOpenOptions, FsType, GroupDetails, StatData,
};
use crate::engine::core::file_impl::FileImpl;
use crate::engine::core::file_types::{FilePermissions, OpenFlags};
use crate::engine::core::future::{
    ExceptionPtr, Future, Promise, futurize_invoke, make_exception_future, make_ready_future,
};
use crate::engine::core::gate::Gate;
use crate::engine::core::idle_cpu_handler::{
    IdleCpuHandler, IdleCpuHandlerResult, WorkWaitingOnReactor,
};
use crate::engine::core::internal::buffer_allocator::BufferAllocator;
use crate::engine::core::internal::estimated_histogram::TimeEstimatedHistogram;
use crate::engine::core::internal::io_desc::KernelCompletion;
use crate::engine::core::internal::io_request::IoRequest;
use crate::engine::core::internal::io_sink::IoSink;
use crate::engine::core::internal::poll::{Poller, Pollfn};
use crate::engine::core::internal::pollable_fd::{PollableFd, PollableFdState, Speculation};
use crate::engine::core::internal::stall_detector::{CpuStallDetector, CpuStallDetectorConfig};
use crate::engine::core::io_queue::{IoGroup, IoQueue};
use crate::engine::core::iostream::BatchFlushListT;
use crate::engine::core::lowres_clock::LowresClock;
use crate::engine::core::make_task::{LambdaTask, make_task, make_task_ptr};
use crate::engine::core::manual_clock::ManualClock;
use crate::engine::core::memory;
use crate::engine::core::metrics;
use crate::engine::core::posix::{FileDesc, PosixThread, throw_system_error_on};
use crate::engine::core::preempt::internal::PreemptionMonitor;
/// The backend interface and selector.
pub use crate::engine::core::reactor_backend::{ReactorBackend, ReactorBackendSelector};
/// The reactor configuration.
pub use crate::engine::core::reactor_config::{ReactorConfig, ReactorOptions};
use crate::engine::core::scheduling::{
    SchedulingGroup, SchedulingGroupKey, SchedulingGroupKeyConfig, current_scheduling_group,
    default_scheduling_group, max_scheduling_groups,
};
use crate::engine::core::scheduling_specific::internal::SchedulingGroupSpecificThreadLocalData;
use crate::engine::core::semaphore::Semaphore;
use crate::engine::core::shared_mutex::SharedMutex;
use crate::engine::core::shared_ptr::SharedPtr;
use crate::engine::core::smp::Smp;
use crate::engine::core::sstring::Sstring;
use crate::engine::core::task::TaskPtr;
use crate::engine::core::temporary_buffer::TemporaryBuffer;
use crate::engine::core::thread_pool::ThreadPool;
use crate::engine::core::timer::{
    Clock as _, SteadyClockType, SteadyTimePoint, Timer, TimerClock, TimerImpl, TimerListT,
    TimerSetT,
};
use crate::engine::core::timer_set::TimerList;
use crate::engine::core::timer_set::{TimerSet, TimerSetEntry};
use crate::engine::net::api::{
    ConnectedSocket, ListenOptions, NetworkStack, ServerSocket, SocketAddress, Transport,
};
use crate::engine::net::packet::Packet;
use crate::engine::util::log::Logger;
use crate::engine::util::noncopyable_function::NoncopyableFunction;
use std::pin::Pin;
use std::ptr::NonNull;

use crate::engine::core::shard_id::ShardId;

/// Hash of an IPv4 socket address (address and port).
pub fn hash_sockaddr_in(a: libc_sockaddr_in) -> usize {
    (a.sin_port as usize) ^ (a.sin_addr.s_addr as usize)
}

/// Whether two IPv4 socket addresses have the same family, port and address.
pub fn sockaddr_in_eq(a: &libc_sockaddr_in, b: &libc_sockaddr_in) -> bool {
    a.sin_addr.s_addr == b.sin_addr.s_addr && a.sin_port == b.sin_port
}

/// Layout of `struct in_addr`.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct libc_in_addr {
    /// The address, in network byte order.
    pub s_addr: u32,
}

/// Layout of `struct sockaddr_in`.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct libc_sockaddr_in {
    /// `AF_INET`.
    pub sin_family: u16,
    /// The port, in network byte order.
    pub sin_port: u16,
    /// The address.
    pub sin_addr: libc_in_addr,
    /// Padding.
    pub sin_zero: [u8; 8],
}

/// Internal reactor hooks used by sibling modules.
pub mod internal {
    use super::*;

    /// Logs an exception thrown by a timer callback.
    pub use super::internal_misc::log_timer_callback_exception;

    /// The number of scheduling groups created so far.
    pub fn scheduling_group_count() -> usize {
        super::scheduling_group_count_impl()
    }

    /// Counts an exception thrown (for the exception metrics).
    #[cfg(target_os = "linux")]
    pub use crate::engine::core::exception_hacks::increase_thrown_exceptions_counter;

    /// Runs `func` when the reactor is destroyed, after all other tasks.
    pub fn at_destroy<Func: FnOnce() + 'static>(func: Func) {
        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
        unsafe { (*engine()).do_at_destroy(func) };
    }

    /// Runs `func` (which returns a future) when the reactor exits, before it is destroyed.
    pub fn at_exit(func: NoncopyableFunction<fn() -> Future<()>>) {
        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
        unsafe { (*engine()).do_at_exit(func) };
    }
}
/// The functions that [`IoCompletion`] adds to [`KernelCompletion`].
/// The virtual functions that `io_completion` adds to `kernel_completion`.
pub struct IoCompletionVtable {
    /// Delivers a successful result (a byte count).
    pub complete: unsafe fn(this: *mut IoCompletion, res: usize),
    /// Delivers a failure.
    pub set_exception: unsafe fn(this: *mut IoCompletion, eptr: ExceptionPtr),
}

/// The base of every I/O completion: an object completed by the kernel with a byte count or
/// an error.
///
/// Laid out as a [`KernelCompletion`] header (whose `complete_with` is
/// [`IoCompletion::complete_with`]) followed by the table of the two functions of
/// [`IoCompletionVtable`].
/// A derived type is `#[repr(C)]` with an `IoCompletion` at offset 0 and implements
/// [`IoCompletionImpl`].
#[repr(C)]
pub struct IoCompletion {
    base: KernelCompletion,
    vtable: &'static IoCompletionVtable,
}

/// Implemented by every type that derives from [`IoCompletion`].
///
/// # Safety
///
/// The implementing type must be `#[repr(C)]` with an `IoCompletion` built with
/// [`IoCompletion::new::<Self>()`] at offset 0.
pub unsafe trait IoCompletionImpl: Sized {
    /// Delivers a successful result `res`; may destroy the object.
    ///
    /// # Safety
    /// `this` points to a live object, with no reference to it live during the call.
    unsafe fn complete(this: *mut Self, res: usize);
    /// Delivers the failure `eptr`; may destroy the object.
    ///
    /// # Safety
    /// As [`IoCompletionImpl::complete`].
    unsafe fn set_exception(this: *mut Self, eptr: ExceptionPtr);
}

unsafe fn io_complete_thunk<T: IoCompletionImpl>(this: *mut IoCompletion, res: usize) {
    // SAFETY: by the contract of `IoCompletionImpl`, `this` is offset 0 of a live `T`.
    unsafe { T::complete(this.cast::<T>(), res) }
}

unsafe fn io_set_exception_thunk<T: IoCompletionImpl>(this: *mut IoCompletion, eptr: ExceptionPtr) {
    // SAFETY: as above.
    unsafe { T::set_exception(this.cast::<T>(), eptr) }
}

unsafe fn io_complete_with_thunk(this: *mut KernelCompletion, res: isize) {
    // SAFETY: this vtable is only installed in the header of an `IoCompletion`.
    unsafe { IoCompletion::complete_with(this.cast::<IoCompletion>(), res) }
}

static IO_COMPLETION_KERNEL_VTABLE: crate::engine::core::internal::io_desc::KernelCompletionVtable =
    crate::engine::core::internal::io_desc::KernelCompletionVtable {
        complete_with: io_complete_with_thunk,
    };

struct IoVtableOf<T>(std::marker::PhantomData<T>);

impl<T: IoCompletionImpl> IoVtableOf<T> {
    const VTABLE: IoCompletionVtable = IoCompletionVtable {
        complete: io_complete_thunk::<T>,
        set_exception: io_set_exception_thunk::<T>,
    };
}

impl IoCompletion {
    /// A header for the containing type `T`.
    #[inline]
    pub const fn new<T: IoCompletionImpl>() -> Self {
        IoCompletion {
            base: KernelCompletion::with_vtable(&IO_COMPLETION_KERNEL_VTABLE),
            vtable: &IoVtableOf::<T>::VTABLE,
        }
    }

    /// The derived object as the thin pointer that travels through the io_sink and the kernel.
    #[inline]
    pub fn as_ptr<T: IoCompletionImpl>(obj: &mut T) -> *mut IoCompletion {
        (obj as *mut T).cast::<IoCompletion>()
    }

    /// Delivers a kernel result: `res >= 0` is a success, a negative value is `-errno` and
    /// becomes a system error.
    ///
    /// # Safety
    ///
    /// `this` must point to a live `IoCompletion`; the call may destroy it.
    #[inline]
    pub unsafe fn complete_with(this: *mut IoCompletion, res: isize) {
        // SAFETY: forwarded from the caller.
        unsafe { io_completion_complete_with_impl(this, res) }
    }

    /// Delivers a successful result `res`, dispatched through the vtable.
    ///
    /// # Safety
    ///
    /// `this` must point to a live `IoCompletion`; the call may destroy it.
    #[inline]
    pub unsafe fn complete(this: *mut IoCompletion, res: usize) {
        // SAFETY: forwarded from the caller; the vtable is 'static.
        unsafe { ((*this).vtable.complete)(this, res) }
    }

    /// Delivers the failure `eptr`, dispatched through the vtable.
    ///
    /// # Safety
    ///
    /// `this` must point to a live `IoCompletion`; the call may destroy it.
    #[inline]
    pub unsafe fn set_exception(this: *mut IoCompletion, eptr: ExceptionPtr) {
        // SAFETY: forwarded from the caller; the vtable is 'static.
        unsafe { ((*this).vtable.set_exception)(this, eptr) }
    }

    /// The [`KernelCompletion`] header of `this`.
    #[inline]
    pub fn as_kernel_completion(this: *mut IoCompletion) -> *mut KernelCompletion {
        this.cast::<KernelCompletion>()
    }
}

/// Capacity of the active task queue lists: the number of scheduling groups rounded up to a
/// power of two.
pub const TASK_QUEUE_LIST_CAPACITY: usize = (max_scheduling_groups() as usize).next_power_of_two();
/// A list of task queues, of fixed capacity.
pub type TaskQueueList = CircularBufferFixedCapacity<*mut TaskQueue, TASK_QUEUE_LIST_CAPACITY>;

/// I/O statistics of one shard.
#[derive(Clone, Copy, Default, Debug)]
pub struct IoStats {
    /// linux-aio reads submitted.
    pub aio_reads: u64,
    /// Bytes read with linux-aio.
    pub aio_read_bytes: u64,
    /// linux-aio writes submitted.
    pub aio_writes: u64,
    /// Bytes written with linux-aio.
    pub aio_write_bytes: u64,
    /// Requests whose result was shorter than asked.
    pub aio_outsizes: u64,
    /// Failed requests.
    pub aio_errors: u64,
    /// File stream reads.
    pub fstream_reads: u64,
    /// Bytes read by file streams.
    pub fstream_read_bytes: u64,
    /// File stream reads that had to wait for I/O.
    pub fstream_reads_blocked: u64,
    /// Bytes of file stream reads that had to wait for I/O.
    pub fstream_read_bytes_blocked: u64,
    /// Read-ahead buffers discarded unused.
    pub fstream_read_aheads_discarded: u64,
    /// Bytes of read-ahead buffers discarded unused.
    pub fstream_read_ahead_discarded_bytes: u64,
}

/// Scheduling statistics.
#[derive(Clone, Copy, Default, Debug)]
pub struct SchedStats {
    /// Total number of tasks processed by this shard's reactor until this point.
    /// Note that tasks can be tiny, running for a few nanoseconds, or can take an
    /// entire task quota.
    pub tasks_processed: u64,
}

/// A duration of the scheduler clock: signed nanoseconds, so that differences such as the
/// steal time may be negative.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchedDuration(i64);

impl SchedDuration {
    /// A duration of `ns` nanoseconds.
    #[inline]
    pub const fn from_nanos(ns: i64) -> Self {
        SchedDuration(ns)
    }
    /// Converts a `std::time::Duration` (saturating at the maximum).
    #[inline]
    pub fn from_std(d: Duration) -> Self {
        SchedDuration(i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
    }
    /// The duration as a `std::time::Duration` (zero when negative).
    #[inline]
    pub fn to_std(self) -> Duration {
        Duration::from_nanos(self.0.max(0) as u64)
    }
    /// The duration in nanoseconds.
    #[inline]
    pub const fn count(self) -> i64 {
        self.0
    }
    /// The duration in whole milliseconds.
    #[inline]
    pub const fn as_millis(self) -> i64 {
        self.0 / 1_000_000
    }
    /// The longest duration.
    #[inline]
    pub const fn max() -> Self {
        SchedDuration(i64::MAX)
    }
}

impl std::ops::Add for SchedDuration {
    type Output = Self;
    #[inline]
    fn add(self, o: Self) -> Self {
        SchedDuration(self.0 + o.0)
    }
}
impl std::ops::Sub for SchedDuration {
    type Output = Self;
    #[inline]
    fn sub(self, o: Self) -> Self {
        SchedDuration(self.0 - o.0)
    }
}
impl std::ops::AddAssign for SchedDuration {
    #[inline]
    fn add_assign(&mut self, o: Self) {
        self.0 += o.0;
    }
}
impl std::ops::SubAssign for SchedDuration {
    #[inline]
    fn sub_assign(&mut self, o: Self) {
        self.0 -= o.0;
    }
}
impl std::ops::Mul<i64> for SchedDuration {
    type Output = Self;
    #[inline]
    fn mul(self, k: i64) -> Self {
        SchedDuration(self.0 * k)
    }
}

/// A time point of the scheduler clock: nanoseconds of `CLOCK_MONOTONIC`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchedTimePoint(i64);

impl SchedTimePoint {
    /// The current time.
    #[inline]
    pub fn now() -> Self {
        SchedTimePoint(<SteadyClockType as crate::engine::core::timer::Clock>::to_rep(
            <SteadyClockType as crate::engine::core::timer::Clock>::now(),
        ))
    }
    /// The time since the clock's epoch.
    #[inline]
    pub const fn time_since_epoch(self) -> SchedDuration {
        SchedDuration(self.0)
    }
}

impl std::ops::Sub for SchedTimePoint {
    type Output = SchedDuration;
    #[inline]
    fn sub(self, o: Self) -> SchedDuration {
        SchedDuration(self.0 - o.0)
    }
}
impl std::ops::Add<SchedDuration> for SchedTimePoint {
    type Output = Self;
    #[inline]
    fn add(self, d: SchedDuration) -> Self {
        SchedTimePoint(self.0 + d.0)
    }
}
impl std::ops::AddAssign<SchedDuration> for SchedTimePoint {
    #[inline]
    fn add_assign(&mut self, d: SchedDuration) {
        self.0 += d.0;
    }
}

/// A task queue: the tasks of one scheduling group and their fair-scheduling accounting.
pub struct TaskQueue {
    /// Virtual runtime: the run time scaled by the inverse of the shares.
    pub _vruntime: i64,
    /// The group's shares.
    pub _shares: f32,
    /// `2^32 / shares`, for scaling the run time without a division.
    pub _reciprocal_shares_times_2_power_32: i64,
    /// Whether the queue is in the active list.
    pub _active: bool,
    /// The scheduling group id.
    pub _id: u8,
    /// When the queue last changed state, to compute wait and starve times.
    pub _ts: SchedTimePoint, // to help calculating wait/starve-times
    /// Total run time.
    pub _runtime: SchedDuration,
    /// Total time spent runnable but waiting for other groups.
    pub _waittime: SchedDuration,
    /// Total time spent runnable while the reactor was idle or polling.
    pub _starvetime: SchedDuration,
    /// Tasks run.
    pub _tasks_processed: u64,
    /// The queued tasks.
    pub _q: CircularBuffer<TaskPtr>,
    /// The group's name.
    pub _name: Sstring,
    // The shortened version of the scheduling group's name: only the first 4 characters
    // are used.
    /// The group's short name.
    pub _shortname: Sstring,
    /// Time by which tasks of this queue overran the task quota.
    pub _time_spent_on_task_quota_violations: SchedDuration,
    /// The queue's metrics.
    pub _metrics: metrics::MetricGroups,
}

impl TaskQueue {
    /// Length of the short name.
    pub const SHORTNAME_SIZE: usize = 4;
}

/// A registered signal handler.
pub struct SignalHandler {
    /// The handler.
    pub _handler: NoncopyableFunction<fn()>,
}

/// The signal table of a reactor: pending signals (set by the kernel-level handler) and the
/// handlers to run for them from the poll loop.
pub struct Signals {
    /// Bit `n` is set when signal `n` arrived and was not handled yet.
    pub(crate) _pending_signals: AtomicU64,
    /// The handlers, by signal number.
    pub(crate) _signal_handlers: HashMap<i32, SignalHandler>,
}

/// A function run when the reactor exits.
type ExitFunc = NoncopyableFunction<fn() -> Future<()>>;

#[repr(C)]
/// The reactor of one shard. See the module documentation.
pub struct Reactor {
    /// The shared SMP state.
    pub(crate) _smp: Arc<Smp>,
    /// The instance that lets non-reactor threads send messages to the shards.
    pub(crate) _alien: *mut alien::Instance,
    /// The run-time configuration.
    pub(crate) _cfg: ReactorConfig,
    /// The eventfd other threads write to wake the reactor.
    pub(crate) _notify_eventfd: FileDesc,
    /// The task-quota timer.
    pub(crate) _task_quota_timer: FileDesc,
    /// The I/O backend, in a node whose address is the
    /// backend's root pointer, with its table of entries (read once, see `BackendEntries`).
    pub(crate) _backend: Option<(
        crate::engine::net::node_ptr::NodePtr<dyn ReactorBackend>,
        &'static crate::engine::core::reactor_backend::BackendEntries,
    )>,
    /// The signal mask applied while sleeping.
    pub(crate) _active_sigmask: SigsetT, // holds sigmask while sleeping with sig disabled
    /// The registered pollers.
    pub(crate) _pollers: Vec<*mut dyn Pollfn>,

    // Each mount point is controlled by its own I/O queue, but ...
    /// The I/O queues, by device id.
    pub(crate) _io_queues: HashMap<u64, SharedPtr<IoQueue>>,
    // ... when dispatched all requests get into this single sink
    /// The sink of dispatched requests, drained by the backend.
    pub(crate) _io_sink: IoSink,
    /// Number of I/O groups.
    pub(crate) _num_io_groups: u32,

    /// Functions to run at exit.
    pub(crate) _exit_funcs: Vec<ExitFunc>,
    /// The shard id.
    pub(crate) _id: u32,
    /// Whether the reactor is stopping.
    pub(crate) _stopping: bool,
    /// Whether the reactor has stopped.
    pub(crate) _stopped: bool,
    /// Whether the reactor finished running tasks (later tasks would leak).
    pub(crate) _finished_running_tasks: bool,
    /// Signalled when a stop is requested.
    pub(crate) _stop_requested: ConditionVariable,
    /// The network stack being created.
    pub(crate) _network_stack_ready: Option<Future<Box<dyn NetworkStack>>>,
    /// The exit code of `run()`.
    pub(crate) _return: i32,
    /// Resolved when the reactor has started.
    pub(crate) _start_promise: Promise<()>,
    /// The preemption monitor used while no backend tick drives preemption.
    pub(crate) _preemption_monitor: PreemptionMonitor,
    /// Total tasks processed.
    pub(crate) _global_tasks_processed: u64,
    /// Number of polls.
    pub(crate) _polls: u64,
    /// Histogram of stall durations.
    pub(crate) _stalls_histogram: TimeEstimatedHistogram,
    /// The CPU stall detector.
    pub(crate) _cpu_stall_detector: Option<Box<CpuStallDetector>>,

    /// Armed steady-clock timers.
    pub(crate) _timers: TimerSetT<SteadyClockType>,
    /// Expired steady-clock timers, waiting for their callbacks.
    pub(crate) _expired_timers: TimerListT<SteadyClockType>,
    /// Armed low-resolution timers.
    pub(crate) _lowres_timers: TimerSetT<LowresClock>,
    /// Expired low-resolution timers.
    pub(crate) _expired_lowres_timers: TimerListT<LowresClock>,
    /// Armed manual-clock timers.
    pub(crate) _manual_timers: TimerSetT<ManualClock>,
    /// Expired manual-clock timers.
    pub(crate) _expired_manual_timers: TimerListT<ManualClock>,
    /// I/O statistics.
    pub(crate) _io_stats: IoStats,
    /// Number of `fsync` calls.
    pub(crate) _fsyncs: u64,
    /// Number of exceptions thrown.
    pub(crate) _exceptions_thrown: u64,
    /// Number of failed futures whose error nobody consumed.
    pub(crate) _abandoned_failed_futures: u64,

    /// The task queues, by scheduling group id: each queue in a
    /// node whose pointer is the root of every pointer to it (the run loop's, the active
    /// lists' and `add_task`'s), so that reaching a queue never retags it through a `Box`.
    pub(crate) _task_queues: [Option<crate::engine::net::node_ptr::NodePtr<TaskQueue>>;
        max_scheduling_groups() as usize],
    /// Per-scheduling-group specific values.
    pub(crate) _scheduling_group_specific_data: SchedulingGroupSpecificThreadLocalData,
    /// Guards the scheduling-group key registry.
    pub(crate) _scheduling_group_keys_mutex: SharedMutex,
    /// The virtual runtime of the last task queue run.
    pub(crate) _last_vruntime: i64,
    /// The active task queues, ordered by virtual runtime.
    pub(crate) _active_task_queues: TaskQueueList,
    /// Task queues that became active while tasks ran, merged later.
    pub(crate) _activating_task_queues: TaskQueueList,
    /// The queue of tasks run when the reactor is destroyed.
    pub(crate) _at_destroy_tasks: *mut TaskQueue,
    /// The task currently running.
    pub(crate) _current_task: Option<TaskPtr>,
    /// Handler that will be called when there is no task to execute on cpu.
    /// It represents a low priority work.
    ///
    /// Handler's return value determines whether handler did any actual work. If no work was done then reactor will go
    /// into sleep.
    ///
    /// Handler's argument is a function that returns true if a task which should be executed on cpu appears or false
    /// otherwise. This function should be used by a handler to return early if a task appears.
    pub(crate) _idle_cpu_handler: IdleCpuHandler,
    /// The network stack.
    pub(crate) _network_stack: Option<Box<dyn NetworkStack>>,
    /// When the next low-resolution timer expires.
    pub(crate) _lowres_next_timeout: <LowresClock as crate::engine::core::timer::Clock>::TimePoint,
    /// The eventfd that disk completions signal, when used.
    pub(crate) _aio_eventfd: Option<PollableFd>,
    /// Recent load samples.
    pub(crate) _loads: CircularBuffer<f64>,
    /// The current load.
    pub(crate) _load: f64,
    // Next two fields are required to enforce the monotonicity of total_steal_time()
    // see that method for details.

    // Last measured accumulated steal time, i.e., the simple difference of accumulated
    // awake time and consumed thread CPU time.
    /// Last measured accumulated steal time.
    pub(crate) _last_true_steal: SchedDuration,
    // Accumulated steal time forced to be monotinic by rejecting any updates that would
    // decrease it. See total_steal_time() for details.
    /// Accumulated steal time, kept monotonic.
    pub(crate) _last_mono_steal: SchedDuration,
    /// Total idle time.
    pub(crate) _total_idle: SchedDuration,
    /// While the reactor is idle, the start of the part of the idle period not yet added to
    /// `_total_idle`.
    pub(crate) _idle_start: Option<SchedTimePoint>,
    /// The last time the reactor was seen to be idle. When work turns up, the idle period
    /// ends here.
    pub(crate) _idle_end: SchedTimePoint,
    /// Total sleep time.
    pub(crate) _total_sleep: SchedDuration,
    /// When the reactor started.
    pub(crate) _start_time: SchedTimePoint,
    /// Streams waiting for a batched flush.
    pub(crate) _flush_batching: BatchFlushListT,
    /// Whether the reactor is sleeping (read by other shards to decide on a wake-up).
    pub(crate) _sleeping: CacheAligned<AtomicBool>,
    /// Set when the reactor is being destroyed (read by the timer thread).
    pub(crate) _dying: AtomicBool,
    /// Gate of the background futures.
    pub(crate) _background_gate: Gate,

    /// The signal table.
    pub(crate) _signals: Signals,
    /// The syscall thread.
    pub(crate) _thread_pool: Option<crate::engine::core::thread_pool::ThreadPoolPtr>,
    /// The reactor's metrics.
    pub(crate) _metric_groups: metrics::MetricGroups,
}

/// A value aligned to a cache line (64 bytes).
#[repr(C, align(64))]
pub struct CacheAligned<T>(pub T);

impl<T> std::ops::Deref for CacheAligned<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// The signal set type.
pub use crate::engine::core::internal::linux_abi::SigsetT;

impl Reactor {
    /// Disk requests in flight per I/O queue.
    pub const MAX_AIO_PER_QUEUE: u32 = 128;
    /// Maximum number of I/O queues.
    pub const MAX_QUEUES: u32 = 8;
    /// Maximum disk requests in flight per shard.
    pub const MAX_AIO: u32 = Self::MAX_AIO_PER_QUEUE * Self::MAX_QUEUES;

    /// Obtains an alien::instance object that can be used to send messages
    /// to the shards from other threads.
    pub fn alien(&self) -> &alien::Instance {
        // SAFETY: the instance outlives the reactor and is shared by `&` with the alien
        // threads.
        unsafe { &*self._alien }
    }

    #[inline]
    /// The specific data of scheduling group `sg`.
    pub(crate) fn get_sg_data(
        &mut self,
        sg: &SchedulingGroup,
    ) -> &mut crate::engine::core::scheduling_specific::internal::PerSchedulingGroup {
        &mut self._scheduling_group_specific_data.per_scheduling_group_data[sg.id as usize]
    }

    #[inline]
    /// The specific data of the scheduling group with id `sg_id`.
    pub(crate) fn get_sg_data_by_id(
        &mut self,
        sg_id: u32,
    ) -> &mut crate::engine::core::scheduling_specific::internal::PerSchedulingGroup {
        &mut self._scheduling_group_specific_data.per_scheduling_group_data[sg_id as usize]
    }

    /// @private
    pub fn stopped(&self) -> bool {
        self._stopped
    }
    /// Number of polls so far.
    pub fn polls(&self) -> u64 {
        self._polls
    }

    /// The current scheduler clock time.
    pub fn now() -> SchedTimePoint {
        SchedTimePoint::now()
    }
    /// Time since the reactor started.
    pub fn uptime(&self) -> SchedDuration {
        Self::now() - self._start_time
    }

    /// The syscall thread pool, shared with the syscall thread, so only `&` is given.
    #[inline]
    pub(crate) fn thread_pool(&self) -> &ThreadPool {
        // SAFETY: the constructor creates the pool before the reactor runs; the destructor
        // drops it.
        unsafe { self._thread_pool.as_deref().unwrap_unchecked() }
    }

    /// The queue's root pointer (the one its shared block gives), which the file and the
    /// queued requests keep: never a pointer taken from a reference to the queue.
    pub fn get_io_queue(&self, devid: u64) -> *mut IoQueue {
        let q = match self._io_queues.get(&devid) {
            Some(q) => q,
            // _io_queues.at(0): the queue of the default device always exists.
            None => match self._io_queues.get(&0) {
                Some(q) => q,
                None => crate::engine::util::assert::internal::assert_fail(
                    "_io_queues.at(0)",
                    file!(),
                    line!() as i32,
                    "reactor::get_io_queue",
                ),
            },
        };
        io_queue_ptr(q)
    }

    // FIXME: reuseport currently leads to heavy load imbalance.
    // Until we fix that, just disable it unconditionally.
    /// Whether `SO_REUSEPORT` may be used (currently never).
    pub fn posix_reuseport_available(&self) -> bool {
        false
    }

    /// Whether `pathname` exists.
    pub fn file_exists(&mut self, pathname: &str) -> Future<bool> {
        self.file_accessible(pathname, AccessFlags::EXISTS)
    }

    /// Resolved once the reactor has started.
    pub fn when_started(&mut self) -> Future<()> {
        self._start_promise.get_future()
    }
    /// Resolves once a stop is requested (a termination signal or a call to `exit()`), or
    /// fails after `timeout`.
    pub fn wait_for_stop(&mut self, timeout: Duration) -> Future<()> {
        let this = self as *mut Reactor;
        // SAFETY: the reactor is constructed in place and never moves, so its condition
        // variable is pinned; the reactor outlives every wait on it.
        unsafe {
            std::pin::Pin::new_unchecked(&self._stop_requested)
                .wait_for_pred(timeout, move || (*this)._stopping)
        }
    }

    /// Deprecated: runs `func` when the reactor is destroyed. Use an orderly shutdown with
    /// a deferred action in the application's main function instead.
    #[deprecated(
        note = "Use teseladb::app_template::run(), teseladb::async(), and teseladb::defer() for orderly shutdown"
    )]
    pub fn at_destroy<Func: FnOnce() + 'static>(&mut self, func: Func) {
        self.do_at_destroy(func);
    }

    /// Runs `func` when the reactor is destroyed.
    pub(crate) fn do_at_destroy<Func: FnOnce() + 'static>(&mut self, func: Func) {
        // SAFETY: `_at_destroy_tasks` points to a task queue owned by `_task_queues`.
        unsafe {
            (*self._at_destroy_tasks)._q.push_back(make_task_ptr(default_scheduling_group(), func))
        };
    }

    /// The task currently running.
    pub fn current_task(&self) -> Option<TaskPtr> {
        self._current_task
    }
    /// Sets the current task.
    ///
    /// If a task wants to resume a different task instead of returning control to the
    /// reactor, it should set the current task to the resumed task. In particular, this is
    /// mandatory if the task is going to die before control is returned to the reactor;
    /// otherwise the current task would be left dangling.
    pub fn set_current_task(&mut self, t: Option<TaskPtr>) {
        self._current_task = t;
    }

    /// Runs `func` and the future it returns in the background.
    pub fn run_in_background_fn<Func: FnOnce() -> Future<()> + 'static>(&mut self, func: Func) {
        self.run_in_background(futurize_invoke(func));
    }

    /// Set a handler that will be called when there is no task to execute on cpu.
    /// Handler should do a low priority work.
    ///
    /// Handler's return value determines whether handler did any actual work. If no work was done then reactor will go
    /// into sleep.
    ///
    /// Handler's argument is a function that returns true if a task which should be executed on cpu appears or false
    /// otherwise. This function should be used by a handler to return early if a task appears.
    pub fn set_idle_cpu_handler(&mut self, handler: IdleCpuHandler) {
        self._idle_cpu_handler = handler;
    }

    /// The network stack.
    ///
    /// # Panics
    ///
    /// Panics if the network stack is not ready.
    pub fn net(&mut self) -> &mut dyn NetworkStack {
        self._network_stack.as_deref_mut().unwrap()
    }

    /// The I/O statistics.
    pub fn get_io_stats(&self) -> &IoStats {
        &self._io_stats
    }
    /// Number of failed futures whose error nobody consumed.
    pub fn abandoned_failed_futures(&self) -> u64 {
        self._abandoned_failed_futures
    }
}

/// Test hooks of the reactor.
pub struct ReactorTest;

crate::engine::util::initial_exec_tls::initial_exec_thread_local! {
    /// This thread's reactor, null until it is
    /// constructed. Read by every `schedule()` and every `engine()`, so it is an initial-exec
    /// thread-local (see `util::initial_exec_tls`); all zero bits is the null pointer.
    fn local_engine_slot() -> Cell<*mut Reactor>, symbol = "__engine_rs_reactor_local_engine";
}

/// Sets this thread's reactor.
#[inline]
fn set_local_engine(p: *mut Reactor) {
    // SAFETY: the calling thread's live copy, reached through a Cell for this call only.
    unsafe { (*local_engine_slot()).set(p) };
}

#[inline]
/// This thread's reactor, or null.
pub fn local_engine() -> *mut Reactor {
    // SAFETY: the calling thread's live copy, read through its Cell.
    unsafe { (*local_engine_slot()).get() }
}

/// The reactor of this thread, as a raw pointer.
///
/// The reactor is re-entered from everything it runs (tasks, pollers, timers, signal
/// handlers), so it is never handed out as a long-lived `&mut Reactor`: callers dereference
/// the pointer for one call, `unsafe { (*engine()).add_task(t) }`, and the run loop, the
/// pollers and the backends hold only this pointer while they call out.
///
/// The pointer is valid while the reactor of this thread runs (from its construction in
/// `Smp::configure` until the thread exits); dereferencing it is sound when no reference to
/// the reactor created elsewhere is live across the access.
#[inline]
pub fn engine() -> *mut Reactor {
    let r = local_engine();
    debug_assert!(!r.is_null(), "engine() called on a thread without a reactor");
    r
}

use crate::engine::core::scheduling::internal::set_current_scheduling_group;

/// Whether this thread has a reactor.
#[inline]
pub fn engine_is_ready() -> bool {
    !local_engine().is_null()
}

unsafe extern "C" {
    fn __libc_current_sigrtmin() -> i32;
}

#[inline]
/// The real-time signal of the high resolution timer (`SIGRTMIN`).
pub fn hrtimer_signal() -> i32 {
    // We don't want to use SIGALRM, because unit test libraries may also use it.
    // SAFETY: plain libc call without arguments.
    unsafe { __libc_current_sigrtmin() }
}

// Reactor implementation.
use crate::engine::core::future::SystemError;
use crate::engine::core::posix::{
    RCV_SHUTDOWN, SHUT_RD, SHUT_RDWR, SHUT_WR, SND_SHUTDOWN, shutdown_mask,
};
use crate::engine::core::reactor_backend::{
    EPOLLIN, EPOLLOUT, Iovec, MSG_DONTWAIT, MSG_NOSIGNAL, Msghdr, SO_ERROR, SOCK_CLOEXEC,
    SOCK_NONBLOCK, SOL_SOCKET, SiginfoT, engine_assert, errno,
};

use crate::engine::util::internal::iovec_utils::iovec_len;

/// Evaluates a `Result`, returning the error as a failed future.
macro_rules! fut_try {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return crate::engine::core::future::make_exception_future(e),
        }
    };
}
pub(crate) use fut_try;

const _: () = assert!(shutdown_mask(SHUT_RD) == RCV_SHUTDOWN);
const _: () = assert!(shutdown_mask(SHUT_WR) == SND_SHUTDOWN);
const _: () = assert!(shutdown_mask(SHUT_RDWR) == (SND_SHUTDOWN | RCV_SHUTDOWN));

/// The I/O properties of a disk, read from the I/O properties file or option.
#[derive(Clone, Debug)]
pub struct DiskParams {
    /// The mount points the properties apply to.
    pub mountpoints: Vec<String>,
    /// The device ids the properties apply to.
    pub devices: Vec<u64>,
    /// Read bandwidth in bytes per second.
    pub read_bytes_rate: u64,
    /// Write bandwidth in bytes per second.
    pub write_bytes_rate: u64,
    /// Read requests per second.
    pub read_req_rate: u64,
    /// Write requests per second.
    pub write_req_rate: u64,
    /// Read size at which the disk saturates.
    pub read_saturation_length: u64,
    /// Write size at which the disk saturates.
    pub write_saturation_length: u64,
    /// Whether reads and writes are scheduled together (full duplex).
    pub duplex: bool,
    /// Factor applied to all rates.
    pub rate_factor: f32,
}

impl Default for DiskParams {
    fn default() -> Self {
        DiskParams {
            mountpoints: Vec::new(),
            devices: Vec::new(),
            read_bytes_rate: u64::MAX,
            write_bytes_rate: u64::MAX,
            read_req_rate: u64::MAX,
            write_req_rate: u64::MAX,
            read_saturation_length: u64::MAX,
            write_saturation_length: u64::MAX,
            duplex: false,
            rate_factor: 1.0,
        }
    }
}

pub mod yaml {
    //! A minimal YAML parser for the I/O properties.
    //!
    //! The YAML subset used for the I/O properties: block and flow mappings and sequences,
    //! nested in any order, plain, single- and double-quoted scalars, and null values. A
    //! malformed document is an error of the form `yaml: error at line L, column C: ...`;
    //! a failed conversion is `yaml: error: bad conversion`.

    use crate::engine::core::future::{ExceptionPtr, RuntimeError};

    /// A parsed YAML node.
    #[derive(Clone, Debug, Default, PartialEq)]
    pub enum Node {
        /// A node that is not in the document (a missing key).
        #[default]
        Undefined,
        /// `~`, `null` or an empty value.
        Null,
        /// A scalar, as written.
        Scalar(String),
        /// A sequence.
        Sequence(Vec<Node>),
        /// A mapping, in document order.
        Map(Vec<(String, Node)>),
    }

    static UNDEFINED: Node = Node::Undefined;

    fn bad_conversion() -> ExceptionPtr {
        ExceptionPtr::new(RuntimeError::new("yaml: error: bad conversion".to_string()))
    }

    impl Node {
        /// The value of `key` in a mapping, or an undefined node.
        pub fn get(&self, key: &str) -> &Node {
            if let Node::Map(m) = self {
                for (k, v) in m {
                    if k == key {
                        return v;
                    }
                }
            }
            &UNDEFINED
        }
        /// Whether the node is present in the document.
        pub fn is_defined(&self) -> bool {
            !matches!(self, Node::Undefined)
        }
        /// The scalar value.
        ///
        /// # Errors
        ///
        /// Returns a bad-conversion error if the node is not a scalar.
        pub fn as_string(&self) -> Result<String, ExceptionPtr> {
            match self {
                Node::Scalar(s) => Ok(s.clone()),
                _ => Err(bad_conversion()),
            }
        }
        /// The scalar as a YAML boolean (`true`/`yes`/`on`/`y` or `false`/`no`/`off`/`n`, in
        /// any of their usual capitalizations).
        ///
        /// # Errors
        ///
        /// Returns a bad-conversion error for any other value.
        pub fn as_bool(&self) -> Result<bool, ExceptionPtr> {
            match self.as_string()?.as_str() {
                "true" | "True" | "TRUE" | "yes" | "Yes" | "YES" | "on" | "On" | "ON" | "y"
                | "Y" => Ok(true),
                "false" | "False" | "FALSE" | "no" | "No" | "NO" | "off" | "Off" | "OFF" | "n"
                | "N" => Ok(false),
                _ => Err(bad_conversion()),
            }
        }
        /// The scalar as an `f32`.
        ///
        /// # Errors
        ///
        /// Returns a bad-conversion error if it is not a number.
        pub fn as_f32(&self) -> Result<f32, ExceptionPtr> {
            self.as_string()?.trim().parse::<f32>().map_err(|_| bad_conversion())
        }
        /// A sequence of scalars.
        ///
        /// # Errors
        ///
        /// Returns a bad-conversion error if the node is not a sequence of scalars.
        pub fn as_string_vec(&self) -> Result<Vec<String>, ExceptionPtr> {
            match self {
                Node::Sequence(v) => v.iter().map(|n| n.as_string()).collect(),
                _ => Err(bad_conversion()),
            }
        }
        /// The elements of a sequence.
        ///
        /// # Errors
        ///
        /// Returns a bad-conversion error if the node is not a sequence.
        pub fn as_sequence(&self) -> Result<&Vec<Node>, ExceptionPtr> {
            match self {
                Node::Sequence(v) => Ok(v),
                _ => Err(bad_conversion()),
            }
        }
    }

    /// A parse error at `line` and `column` (0-based; shown 1-based).
    fn parse_error(line: usize, column: usize, msg: &str) -> ExceptionPtr {
        ExceptionPtr::new(RuntimeError::new(format!(
            "yaml: error at line {}, column {}: {}",
            line + 1,
            column + 1,
            msg
        )))
    }

    const END_OF_MAP: &str = "end of map not found";
    const END_OF_SEQ: &str = "end of sequence not found";
    const END_OF_MAP_FLOW: &str = "end of map flow not found";
    const END_OF_SEQ_FLOW: &str = "end of sequence flow not found";
    const UNKNOWN_TOKEN: &str = "unknown token";
    const ILLEGAL_MAP_VALUE: &str = "illegal map value";

    /// One significant line: its number, its indentation and its text without the comment.
    struct Line {
        no: usize,
        indent: usize,
        text: String,
    }

    fn strip_comment(s: &str) -> &str {
        let mut in_q: Option<char> = None;
        for (i, c) in s.char_indices() {
            match (c, in_q) {
                ('\'' | '"', None) => in_q = Some(c),
                (q, Some(o)) if q == o => in_q = None,
                ('#', None) if i == 0 || s[..i].ends_with(' ') || s[..i].ends_with('\t') => {
                    return &s[..i];
                }
                _ => {}
            }
        }
        s
    }

    /// The scalar of plain or quoted text (`~`, `null` and empty are null).
    fn plain_or_quoted(t: &str, line: usize, col: usize) -> Result<Node, ExceptionPtr> {
        let t = t.trim();
        if t.is_empty() || t == "~" || t == "null" || t == "Null" || t == "NULL" {
            return Ok(Node::Null);
        }
        let b = t.as_bytes();
        if b[0] == b'"' || b[0] == b'\'' {
            let mut f = Flow { s: t.as_bytes(), i: 0, line, col };
            let s = f.quoted()?;
            f.ws();
            if f.i != f.s.len() {
                return Err(parse_error(line, col + f.i, UNKNOWN_TOKEN));
            }
            return Ok(Node::Scalar(s));
        }
        Ok(Node::Scalar(t.to_string()))
    }

    /// A flow collection or scalar in `s`, which starts at `col` of line `line`.
    struct Flow<'a> {
        s: &'a [u8],
        i: usize,
        line: usize,
        col: usize,
    }

    impl Flow<'_> {
        fn err(&self, msg: &str) -> ExceptionPtr {
            parse_error(self.line, self.col + self.i, msg)
        }
        fn ws(&mut self) {
            while self.i < self.s.len()
                && (self.s[self.i] == b' ' || self.s[self.i] == b'\t' || self.s[self.i] == b'\n')
            {
                self.i += 1;
            }
        }
        fn peek(&self) -> Option<u8> {
            self.s.get(self.i).copied()
        }
        fn quoted(&mut self) -> Result<String, ExceptionPtr> {
            let q = self.s[self.i];
            let start = self.i;
            self.i += 1;
            let mut out = Vec::new();
            while self.i < self.s.len() {
                let c = self.s[self.i];
                if c == q {
                    if q == b'\'' && self.s.get(self.i + 1) == Some(&b'\'') {
                        out.push(b'\'');
                        self.i += 2;
                        continue;
                    }
                    self.i += 1;
                    return String::from_utf8(out).map_err(|_| self.err(UNKNOWN_TOKEN));
                }
                if q == b'"' && c == b'\\' && self.i + 1 < self.s.len() {
                    let e = self.s[self.i + 1];
                    out.push(match e {
                        b'n' => b'\n',
                        b't' => b'\t',
                        b'0' => 0,
                        b'\\' | b'"' | b'/' => e,
                        _ => return Err(self.err("unknown escape character")),
                    });
                    self.i += 2;
                    continue;
                }
                out.push(c);
                self.i += 1;
            }
            self.i = start;
            Err(self.err("end of string not found"))
        }
        /// A plain scalar inside a flow collection: up to `,`, `]`, `}`, or `:` followed by a
        /// blank or the end of a flow entry.
        fn plain(&mut self) -> Result<Node, ExceptionPtr> {
            let start = self.i;
            while let Some(c) = self.peek() {
                if c == b',' || c == b']' || c == b'}' {
                    break;
                }
                if c == b':'
                    && matches!(
                        self.s.get(self.i + 1),
                        None | Some(b' ' | b'\t' | b'\n' | b',' | b']' | b'}')
                    )
                {
                    break;
                }
                if c == b'[' || c == b'{' {
                    return Err(self.err(UNKNOWN_TOKEN));
                }
                self.i += 1;
            }
            let t =
                std::str::from_utf8(&self.s[start..self.i]).map_err(|_| self.err(UNKNOWN_TOKEN))?;
            plain_or_quoted(t, self.line, self.col + start)
        }
        fn node(&mut self) -> Result<Node, ExceptionPtr> {
            self.ws();
            match self.peek() {
                Some(b'{') => self.map(),
                Some(b'[') => self.seq(),
                Some(b'"' | b'\'') => Ok(Node::Scalar(self.quoted()?)),
                Some(b',' | b']' | b'}') | None => Ok(Node::Null),
                Some(_) => self.plain(),
            }
        }
        fn key(&mut self) -> Result<String, ExceptionPtr> {
            match self.node()? {
                Node::Scalar(k) => Ok(k),
                Node::Null => Ok(String::new()),
                _ => Err(self.err(ILLEGAL_MAP_VALUE)),
            }
        }
        fn map(&mut self) -> Result<Node, ExceptionPtr> {
            let open = self.i;
            self.i += 1;
            let mut m = Vec::new();
            loop {
                self.ws();
                match self.peek() {
                    Some(b'}') => {
                        self.i += 1;
                        return Ok(Node::Map(m));
                    }
                    None => {
                        self.i = open;
                        return Err(self.err(END_OF_MAP_FLOW));
                    }
                    _ => {}
                }
                let k = self.key()?;
                self.ws();
                let v = if self.peek() == Some(b':') {
                    self.i += 1;
                    self.node()?
                } else {
                    Node::Null
                };
                m.push((k, v));
                self.ws();
                match self.peek() {
                    Some(b',') => self.i += 1,
                    Some(b'}') => {}
                    _ => {
                        self.i = open;
                        return Err(self.err(END_OF_MAP_FLOW));
                    }
                }
            }
        }
        fn seq(&mut self) -> Result<Node, ExceptionPtr> {
            let open = self.i;
            self.i += 1;
            let mut v = Vec::new();
            loop {
                self.ws();
                match self.peek() {
                    Some(b']') => {
                        self.i += 1;
                        return Ok(Node::Sequence(v));
                    }
                    None => {
                        self.i = open;
                        return Err(self.err(END_OF_SEQ_FLOW));
                    }
                    _ => {}
                }
                let item = self.node()?;
                self.ws();
                // `[a: b]` is a single-pair mapping in a flow sequence.
                let item = if self.peek() == Some(b':') {
                    self.i += 1;
                    let k = match item {
                        Node::Scalar(k) => k,
                        Node::Null => String::new(),
                        _ => return Err(self.err(ILLEGAL_MAP_VALUE)),
                    };
                    Node::Map(vec![(k, self.node()?)])
                } else {
                    item
                };
                v.push(item);
                self.ws();
                match self.peek() {
                    Some(b',') => self.i += 1,
                    Some(b']') => {}
                    _ => {
                        self.i = open;
                        return Err(self.err(END_OF_SEQ_FLOW));
                    }
                }
            }
        }
    }

    /// Whether the flow text opened in `t` is closed (brackets outside quotes balance).
    fn flow_closed(t: &str) -> bool {
        let mut depth = 0i32;
        let mut in_q: Option<char> = None;
        for c in t.chars() {
            match (c, in_q) {
                ('\'' | '"', None) => in_q = Some(c),
                (q, Some(o)) if q == o => in_q = None,
                ('[' | '{', None) => depth += 1,
                (']' | '}', None) => depth -= 1,
                _ => {}
            }
        }
        depth <= 0
    }

    /// The position of the `:` that ends a block mapping key in `t`, if `t` is a key line.
    fn key_end(t: &str) -> Option<usize> {
        let b = t.as_bytes();
        if b.first().is_some_and(|c| matches!(c, b'[' | b'{')) {
            return None;
        }
        let mut in_q: Option<u8> = None;
        for i in 0..b.len() {
            let c = b[i];
            match (c, in_q) {
                (b'\'' | b'"', None) if i == 0 => in_q = Some(c),
                (q, Some(o)) if q == o => in_q = None,
                (b':', None) if i + 1 == b.len() || b[i + 1] == b' ' || b[i + 1] == b'\t' => {
                    return Some(i);
                }
                _ => {}
            }
        }
        None
    }

    struct Parser {
        lines: Vec<Line>,
        pos: usize,
    }

    impl Parser {
        fn at(&self, indent: usize) -> bool {
            self.pos < self.lines.len() && self.lines[self.pos].indent == indent
        }

        /// A value that starts at `col` of the current line with text `t` (already consumed
        /// from the line): flow text may go on in the next, more indented, lines.
        fn inline_value(
            &mut self,
            t: &str,
            line: usize,
            col: usize,
            parent_indent: usize,
        ) -> Result<Node, ExceptionPtr> {
            if t.starts_with('[') || t.starts_with('{') {
                let mut text = t.to_string();
                while !flow_closed(&text)
                    && self.pos < self.lines.len()
                    && self.lines[self.pos].indent > parent_indent
                {
                    text.push('\n');
                    text.push_str(&self.lines[self.pos].text);
                    self.pos += 1;
                }
                let mut f = Flow { s: text.as_bytes(), i: 0, line, col };
                let n = f.node()?;
                f.ws();
                if f.i != f.s.len() {
                    return Err(f.err(UNKNOWN_TOKEN));
                }
                return Ok(n);
            }
            plain_or_quoted(t, line, col)
        }

        /// The value of a key whose text after the `:` is empty.
        fn nested_value(
            &mut self,
            indent: usize,
            allow_seq_at_indent: bool,
        ) -> Result<Node, ExceptionPtr> {
            if self.pos < self.lines.len() {
                let next = self.lines[self.pos].indent;
                let is_seq = self.lines[self.pos].text.starts_with('-');
                if next > indent {
                    return self.block(next);
                }
                if allow_seq_at_indent && next == indent && is_seq {
                    return self.seq(indent);
                }
            }
            Ok(Node::Null)
        }

        fn block(&mut self, indent: usize) -> Result<Node, ExceptionPtr> {
            let l = &self.lines[self.pos];
            if l.text == "-" || l.text.starts_with("- ") || l.text.starts_with("-\t") {
                return self.seq(indent);
            }
            if key_end(&l.text).is_some() {
                return self.map(indent, None);
            }
            let (t, no, col) = (l.text.clone(), l.no, l.indent);
            self.pos += 1;
            let n = self.inline_value(&t, no, col, indent)?;
            if self.pos < self.lines.len() && self.lines[self.pos].indent > indent {
                let l = &self.lines[self.pos];
                return Err(parse_error(l.no, l.indent, ILLEGAL_MAP_VALUE));
            }
            Ok(n)
        }

        /// A block mapping at `indent`; `first` is an entry already read from a `- key: v` line.
        fn map(
            &mut self,
            indent: usize,
            first: Option<(String, Node)>,
        ) -> Result<Node, ExceptionPtr> {
            let mut m: Vec<(String, Node)> = first.into_iter().collect();
            while self.at(indent) {
                let l = &self.lines[self.pos];
                if l.text.starts_with('-') && (l.text.len() == 1 || l.text.as_bytes()[1] == b' ') {
                    break;
                }
                let Some(k_end) = key_end(&l.text) else {
                    return Err(parse_error(l.no, l.indent, END_OF_MAP));
                };
                let (no, text) = (l.no, l.text.clone());
                let key = match plain_or_quoted(&text[..k_end], no, indent)? {
                    Node::Scalar(k) => k,
                    _ => String::new(),
                };
                let rest = text[k_end + 1..].trim_start();
                let rest_col = indent + (text.len() - rest.len());
                self.pos += 1;
                let v = if rest.is_empty() {
                    self.nested_value(indent, true)?
                } else {
                    self.inline_value(rest, no, rest_col, indent)?
                };
                m.push((key, v));
            }
            if self.pos < self.lines.len() && self.lines[self.pos].indent > indent {
                let l = &self.lines[self.pos];
                return Err(parse_error(l.no, l.indent, END_OF_MAP));
            }
            Ok(Node::Map(m))
        }

        fn seq(&mut self, indent: usize) -> Result<Node, ExceptionPtr> {
            let mut v = Vec::new();
            while self.at(indent) {
                let l = &self.lines[self.pos];
                if !(l.text == "-" || l.text.starts_with("- ") || l.text.starts_with("-\t")) {
                    break;
                }
                let (no, text) = (l.no, l.text.clone());
                let rest = text[1..].trim_start();
                let rest_col = indent + (text.len() - rest.len());
                self.pos += 1;
                if rest.is_empty() {
                    v.push(self.nested_value(indent, false)?);
                } else if rest.starts_with("- ") || rest == "-" {
                    // `- - x`: a sequence nested on the dash line.
                    self.lines
                        .insert(self.pos, Line { no, indent: rest_col, text: rest.to_string() });
                    v.push(self.seq(rest_col)?);
                } else if let Some(k_end) = key_end(rest) {
                    // A mapping that starts on the dash line, at the column of its first key.
                    let key = match plain_or_quoted(&rest[..k_end], no, rest_col)? {
                        Node::Scalar(k) => k,
                        _ => String::new(),
                    };
                    let after = rest[k_end + 1..].trim_start();
                    let after_col = rest_col + (rest.len() - after.len());
                    let val = if after.is_empty() {
                        self.nested_value(rest_col, true)?
                    } else {
                        self.inline_value(after, no, after_col, rest_col)?
                    };
                    v.push(self.map(rest_col, Some((key, val)))?);
                } else {
                    v.push(self.inline_value(rest, no, rest_col, indent)?);
                }
            }
            if self.pos < self.lines.len() && self.lines[self.pos].indent > indent {
                let l = &self.lines[self.pos];
                return Err(parse_error(l.no, l.indent, END_OF_SEQ));
            }
            Ok(Node::Sequence(v))
        }
    }

    /// Parses a YAML document.
    ///
    /// # Errors
    ///
    /// Returns a parse error for a malformed document.
    pub fn load(input: &str) -> Result<Node, ExceptionPtr> {
        let mut lines = Vec::new();
        for (no, raw) in input.lines().enumerate() {
            let l = strip_comment(raw).trim_end();
            let t = l.trim_start();
            if t.is_empty() || t == "---" || t == "..." {
                continue;
            }
            if l.starts_with('\t') {
                return Err(parse_error(no, 0, UNKNOWN_TOKEN));
            }
            lines.push(Line { no, indent: l.len() - t.len(), text: t.to_string() });
        }
        if lines.is_empty() {
            return Ok(Node::Null);
        }
        let first = lines[0].indent;
        let mut p = Parser { lines, pos: 0 };
        let n = p.block(first)?;
        if p.pos < p.lines.len() {
            let l = &p.lines[p.pos];
            return Err(parse_error(l.no, l.indent, END_OF_MAP));
        }
        Ok(n)
    }

    /// Reads and parses the YAML file at `path`.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or is malformed.
    pub fn load_file(path: &str) -> Result<Node, ExceptionPtr> {
        let s = std::fs::read_to_string(path).map_err(|_| {
            ExceptionPtr::new(RuntimeError::new(format!("yaml: error: bad file: {}", path)))
        })?;
        load(&s)
    }
}

/// Fills `mp` from one entry of the I/O properties (`mountpoint` or `mountpoints`, `read_iops`,
/// `read_bandwidth`, `write_iops`, `write_bandwidth`, and the optional duplex, rate factor and
/// saturation lengths).
///
/// # Errors
///
/// Returns an error for a missing mount point or rate, or a value of the wrong type.
pub fn decode_disk_params(node: &yaml::Node, mp: &mut DiskParams) -> Result<bool, ExceptionPtr> {
    use crate::engine::util::conversions::parse_memory_size;
    if node.get("mountpoints").is_defined() {
        mp.mountpoints = node.get("mountpoints").as_string_vec()?;
    } else {
        mp.mountpoints.push(node.get("mountpoint").as_string()?);
    }
    mp.read_bytes_rate = parse_memory_size(&node.get("read_bandwidth").as_string()?)? as u64;
    mp.read_req_rate = parse_memory_size(&node.get("read_iops").as_string()?)? as u64;
    mp.write_bytes_rate = parse_memory_size(&node.get("write_bandwidth").as_string()?)? as u64;
    mp.write_req_rate = parse_memory_size(&node.get("write_iops").as_string()?)? as u64;
    if node.get("read_saturation_length").is_defined() {
        mp.read_saturation_length =
            parse_memory_size(&node.get("read_saturation_length").as_string()?)? as u64;
    }
    if node.get("write_saturation_length").is_defined() {
        mp.write_saturation_length =
            parse_memory_size(&node.get("write_saturation_length").as_string()?)? as u64;
    }
    if node.get("duplex").is_defined() {
        mp.duplex = node.get("duplex").as_bool()?;
    }
    if node.get("rate_factor").is_defined() {
        mp.rate_factor = node.get("rate_factor").as_f32()?;
    }
    Ok(true)
}

static ENGINE_LOGGER: std::sync::LazyLock<std::pin::Pin<Box<Logger>>> =
    std::sync::LazyLock::new(|| Logger::new("engine").expect("logger registered once"));

/// The reactor's logger (`engine`).
pub fn engine_logger() -> &'static Logger {
    &ENGINE_LOGGER
}

impl Reactor {
    #[deprecated(note = "Use this_shard_id")]
    /// The shard id. Deprecated: use `this_shard_id()`.
    pub fn cpu_id(&self) -> ShardId {
        engine_assert!(self._id == crate::engine::core::shard_id::this_shard_id());
        self._id
    }

    /// @private
    pub fn update_shares_for_queues(
        &mut self,
        pc: crate::engine::core::io_queue::internal::PriorityClass,
        shares: u32,
    ) {
        for q in self._io_queues.values() {
            // SAFETY: the queue is engaged; `get()` is its root pointer, with no reference to
            // it live during the call. A metric registration error cannot be returned from
            // here: it is logged and the process aborts.
            if let Err(e) = unsafe { IoQueue::update_shares_for_class(io_queue_ptr(q), pc, shares) }
            {
                engine_logger().error(
                    crate::engine::util::log::FormatInfo::new("update_shares_for_queues: {}"),
                    format_args!("update_shares_for_queues: {}", e),
                );
                std::process::abort();
            }
        }
    }

    /// @private
    pub fn update_bandwidth_for_queues(
        &mut self,
        pc: crate::engine::core::io_queue::internal::PriorityClass,
        bandwidth: u64,
    ) -> Future<()> {
        let bandwidth = bandwidth / self._num_io_groups as u64;
        Smp::invoke_on_all(move || {
            // SAFETY: runs as a task on each shard; the map is not changed while the queues
            // update (updating runs no task), and each queue is engaged and reached by its own
            // pointer only here.
            let queues = unsafe { &*std::ptr::addr_of!((*engine())._io_queues) };
            crate::engine::core::loop_::parallel_for_each(
                queues.values(),
                move |queue: &SharedPtr<IoQueue>| {
                    // SAFETY: as above; `get()` is the queue's root pointer.
                    unsafe {
                        IoQueue::update_bandwidth_for_class(io_queue_ptr(queue), pc, bandwidth)
                    }
                },
            )
        })
    }

    /// @private
    pub fn rename_queues(
        &mut self,
        pc: crate::engine::core::io_queue::internal::PriorityClass,
        new_name: Sstring,
    ) {
        for queue in self._io_queues.values_mut() {
            // SAFETY: the queue is engaged and reached by this reference only during the call.
            unsafe { queue.get_mut_unchecked() }.rename_priority_class(pc, new_name.clone());
        }
    }

    // The generic socket operations of the AIO and epoll backends (and `read` of
    // io_uring): `do_accept` and the others. They reach the backend through its
    // table (see `BackendEntries`) and take the descriptor state by its root pointer, which
    // the continuations keep; no reference to the reactor, the backend or the state is held
    // across a backend call.
    //
    // # Safety (all of them)
    // On this shard's reactor thread; `fd` is the root pointer of a live state made by the
    // reactor's backend, which outlives the returned future,
    // with no reference to it live.

    /// Accepts a connection once the listening socket is readable.
    pub(crate) unsafe fn do_accept(
        listenfd: *mut PollableFdState,
    ) -> Future<(PollableFd, SocketAddress)> {
        // SAFETY: the contract above.
        unsafe { Self::readable_or_writeable(listenfd) }.then(move |()| {
            // SAFETY: the listening fd state outlives the accept; the reborrow runs syscalls
            // and ends before the backend is reached.
            let listenfd_ref = unsafe { &mut *listenfd };
            let mut sa = SocketAddress::default();
            fut_try!(listenfd_ref.maybe_no_more_recv());
            let maybe_fd =
                fut_try!(listenfd_ref.fd.try_accept(&mut sa, SOCK_NONBLOCK | SOCK_CLOEXEC));
            let Some(nfd) = maybe_fd else {
                // We speculated that we will have an another connection, but got a false
                // positive. Try again without speculation.
                // SAFETY: as above.
                return unsafe { Self::do_accept(listenfd) };
            };
            // Speculate that there is another connection on this listening socket, to avoid
            // a task-quota delay. Usually this will fail, but accept is a rare-enough operation
            // that it is worth the false positive in order to withstand a connection storm
            // without having to accept at a rate of 1 per task quota.
            listenfd_ref.speculate_epoll(EPOLLIN as i32);
            let pfd = PollableFd::new(nfd, Speculation::new(EPOLLOUT as i32));
            make_ready_future((pfd, sa))
        })
    }

    /// Starts a connection to `sa` and resolves once the socket is connected.
    pub(crate) unsafe fn do_connect(
        pfd: *mut PollableFdState,
        sa: &mut SocketAddress,
    ) -> Future<()> {
        // SAFETY: the contract above; the address passed is at least as long as the length
        // passed.
        fut_try!(unsafe { (*pfd).fd.connect(sa.as_posix_sockaddr(), sa.length()) });
        // SAFETY: the contract above.
        unsafe { Self::writeable(pfd) }.then(move |()| {
            // SAFETY: the fd state outlives the connect.
            let err = fut_try!(unsafe { (*pfd).fd.getsockopt_int(SOL_SOCKET, SO_ERROR) });
            if err != 0 {
                return make_exception_future(ExceptionPtr::from(SystemError::from_errno(err)));
            }
            make_ready_future(())
        })
    }

    /// Reads up to `len` bytes into `buffer`, waiting for readability as needed.
    pub(crate) unsafe fn do_read(
        fd: *mut PollableFdState,
        buffer: *mut std::ffi::c_void,
        len: usize,
    ) -> Future<usize> {
        // SAFETY: the contract above.
        unsafe { Self::readable(fd) }.then(move |()| {
            // SAFETY: the state outlives the read; the buffer is valid for writes of the
            // length passed.
            let r = fut_try!(unsafe { (*fd).fd.read(buffer, len) });
            let Some(r) = r else {
                // SAFETY: as above.
                return unsafe { Self::do_read(fd, buffer, len) };
            };
            if r == len {
                // SAFETY: as above; one call on the state.
                unsafe { (*fd).speculate_epoll(EPOLLIN as i32) };
            }
            make_ready_future(r)
        })
    }

    /// Reads some data into a buffer from `ba`, waiting for readability as needed.
    pub(crate) unsafe fn do_read_some(
        fd: *mut PollableFdState,
        ba: *mut dyn BufferAllocator,
    ) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: the contract above.
        unsafe { Self::readable(fd) }.then(move |()| {
            // SAFETY: the state and the allocator outlive the read.
            let mut buffer = unsafe { (*ba).allocate_buffer() };
            // SAFETY: as above; the buffer is valid for writes of the length passed.
            let r = fut_try!(unsafe {
                (*fd).fd.read(buffer.get_write() as *mut std::ffi::c_void, buffer.size())
            });
            let Some(r) = r else {
                // Speculation failure, try again with real polling this time
                // Note we release the buffer and will reallocate it when poll
                // completes.
                // SAFETY: as above.
                return unsafe { Self::do_read_some(fd, ba) };
            };
            if r == buffer.size() {
                // SAFETY: as above; one call on the state.
                unsafe { (*fd).speculate_epoll(EPOLLIN as i32) };
            }
            // SAFETY: the kernel read at most `buffer.size()` bytes.
            unsafe { buffer.trim_unchecked(r) };
            make_ready_future(buffer)
        })
    }

    // The iovecs are taken as `&Vec`, as the backends' recvmsg that forward to it.
    #[allow(clippy::ptr_arg)]
    /// Receives into the buffers of `iov`, waiting for readability as needed.
    pub(crate) unsafe fn do_recvmsg(fd: *mut PollableFdState, iov: &Vec<Iovec>) -> Future<usize> {
        let mut iov = iov.clone();
        // SAFETY: the contract above.
        unsafe { Self::readable(fd) }.then(move |()| {
            let mut mh =
                Msghdr { msg_iov: iov.as_mut_ptr(), msg_iovlen: iov.len(), ..Msghdr::default() };
            // SAFETY: the state outlives the read; the message header and the buffers it
            // describes are live for the call.
            let r = fut_try!(unsafe { (*fd).fd.recvmsg(&mut mh, 0) });
            let Some(r) = r else {
                // SAFETY: as above.
                return unsafe { Self::do_recvmsg(fd, &iov) };
            };
            if r == iovec_len(&iov) {
                // SAFETY: as above; one call on the state.
                unsafe { (*fd).speculate_epoll(EPOLLIN as i32) };
            }
            make_ready_future(r)
        })
    }

    /// Sends up to `len` bytes of `buffer`, waiting for writability as needed.
    pub(crate) unsafe fn do_send(
        fd: *mut PollableFdState,
        buffer: *const std::ffi::c_void,
        len: usize,
    ) -> Future<usize> {
        // SAFETY: the contract above.
        unsafe { Self::writeable(fd) }.then(move |()| {
            // SAFETY: the state outlives the send; the buffer is valid for reads of the
            // length passed.
            let r = fut_try!(unsafe { (*fd).fd.send(buffer, len, MSG_NOSIGNAL) });
            let Some(r) = r else {
                // SAFETY: as above.
                return unsafe { Self::do_send(fd, buffer, len) };
            };
            if r == len {
                // SAFETY: as above; one call on the state.
                unsafe { (*fd).speculate_epoll(EPOLLOUT as i32) };
            }
            make_ready_future(r)
        })
    }

    /// Sends some of the packet `p`, waiting for writability as needed.
    pub(crate) unsafe fn do_sendmsg(fd: *mut PollableFdState, p: &mut Packet) -> Future<usize> {
        let pp = p as *mut Packet;
        // SAFETY: the contract above.
        unsafe { Self::writeable(fd) }.then(move |()| {
            const _: () = assert!(
                std::mem::size_of::<Iovec>()
                    == std::mem::size_of::<crate::engine::net::packet::Fragment>()
                    && std::mem::align_of::<Iovec>()
                        == std::mem::align_of::<crate::engine::net::packet::Fragment>(),
                "net::fragment and iovec should be equivalent"
            );
            // SAFETY: the packet outlives the send.
            let p = unsafe { &mut *pp };

            let iov = p.fragment_array() as *mut Iovec;
            let mh = Msghdr {
                msg_iov: iov,
                msg_iovlen: (p.nr_frags() as usize).min(IOV_MAX),
                ..Msghdr::default()
            };
            // SAFETY: the state outlives the send; the message header and the buffers it
            // describes are live for the call.
            let r = fut_try!(unsafe { (*fd).fd.sendmsg(&mh, MSG_NOSIGNAL) });
            let Some(r) = r else {
                // SAFETY: as above.
                return unsafe { Self::do_sendmsg(fd, p) };
            };
            if r == p.len() as usize {
                // SAFETY: as above; one call on the state.
                unsafe { (*fd).speculate_epoll(EPOLLOUT as i32) };
            }
            make_ready_future(r)
        })
    }

    unsafe fn send_all_part(
        fd: *mut PollableFdState,
        buffer: *const std::ffi::c_void,
        len: usize,
        completed: usize,
    ) -> Future<()> {
        if completed == len {
            make_ready_future(())
        } else {
            // SAFETY: `buffer` holds `len` bytes.
            let part_ptr =
                unsafe { (buffer as *const u8).add(completed) } as *const std::ffi::c_void;
            // SAFETY: the contract above; the backend is reached through its table.
            let f = unsafe {
                let (b, e) = Self::backend_entries(engine());
                (e.send)(b, fd, part_ptr, len - completed)
            };
            f.then(move |part: usize| {
                // SAFETY: the state outlives the send.
                unsafe { Self::send_all_part(fd, buffer, len, completed + part) }
            })
        }
    }

    /// Receives some data into a buffer from `ba`, waiting for readability as needed.
    pub(crate) unsafe fn do_recv_some(
        fd: *mut PollableFdState,
        ba: *mut dyn BufferAllocator,
    ) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: the contract above.
        unsafe { Self::readable(fd) }.then(move |()| {
            // SAFETY: the state and the allocator outlive the read.
            let mut buffer = unsafe { (*ba).allocate_buffer() };
            // SAFETY: as above; the buffer is valid for writes of the length passed.
            let r = fut_try!(unsafe {
                (*fd).fd.recv(
                    buffer.get_write() as *mut std::ffi::c_void,
                    buffer.size(),
                    MSG_DONTWAIT,
                )
            });
            let Some(r) = r else {
                // SAFETY: as above.
                return unsafe { Self::do_recv_some(fd, ba) };
            };
            if r as usize == buffer.size() {
                // SAFETY: as above; one call on the state.
                unsafe { (*fd).speculate_epoll(EPOLLIN as i32) };
            }
            // SAFETY: the kernel read at most `buffer.size()` bytes.
            unsafe { buffer.trim_unchecked(r as usize) };
            make_ready_future(buffer)
        })
    }

    /// Sends all `len` bytes of `buffer` on `fd`.
    ///
    /// # Safety
    /// As the operations above; `buffer` is valid for reads of `len` bytes and not written
    /// or freed until the returned future resolves.
    pub unsafe fn send_all(
        fd: *mut PollableFdState,
        buffer: *const std::ffi::c_void,
        len: usize,
    ) -> Future<()> {
        engine_assert!(len != 0);
        // SAFETY: forwarded.
        unsafe { Self::send_all_part(fd, buffer, len, 0) }
    }
}

const IOV_MAX: usize = 1024;

/// The backend's root pointer and table, for one call of this thread's reactor.
///
/// # Safety
/// On a reactor thread, with the reactor set.
#[inline]
unsafe fn engine_entries()
-> (*mut (), &'static crate::engine::core::reactor_backend::BackendEntries) {
    // SAFETY: guaranteed by the caller.
    unsafe { Reactor::backend_entries(engine()) }
}

// The operations of a descriptor state, on the state's root pointer: the backend's I/O
// entries may run completions that come back to the same state through another copy of its
// `pollable_fd` (see `BackendEntries`), so no reference to the state is held across them.
//
// # Safety (all of them)
// On this shard's reactor thread; `this` is the root pointer of a live state made by the
// reactor's backend, which outlives the returned future, with no reference to it live.
impl PollableFdState {
    /// # Safety
    /// As above; `buffer` is valid for writes of `size` bytes and used by no one else until the returned future resolves.
    pub unsafe fn read_some_chars(this: *mut Self, buffer: *mut u8, size: usize) -> Future<usize> {
        // SAFETY: the contract above.
        unsafe {
            let (b, e) = engine_entries();
            (e.read)(b, this, buffer as *mut std::ffi::c_void, size)
        }
    }

    /// # Safety
    /// As above; `buffer` is valid for writes of `size` bytes and used by no one else until the returned future resolves.
    pub unsafe fn read_some_bytes(this: *mut Self, buffer: *mut u8, size: usize) -> Future<usize> {
        // SAFETY: the contract above.
        unsafe { Self::read_some_chars(this, buffer, size) }
    }

    /// # Safety
    /// As above.
    pub unsafe fn read_some_iov(this: *mut Self, iov: &Vec<Iovec>) -> Future<usize> {
        // SAFETY: the contract above.
        unsafe {
            let (b, e) = engine_entries();
            (e.recvmsg)(b, this, iov)
        }
    }

    /// # Safety
    /// As above; the allocator outlives the read.
    pub unsafe fn read_some(
        this: *mut Self,
        ba: *mut dyn BufferAllocator,
    ) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: the contract above.
        unsafe {
            let (b, e) = engine_entries();
            (e.read_some)(b, this, ba)
        }
    }

    /// # Safety
    /// As above; the packet outlives the write.
    pub unsafe fn write_some(this: *mut Self, p: &mut Packet) -> Future<usize> {
        // SAFETY: the contract above.
        unsafe {
            let (b, e) = engine_entries();
            (e.sendmsg)(b, this, p)
        }
    }

    /// # Safety
    /// As above; `buffer` is valid for reads of `size` bytes and not written or freed until the returned future resolves.
    pub unsafe fn write_all_chars(this: *mut Self, buffer: *const u8, size: usize) -> Future<()> {
        // SAFETY: the contract above.
        unsafe { Reactor::send_all(this, buffer as *const std::ffi::c_void, size) }
    }

    /// # Safety
    /// As above; `buffer` is valid for reads of `size` bytes and not written or freed until the returned future resolves.
    pub unsafe fn write_all_bytes(this: *mut Self, buffer: *const u8, size: usize) -> Future<()> {
        // SAFETY: the contract above.
        unsafe { Reactor::send_all(this, buffer as *const std::ffi::c_void, size) }
    }

    /// # Safety
    /// As above; the packet outlives the write.
    pub unsafe fn write_all(this: *mut Self, p: &mut Packet) -> Future<()> {
        let pp = p as *mut Packet;
        // SAFETY: the contract above.
        unsafe { Self::write_some(this, p) }.then(move |size: usize| {
            // SAFETY: the state and the packet both outlive the write.
            let p = unsafe { &mut *pp };
            if p.len() as usize == size {
                return make_ready_future(());
            }
            p.trim_front(size);
            // SAFETY: as above.
            unsafe { Self::write_all(this, p) }
        })
    }

    /// # Safety
    /// As above.
    pub unsafe fn readable(this: *mut Self) -> Future<()> {
        // SAFETY: the contract above.
        unsafe { Reactor::readable(this) }
    }

    /// # Safety
    /// As above.
    pub unsafe fn writeable(this: *mut Self) -> Future<()> {
        // SAFETY: the contract above.
        unsafe { Reactor::writeable(this) }
    }

    /// # Safety
    /// As above.
    pub unsafe fn poll_rdhup(this: *mut Self) -> Future<()> {
        // SAFETY: the contract above.
        unsafe { Reactor::poll_rdhup(this) }
    }

    /// # Safety
    /// As above.
    pub unsafe fn readable_or_writeable(this: *mut Self) -> Future<()> {
        // SAFETY: the contract above.
        unsafe { Reactor::readable_or_writeable(this) }
    }

    /// # Safety
    /// As above.
    pub unsafe fn accept(this: *mut Self) -> Future<(PollableFd, SocketAddress)> {
        // SAFETY: the contract above.
        unsafe {
            let (b, e) = engine_entries();
            (e.accept)(b, this)
        }
    }

    /// # Safety
    /// As above.
    pub unsafe fn connect(this: *mut Self, sa: &mut SocketAddress) -> Future<()> {
        // SAFETY: the contract above.
        unsafe {
            let (b, e) = engine_entries();
            (e.connect)(b, this, sa)
        }
    }

    /// # Safety
    /// As above; the allocator outlives the read.
    pub unsafe fn recv_some(
        this: *mut Self,
        ba: *mut dyn BufferAllocator,
    ) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: the contract above; a short reborrow that runs no completion.
        fut_try!(unsafe { (*this).maybe_no_more_recv() });
        // SAFETY: the contract above.
        unsafe {
            let (b, e) = engine_entries();
            (e.recv_some)(b, this, ba)
        }
    }

    /// # Safety
    /// As above; `msg` and the buffers it describes stay valid, and are used by no one else, until the returned future resolves.
    pub unsafe fn recvmsg(this: *mut Self, msg: *mut Msghdr) -> Future<usize> {
        // SAFETY: the contract above; a short reborrow that runs no completion.
        fut_try!(unsafe { (*this).maybe_no_more_recv() });
        // SAFETY: the contract above.
        unsafe { Reactor::readable(this) }.then(move |()| {
            // SAFETY: the state outlives the call; `msg` is owned by the caller
            // until the future resolves; the message header and the buffers it describes are
            // live for the call.
            let r = fut_try!(unsafe { (*this).fd.recvmsg(&mut *msg, 0) });
            let Some(r) = r else {
                // SAFETY: the caller's contract on `msg` holds until the returned future resolves.
                return unsafe { Self::recvmsg(this, msg) };
            };
            // We always speculate here to optimize for throughput in a workload
            // with multiple outstanding requests. This way the caller can consume
            // all messages without resorting to epoll. However this adds extra
            // recvmsg() call when we hit the empty queue condition, so it may
            // hurt request-response workload in which the queue is empty when we
            // initially enter recvmsg(). If that turns out to be a problem, we can
            // improve speculation by using recvmmsg().
            // SAFETY: as above; one call on the state.
            unsafe { (*this).speculate_epoll(EPOLLIN as i32) };
            make_ready_future(r)
        })
    }

    /// # Safety
    /// As above; `msg` and the buffers it describes stay valid and unchanged until the returned future resolves.
    pub unsafe fn sendmsg(this: *mut Self, msg: *mut Msghdr) -> Future<usize> {
        // SAFETY: the contract above; a short reborrow that runs no completion.
        fut_try!(unsafe { (*this).maybe_no_more_send() });
        // SAFETY: the contract above.
        unsafe { Reactor::writeable(this) }.then(move |()| {
            // SAFETY: as in recvmsg().
            let r = fut_try!(unsafe { (*this).fd.sendmsg(&*msg, 0) });
            let Some(r) = r else {
                // SAFETY: as above.
                return unsafe { Self::sendmsg(this, msg) };
            };
            // For UDP this will always speculate. We can't know if there's room
            // or not, but most of the time there should be so the cost of mis-
            // speculation is amortized.
            // SAFETY: as above.
            let total =
                unsafe { iovec_len(std::slice::from_raw_parts((*msg).msg_iov, (*msg).msg_iovlen)) };
            if r == total {
                // SAFETY: as above; one call on the state.
                unsafe { (*this).speculate_epoll(EPOLLOUT as i32) };
            }
            make_ready_future(r)
        })
    }

    /// # Safety
    /// As above; `buf` is valid for reads of `len` bytes and not written or freed until the returned future resolves.
    pub unsafe fn sendto(
        this: *mut Self,
        addr: SocketAddress,
        buf: *const std::ffi::c_void,
        len: usize,
    ) -> Future<usize> {
        // SAFETY: the contract above; a short reborrow that runs no completion.
        fut_try!(unsafe { (*this).maybe_no_more_send() });
        // SAFETY: the contract above.
        unsafe { Reactor::writeable(this) }.then(move |()| {
            // SAFETY: as in recvmsg(); the buffer is valid for reads of the length passed.
            let r = fut_try!(unsafe { (*this).fd.sendto(&addr, buf, len, 0) });
            let Some(r) = r else {
                // SAFETY: the buffer is valid for reads of the length passed.
                return unsafe { Self::sendto(this, addr, buf, len) };
            };
            // See the comment about speculation in sendmsg().
            if r == len {
                // SAFETY: as above; one call on the state.
                unsafe { (*this).speculate_epoll(EPOLLOUT as i32) };
            }
            make_ready_future(r)
        })
    }
}

/// The preemption monitor hook used by the backends.
pub mod internal_preempt {
    /// Installs the preemption monitor of this thread; it only stores the pointer.
    pub use crate::engine::core::preempt::internal::set_need_preempt_var;
}

/// Counts the tasks run by type and prints the counts every million tasks (enabled with
/// `cfg(engine_task_histogram)`).
pub struct TaskHistogram {
    _histogram: HashMap<&'static str, u64>,
    _countdown_to_print: u32,
}

impl Default for TaskHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskHistogram {
    const MAX_COUNTDOWN: u32 = 1_000_000;

    /// An empty histogram.
    pub fn new() -> Self {
        TaskHistogram { _histogram: HashMap::new(), _countdown_to_print: Self::MAX_COUNTDOWN }
    }
    /// Counts `t`; prints and resets the histogram every million tasks.
    pub fn add(&mut self, t: TaskPtr) {
        // SAFETY: `t` is a live task about to run.
        *self._histogram.entry(unsafe { t.type_name() }).or_insert(0) += 1;
        self._countdown_to_print -= 1;
        if self._countdown_to_print == 0 {
            self.print();
            self._countdown_to_print = Self::MAX_COUNTDOWN;
            self._histogram.clear();
        }
    }
    /// Prints the counts to standard output.
    pub fn print(&self) {
        println!(
            "task histogram, {} task types {} tasks",
            self._histogram.len(),
            Self::MAX_COUNTDOWN - self._countdown_to_print
        );
        for (type_, count) in &self._histogram {
            println!("  {:10} {}", count, type_);
        }
    }
}

#[cfg(engine_task_histogram)]
thread_local! {
    static THIS_THREAD_TASK_HISTOGRAM: RefCell<TaskHistogram> = RefCell::new(TaskHistogram::new());
}

/// Counts `t` in this thread's task histogram (a no-op unless enabled).
#[cfg(engine_task_histogram)]
pub fn task_histogram_add_task(t: TaskPtr) {
    THIS_THREAD_TASK_HISTOGRAM.with(|h| h.borrow_mut().add(t));
}

#[cfg(not(engine_task_histogram))]
#[inline(always)]
/// Counts `t` in this thread's task histogram (a no-op unless enabled).
pub fn task_histogram_add_task(_t: TaskPtr) {}

// Base version where this works; some filesystems were only fixed later, so
// this value is mixed in with filesystem-provided values later.
/// Whether the kernel's version supports `RWF_NOWAIT` for linux-aio.
pub static AIO_NOWAIT_SUPPORTED: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
    crate::engine::core::internal::uname::kernel_uname().whitelisted(&["4.13"])
});

/// Whether `RWF_NOWAIT` is used for linux-aio: the kernel probe, until the SMP configuration
/// sets it from the `linux-aio-nowait` option.
#[inline]
pub fn aio_nowait_supported() -> bool {
    match AIO_NOWAIT_OVERRIDE.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => *AIO_NOWAIT_SUPPORTED,
    }
}

static ABORT_ON_EBADF: AtomicBool = AtomicBool::new(false);

/// Makes `EBADF` and `ENOTSOCK` errors abort the process (to catch descriptor misuse).
pub fn set_abort_on_ebadf(do_abort: bool) {
    ABORT_ON_EBADF.store(do_abort, Ordering::SeqCst);
}

/// Whether `EBADF` and `ENOTSOCK` errors abort the process.
pub fn is_abort_on_ebadf_enabled() -> bool {
    ABORT_ON_EBADF.load(Ordering::SeqCst)
}

/// Converts a steady-clock time point to a `timespec`.
pub fn to_timespec(
    t: <SteadyClockType as crate::engine::core::timer::Clock>::TimePoint,
) -> crate::engine::core::posix::Timespec {
    let n = t.0;
    crate::engine::core::posix::Timespec { tv_sec: n / 1_000_000_000, tv_nsec: n % 1_000_000_000 }
}

impl Default for Signals {
    fn default() -> Self {
        Self::new()
    }
}

impl Signals {
    /// An empty signal table.
    pub fn new() -> Self {
        Signals { _pending_signals: AtomicU64::new(0), _signal_handlers: HashMap::new() }
    }

    /// Registers `handler` for `signo` (replacing any previous one) and installs the
    /// kernel-level handler that records the signal for the poll loop.
    ///
    /// # Errors
    ///
    /// Returns the system error of `sigaction` or of unblocking the signal.
    pub fn handle_signal(
        &mut self,
        signo: i32,
        handler: NoncopyableFunction<fn()>,
    ) -> Result<(), ExceptionPtr> {
        let h = SignalHandler::new(signo, handler);
        let inserted = self._signal_handlers.insert(signo, h).is_none();
        if !inserted {
            // since we register the same handler to OS for all signals, we could
            // skip sigaction when a handler has already been registered before.
            return Ok(());
        }

        extern "C" fn sa_sigaction(sig: i32, info: *mut SiginfoT, p: *mut std::ffi::c_void) {
            // The signal interrupts the reactor thread anywhere, so the handler forms no
            // reference to the reactor: it reaches the backend through the raw pointer, and
            // the backends' signal_received() only touches atomics and file descriptors.
            let r = local_engine();
            if r.is_null() {
                Signals::failed_to_handle(sig);
                return;
            }
            // SAFETY: `r` is the live reactor of this thread; signal_received takes `&self`.
            unsafe { (*Reactor::backend_of(r)).signal_received(sig, info, p) };
        }
        let sa = Sigaction {
            sa_sigaction: sa_sigaction as *const () as usize,
            sa_mask: crate::engine::core::posix::make_empty_sigset_mask(),
            sa_flags: SA_SIGINFO | SA_RESTART,
            ..Sigaction::default()
        };
        // SAFETY: `sa` is a valid sigaction.
        let r = unsafe { sigaction(signo, &sa, std::ptr::null_mut()) };
        throw_system_error_on(r == -1, "sigaction")?;
        let mask = crate::engine::core::posix::make_sigset_mask(signo);
        // SAFETY: `mask` is a valid signal set.
        let r = unsafe { pthread_sigmask(SIG_UNBLOCK, &mask, std::ptr::null_mut()) };
        crate::engine::core::posix::throw_pthread_error(r)?;
        Ok(())
    }

    /// Like [`handle_signal`](Self::handle_signal), but the handler runs only for the first
    /// delivery.
    ///
    /// # Errors
    ///
    /// As [`handle_signal`](Self::handle_signal).
    pub fn handle_signal_once(
        &mut self,
        signo: i32,
        mut handler: NoncopyableFunction<fn()>,
    ) -> Result<(), ExceptionPtr> {
        let mut fired = false;
        self.handle_signal(
            signo,
            NoncopyableFunction::<fn()>::new(move || {
                if !fired {
                    fired = true;
                    handler.call();
                }
            }),
        )
    }

    /// Runs the handlers of the pending signals; returns `true` if any ran.
    ///
    /// The handlers re-enter the reactor (a handler may stop it or register another
    /// handler), so the signals are reached through `this`, and each handler is moved out of
    /// the map for its call (the map does not keep its values in place, so an
    /// insertion could move it) and put back afterwards unless it was replaced meanwhile.
    ///
    /// # Safety
    /// `this` points to the live signals of this thread's reactor, with no reference to them
    /// live.
    pub unsafe fn poll_signal(this: *mut Self) -> bool {
        // SAFETY: guaranteed by the caller; every reference below ends before a handler runs.
        unsafe {
            let signals = (*this)._pending_signals.load(Ordering::Relaxed);
            if signals != 0 {
                (*this)._pending_signals.fetch_and(!signals, Ordering::Relaxed);
                for i in 0..(std::mem::size_of::<u64>() * 8) {
                    if signals & (1u64 << i) != 0 {
                        let signo = i as i32;
                        let Some(h) = (*this)._signal_handlers.get_mut(&signo) else {
                            // _signal_handlers.at(i): a pending signal always has a handler.
                            crate::engine::util::assert::internal::assert_fail(
                                "signal without a handler",
                                file!(),
                                line!() as i32,
                                "poll_signal",
                            );
                        };
                        let mut handler = std::mem::replace(
                            &mut h._handler,
                            NoncopyableFunction::<fn()>::empty(),
                        );
                        handler.call();
                        if let Some(h) = (*this)._signal_handlers.get_mut(&signo)
                            && !h._handler.is_some()
                        {
                            h._handler = handler;
                        }
                    }
                }
            }
            signals != 0
        }
    }

    /// Whether any signal is pending.
    pub fn pure_poll_signal(&self) -> bool {
        self._pending_signals.load(Ordering::Relaxed) != 0
    }

    /// The kernel-level handler: records `signo` as pending and wakes the reactor.
    ///
    /// Runs in a signal handler: it forms no reference to the reactor, only to the backend
    /// (whose `start_handling_signal` is signal-safe) and to an atomic.
    pub fn action(signo: i32, _siginfo: *mut SiginfoT, _ignore: *mut std::ffi::c_void) {
        let r = engine();
        // SAFETY: the reactor of this thread is set (the handlers check it); only the
        // backend and the atomic pending set are reached.
        unsafe {
            (*Reactor::backend_of(r)).start_handling_signal();
            (*std::ptr::addr_of!((*r)._signals._pending_signals))
                .fetch_or(1u64 << signo, Ordering::Relaxed);
        }
    }

    /// Logs that `signo` arrived on a thread without a reactor able to handle it.
    pub fn failed_to_handle(signo: i32) {
        let mut tname = [0 as std::ffi::c_char; 64];
        // SAFETY: `tname` is a buffer of 64 bytes.
        unsafe { pthread_getname_np(pthread_self(), tname.as_mut_ptr(), tname.len()) };
        // SAFETY: plain gettid system call.
        let tid = unsafe { syscall(SYS_GETTID) };
        // SAFETY: pthread_getname_np writes a NUL-terminated string.
        let name = unsafe { std::ffi::CStr::from_ptr(tname.as_ptr()) }.to_string_lossy();
        engine_logger().error(
            crate::engine::util::log::FormatInfo::new(
                "Failed to handle signal {} on thread {} ({}): engine not ready",
            ),
            format_args!(
                "Failed to handle signal {} on thread {} ({}): engine not ready",
                signo, tid, name
            ),
        );
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        let mut mask = SigsetT::default();
        // SAFETY: `mask` is a valid signal set.
        unsafe {
            sigfillset(&mut mask);
            pthread_sigmask(SIG_BLOCK, &mask, std::ptr::null_mut());
        }
    }
}

impl SignalHandler {
    /// A handler entry running `handler`.
    pub fn new(_signo: i32, handler: NoncopyableFunction<fn()>) -> Self {
        SignalHandler { _handler: handler }
    }
}

impl Reactor {
    /// Register a user-defined signal handler
    #[deprecated(note = "Use teseladb::handle_signal(signo, handler, once); instead")]
    pub fn handle_signal(
        &mut self,
        signo: i32,
        handler: NoncopyableFunction<fn()>,
    ) -> Result<(), ExceptionPtr> {
        self._signals.handle_signal(signo, handler)
    }
}

/// The `sigaction` structure of Linux x86_64/aarch64 as glibc lays it out.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Sigaction {
    /// The handler (`sa_sigaction`).
    pub sa_sigaction: usize,
    /// Signals blocked while the handler runs.
    pub sa_mask: SigsetT,
    /// `SA_*` flags.
    pub sa_flags: i32,
    /// Restorer (unused).
    pub sa_restorer: usize,
}

/// A signal stack description (`stack_t`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StackT {
    /// Stack base.
    pub ss_sp: *mut std::ffi::c_void,
    /// `SS_*` flags.
    pub ss_flags: i32,
    /// Stack size.
    pub ss_size: usize,
}

/// `sigaction` flag: the handler takes `siginfo_t`.
pub const SA_SIGINFO: i32 = 4;
/// `sigaction` flag: restart interrupted system calls.
pub const SA_RESTART: i32 = 0x10000000;
/// `sigaction` flag: run on the alternate signal stack.
pub const SA_ONSTACK: i32 = 0x08000000;
/// `sigaction` flag: do not block the signal in its handler.
pub const SA_NODEFER: i32 = 0x40000000;
/// `sigaction` flag: reset the handler after one delivery (0x80000000).
pub const SA_RESETHAND: i32 = i32::MIN;
/// `pthread_sigmask` operation: block signals.
pub const SIG_BLOCK: i32 = 0;
/// `pthread_sigmask` operation: unblock signals.
pub const SIG_UNBLOCK: i32 = 1;
/// `pthread_sigmask` operation: set the mask.
pub const SIG_SETMASK: i32 = 2;
/// The `gettid` system call number and the signal stack size.
pub use crate::engine::core::internal::linux_abi::{nr::GETTID as SYS_GETTID, sigstksz};
/// `eventfd` flag: close on exec.
pub const EFD_CLOEXEC: i32 = 0o2000000;
/// `eventfd` flag: non-blocking.
pub const EFD_NONBLOCK: i32 = 0o4000;
/// The monotonic clock id.
pub const CLOCK_MONOTONIC: i32 = 1;
/// The calling thread's CPU-time clock id.
pub const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
/// `timerfd_create` flag: close on exec.
pub const TFD_CLOEXEC: i32 = 0o2000000;

unsafe extern "C" {
    /// Installs a signal handler.
    pub fn sigaction(signum: i32, act: *const Sigaction, oldact: *mut Sigaction) -> i32;
    /// Sets the alternate signal stack.
    pub fn sigaltstack(ss: *const StackT, old_ss: *mut StackT) -> i32;
    /// Changes the calling thread's signal mask.
    pub fn pthread_sigmask(how: i32, set: *const SigsetT, oldset: *mut SigsetT) -> i32;
    /// Fills a signal set.
    pub fn sigfillset(set: *mut SigsetT) -> i32;
    /// Empties a signal set.
    pub fn sigemptyset(set: *mut SigsetT) -> i32;
    /// Adds a signal to a set.
    pub fn sigaddset(set: *mut SigsetT, signo: i32) -> i32;
    /// Removes a signal from a set.
    pub fn sigdelset(set: *mut SigsetT, signo: i32) -> i32;
    /// Reads a thread's name.
    pub fn pthread_getname_np(thread: usize, name: *mut std::ffi::c_char, len: usize) -> i32;
    /// The calling thread's handle.
    pub fn pthread_self() -> usize;
    /// A raw system call.
    pub fn syscall(num: i64, ...) -> i64;
}

/// Accumulates an in-memory backtrace and flushes it to standard error eventually.
/// Async-signal safe and allocation-free.
pub struct BacktraceBuffer {
    _pos: u32,
    _buf: [u8; BacktraceBuffer::MAX_SIZE],
    _immediate: bool,
}

impl BacktraceBuffer {
    const MAX_SIZE: usize = 8 << 10;

    /// An empty buffer; when `immediate`, every append is written at once.
    pub fn new(immediate: bool) -> Self {
        BacktraceBuffer { _pos: 0, _buf: [0; Self::MAX_SIZE], _immediate: immediate }
    }

    /// Writes the buffered text.
    pub fn flush(&mut self) {
        if !self._immediate && self._pos > 0 {
            crate::engine::util::print_safe::print_safe(&self._buf[..self._pos as usize]);
            self._pos = 0;
        }
    }

    /// Makes room for `len` bytes, flushing if needed.
    pub fn reserve(&mut self, len: usize) {
        if self._immediate {
            return;
        }
        engine_assert!(len < Self::MAX_SIZE);
        if self._pos as usize + len >= Self::MAX_SIZE {
            self.flush();
        }
    }

    /// Appends raw bytes.
    pub fn append_bytes(&mut self, s: &[u8]) {
        if self._immediate {
            crate::engine::util::print_safe::print_safe(s);
        } else {
            self.reserve(s.len());
            let pos = self._pos as usize;
            self._buf[pos..pos + s.len()].copy_from_slice(s);
            self._pos += s.len() as u32;
        }
    }

    /// Appends a string.
    pub fn append(&mut self, s: &str) {
        self.append_bytes(s.as_bytes());
    }

    /// Appends `n` in decimal.
    pub fn append_decimal(&mut self, n: u64) {
        let mut buf = [0u8; std::mem::size_of::<u64>() * 3];
        let len = crate::engine::util::print_safe::convert_decimal_safe(
            &mut buf,
            std::mem::size_of::<u64>() * 3,
            n,
        );
        self.append_bytes(&buf[..len]);
    }

    /// Appends `ptr` in hexadecimal.
    pub fn append_hex(&mut self, ptr: u64) {
        let mut buf = [0u8; std::mem::size_of::<u64>() * 2];
        let p = crate::engine::util::print_safe::convert_hex_safe(&mut buf, ptr);
        self.append_bytes(&buf[p..]);
    }

    /// Appends the current backtrace, one frame per line.
    pub fn append_backtrace(&mut self) {
        let immediate = self._immediate;
        crate::engine::util::backtrace::backtrace_with(
            |f: crate::engine::util::backtrace::Frame| {
                self.append("  ");
                let so = f.so;
                if !so.name.empty() {
                    self.append_bytes(so.name.as_bytes());
                    self.append("+");
                }

                self.append("0x");
                self.append_hex(f.addr as u64);
                self.append("\n");
            },
            immediate,
        );
    }

    /// Appends the current backtrace on a single line.
    pub fn append_backtrace_oneline(&mut self) {
        let immediate = self._immediate;
        crate::engine::util::backtrace::backtrace_with(
            |f: crate::engine::util::backtrace::Frame| {
                self.reserve(3 + std::mem::size_of::<usize>() * 2);
                self.append(" 0x");
                self.append_hex(f.addr as u64);
            },
            immediate,
        );
    }
}

impl Drop for BacktraceBuffer {
    fn drop(&mut self) {
        self.flush();
    }
}

fn print_with_backtrace_buf(buf: &mut BacktraceBuffer, oneline: bool) {
    if !local_engine().is_null() {
        buf.append(" on shard ");
        buf.append_decimal(crate::engine::core::shard_id::this_shard_id() as u64);

        buf.append(", in scheduling group ");
        buf.append(current_scheduling_group().name().as_str());
    }

    if !oneline {
        buf.append(".\nBacktrace:\n");
        buf.append_backtrace();
    } else {
        buf.append(". Backtrace:");
        buf.append_backtrace_oneline();
        buf.append("\n");
    }
}
/// Prints the current backtrace to standard output with the given cause.
///
/// If `oneline` is true, the backtrace is printed entirely on one line, otherwise with one
/// line per frame. If `immediate` is true, the backtrace is printed frame by frame with a
/// call to `write(2)` each, otherwise in a single call to `write(2)`. The former is more
/// robust where the backtrace itself may crash at some point down the stack, while the
/// latter is more efficient and avoids splitting output in the face of concurrent logging
/// by other shards.
pub fn print_with_backtrace(cause: &str, oneline: bool, immediate: bool) {
    let mut buf = BacktraceBuffer::new(immediate);
    buf.append(cause);
    print_with_backtrace_buf(&mut buf, oneline);
}

/// The guard returned by [`install_signal_handler_stack`].
pub struct SignalHandlerStack {
    mem: Option<Box<[u8]>>,
    prev_stack: StackT,
}

impl Drop for SignalHandlerStack {
    fn drop(&mut self) {
        // SAFETY: `prev_stack` was filled in by sigaltstack.
        let r = unsafe { sigaltstack(&self.prev_stack, std::ptr::null_mut()) };
        if let Err(e) = throw_system_error_on(r == -1, "sigaltstack") {
            std::mem::forget(self.mem.take()); // We failed to restore previous stack, must leak it.
            engine_logger().error(
                crate::engine::util::log::FormatInfo::new("Failed to restore signal stack: {}"),
                format_args!(
                    "Failed to restore signal stack: {}",
                    crate::engine::util::log::DisplayExceptionPtr(Some(&e))
                ),
            );
        }
    }
}

/// Installs an alternate signal stack for the current thread, so that `SIGSEGV` on a stack
/// overflow can be handled. The stack remains installed as long as the returned guard is
/// kept alive; when it goes out of scope the previous stack is restored. Built with
/// AddressSanitizer, nothing is installed (the default stack size is too small there).
///
/// # Errors
///
/// Returns the system error of `sigaltstack`.
pub fn install_signal_handler_stack() -> Result<SignalHandlerStack, ExceptionPtr> {
    let size = sigstksz();
    let mut mem = vec![0u8; size].into_boxed_slice();
    let stack =
        StackT { ss_sp: mem.as_mut_ptr() as *mut std::ffi::c_void, ss_flags: 0, ss_size: size };
    let mut prev_stack = StackT { ss_sp: std::ptr::null_mut(), ss_flags: 0, ss_size: 0 };
    // SAFETY: `stack` describes memory that the returned guard keeps alive.
    let r = unsafe { sigaltstack(&stack, &mut prev_stack) };
    throw_system_error_on(r == -1, "sigaltstack")?;
    Ok(SignalHandlerStack { mem: Some(mem), prev_stack })
}

fn shorten_name(name: &Sstring, length: usize) -> Sstring {
    engine_assert!(!name.empty());
    engine_assert!(length > 0);

    const DELIMITER: &str = "_";

    let mut shortname: Vec<u8> = Vec::with_capacity(length);
    let name_s = name.as_str();
    if !name_s.contains(DELIMITER) {
        // use the prefix as the fallback, if the name is not underscore
        // delimited
        let n = length.min(name_s.len());
        shortname.extend_from_slice(&name_s.as_bytes()[..n]);
    } else {
        // token_compress_on: consecutive delimiters are one
        for part in name_s.split('_').filter(|p| !p.is_empty()) {
            if shortname.len() == length {
                break;
            }
            engine_assert!(!part.is_empty());
            // convert "hello_world" to "hw"
            shortname.push(part.as_bytes()[0]);
        }
    }
    // pad the remaining part with spaces, so the shortened name is always
    // of length long.
    shortname.resize(length, b' ');
    Sstring::from_bytes(&shortname)
}

impl TaskQueue {
    /// A task queue for scheduling group `id`, with its names and shares.
    ///
    /// # Errors
    ///
    /// Returns the error of registering the queue's metrics.
    pub fn new(
        id: u32,
        name: Sstring,
        shortname: Sstring,
        shares: f32,
    ) -> Result<Box<Self>, ExceptionPtr> {
        let shares = shares.max(1.0);
        let mut this = Box::new(TaskQueue {
            _vruntime: 0,
            _shares: shares,
            _reciprocal_shares_times_2_power_32: ((1u64 << 32) as f32 / shares) as i64,
            _active: false,
            _id: id as u8,
            _ts: Reactor::now(),
            _runtime: SchedDuration::default(),
            _waittime: SchedDuration::default(),
            _starvetime: SchedDuration::default(),
            _tasks_processed: 0,
            _q: CircularBuffer::new(),
            _name: Sstring::new(),
            _shortname: Sstring::new(),
            _time_spent_on_task_quota_violations: SchedDuration::default(),
            _metrics: metrics::MetricGroups::new(),
        });
        this.rename(name, shortname)?;
        Ok(this)
    }

    fn register_stats(&mut self) -> Result<(), ExceptionPtr> {
        let mut new_metrics = metrics::MetricGroups::new();
        use crate::engine::core::metrics as sm;
        // The group label holds per-thread data, so each reactor thread keeps its own.
        thread_local! {
            static GROUP: sm::LabelType = sm::label("group");
        }
        let group_label = GROUP.with(|g| g.call(self._name.as_str()));
        let this = self as *const TaskQueue;
        // SAFETY (for every closure below): the metrics are owned by the task queue and are
        // unregistered when it is dropped, so `this` is valid whenever they are read.
        new_metrics.add_group("scheduler", vec![
            // SAFETY: `this` is the task queue, live while its metrics are registered (see above);
            // the metric is read on the queue's shard, outside any mutation of the queue.
            sm::make_counter("runtime_ms", move || unsafe { (*this)._runtime.as_millis() },
                sm::description("Accumulated runtime of this task queue; an increment rate of 1000ms per second indicates full utilization"),
                vec![group_label.clone()]),
            // SAFETY: `this` is the task queue, live while its metrics are registered (see above);
            // the metric is read on the queue's shard, outside any mutation of the queue.
            sm::make_counter("waittime_ms", move || unsafe { (*this)._waittime.as_millis() },
                sm::description("Accumulated waittime of this task queue; an increment rate of 1000ms per second indicates queue is waiting for something (e.g. IO)"),
                vec![group_label.clone()]),
            // SAFETY: `this` is the task queue, live while its metrics are registered (see above);
            // the metric is read on the queue's shard, outside any mutation of the queue.
            sm::make_counter("starvetime_ms", move || unsafe { (*this)._starvetime.as_millis() },
                sm::description("Accumulated starvation time of this task queue; an increment rate of 1000ms per second indicates the scheduler feels really bad"),
                vec![group_label.clone()]),
            // SAFETY: `this` is the task queue, live while its metrics are registered (see above);
            // the metric is read on the queue's shard, outside any mutation of the queue.
            sm::make_counter("tasks_processed", move || unsafe { (*this)._tasks_processed },
                sm::description("Count of tasks executing on this queue; indicates together with runtime_ms indicates length of tasks"),
                vec![group_label.clone()]),
            // SAFETY: `this` is the task queue, live while its metrics are registered (see above);
            // the metric is read on the queue's shard, outside any mutation of the queue.
            sm::make_gauge("queue_length", move || unsafe { (*this)._q.size() as f64 },
                sm::description("Size of backlog on this queue, in tasks; indicates whether the queue is busy and/or contended"),
                vec![group_label.clone()]),
            // SAFETY: `this` is the task queue, live while its metrics are registered (see above);
            // the metric is read on the queue's shard, outside any mutation of the queue.
            sm::make_gauge("shares", move || unsafe { (*this)._shares as f64 },
                sm::description("Shares allocated to this queue"),
                vec![group_label.clone()]),
            // SAFETY: `this` is the task queue, live while its metrics are registered (see above);
            // the metric is read on the queue's shard, outside any mutation of the queue.
            sm::make_counter("time_spent_on_task_quota_violations_ms", move || unsafe {
                    (*this)._time_spent_on_task_quota_violations.as_millis()
                },
                sm::description("Total amount in milliseconds we were in violation of the task quota"),
                vec![group_label.clone()]),
        ])?;

        crate::engine::net::stack::register_net_metrics_for_scheduling_group(
            &mut new_metrics,
            self._id as u32,
            &group_label,
        );

        self._metrics = std::mem::take(&mut new_metrics);
        Ok(())
    }

    /// The metric registration error of `register_stats` is the `Err`.
    /// Renames the queue and re-registers its metrics under the new name.
    ///
    /// # Errors
    ///
    /// Returns the error of registering the metrics.
    pub fn rename(
        &mut self,
        new_name: Sstring,
        new_shortname: Sstring,
    ) -> Result<(), ExceptionPtr> {
        engine_assert!(!new_name.empty());
        if self._name != new_name {
            self._name = new_name;
            if new_shortname.empty() {
                self._shortname = shorten_name(&self._name, Self::SHORTNAME_SIZE);
            } else {
                self._shortname = Sstring::from(format!(
                    "{:>width$}",
                    new_shortname.as_str(),
                    width = Self::SHORTNAME_SIZE
                ));
            }
            self.register_stats()?;
        }
        Ok(())
    }

    /// Converts run time to virtual run time (scaled by the inverse of the shares).
    // The multiplication may overflow; the result is clamped at zero.
    #[inline]
    pub fn to_vruntime(&self, runtime: SchedDuration) -> i64 {
        let scaled = (runtime.count().wrapping_mul(self._reciprocal_shares_times_2_power_32)) >> 32;
        // Prevent overflow from returning ridiculous values
        scaled.max(0)
    }

    /// Sets the shares (at least 1).
    pub fn set_shares(&mut self, shares: f32) {
        self._shares = shares.max(1.0);
        self._reciprocal_shares_times_2_power_32 = ((1u64 << 32) as f32 / self._shares) as i64;
    }
}

impl Reactor {
    /// Charges `runtime` to `tq`, recording task-quota violations in the stall histogram.
    pub(crate) fn account_runtime(&mut self, tq: &mut TaskQueue, runtime: SchedDuration) {
        if runtime > (self._cfg.task_quota * 2) {
            // The histogram counts microseconds.
            self._stalls_histogram.add_micro((runtime.count() / 1000) as u64);
            tq._time_spent_on_task_quota_violations += runtime - self._cfg.task_quota;
        }
        tq._vruntime += tq.to_vruntime(runtime);
        tq._runtime += runtime;
    }
}

/// Orders task queues by virtual runtime.
pub fn task_queue_indirect_compare(tq1: &*mut TaskQueue, tq2: &*mut TaskQueue) -> bool {
    // SAFETY: the active task queue list holds pointers to live task queues.
    unsafe { (**tq1)._vruntime < (**tq2)._vruntime }
}

impl Reactor {
    /// Constructs the reactor of shard `id` in place at `place`: the constructor uses
    /// `engine()`, so this thread's reactor pointer already points at `place`.
    ///
    /// # Safety
    ///
    /// `place` must be valid, suitably aligned, uninitialized storage for a `Reactor`. On
    /// `Ok` it holds a fully constructed reactor; on `Err` it holds nothing.
    pub unsafe fn construct_in_place(
        place: *mut Reactor,
        smp: Arc<Smp>,
        alien: *mut alien::Instance,
        id: u32,
        rbs: ReactorBackendSelector,
        cfg: ReactorConfig,
    ) -> Result<(), ExceptionPtr> {
        // SAFETY: `place` is valid storage (caller contract).
        unsafe {
            std::ptr::write(
                place,
                Reactor {
                    _smp: smp,
                    _alien: alien,
                    _cfg: cfg,
                    _notify_eventfd: FileDesc::eventfd(0, EFD_CLOEXEC)?,
                    _task_quota_timer: FileDesc::timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC)?,
                    _backend: None,
                    _active_sigmask: SigsetT::default(),
                    _pollers: Vec::new(),
                    _io_queues: HashMap::new(),
                    _io_sink: IoSink::new(),
                    _num_io_groups: 0,
                    _exit_funcs: Vec::new(),
                    _id: id,
                    _stopping: false,
                    _stopped: false,
                    _finished_running_tasks: false,
                    _stop_requested: ConditionVariable::new(),
                    _network_stack_ready: None,
                    _return: 0,
                    _start_promise: Promise::new(),
                    _preemption_monitor: PreemptionMonitor::default(),
                    _global_tasks_processed: 0,
                    _polls: 0,
                    _stalls_histogram: TimeEstimatedHistogram::new(),
                    _cpu_stall_detector: Some(
                        crate::engine::core::internal::stall_detector::make_cpu_stall_detector(
                            CpuStallDetectorConfig::default(),
                        )?,
                    ),
                    _timers: TimerSet::new(),
                    _expired_timers: TimerList::new(),
                    _lowres_timers: TimerSet::new(),
                    _expired_lowres_timers: TimerList::new(),
                    _manual_timers: TimerSet::new(),
                    _expired_manual_timers: TimerList::new(),
                    _io_stats: IoStats::default(),
                    _fsyncs: 0,
                    _exceptions_thrown: 0,
                    _abandoned_failed_futures: 0,
                    _task_queues: std::array::from_fn(|_| None),
                    _scheduling_group_specific_data:
                        SchedulingGroupSpecificThreadLocalData::default(),
                    _scheduling_group_keys_mutex: SharedMutex::new(),
                    _last_vruntime: 0,
                    _active_task_queues: TaskQueueList::new(),
                    _activating_task_queues: TaskQueueList::new(),
                    _at_destroy_tasks: std::ptr::null_mut(),
                    _current_task: None,
                    _idle_cpu_handler: IdleCpuHandler::new(|_: WorkWaitingOnReactor| {
                        IdleCpuHandlerResult::NoMoreWork
                    }),
                    _network_stack: None,
                    _lowres_next_timeout:
                        <LowresClock as crate::engine::core::timer::Clock>::TimePoint::max(),
                    _aio_eventfd: None,
                    _loads: CircularBuffer::new(),
                    _load: 0.0,
                    _last_true_steal: SchedDuration::default(),
                    _last_mono_steal: SchedDuration::default(),
                    _total_idle: SchedDuration::default(),
                    _idle_start: None,
                    _idle_end: SchedTimePoint::default(),
                    _total_sleep: SchedDuration::default(),
                    _start_time: Reactor::now(),
                    _flush_batching: BatchFlushListT::new(),
                    _sleeping: CacheAligned(AtomicBool::new(false)),
                    _dying: AtomicBool::new(false),
                    _background_gate: Gate::new(),
                    _signals: Signals::new(),
                    _thread_pool: None,
                    _metric_groups: metrics::MetricGroups::new(),
                },
            )
        };
        let rp = place;
        // The rest of the body reaches the reactor only through `place`, one place expression
        // per access: the backend's constructor, the task queues and `start_tick` reach it
        // through `rp` and `engine()` too, so no reference to it is kept across them.
        //
        // SAFETY: `place` now holds the reactor's fields, and no reference to the reactor is
        // live (the same holds for each `(*place)` below).
        // The pool only uses the reactor's notify eventfd, which outlives it (the destructor
        // joins the pool first).
        let notify = unsafe { (*place)._notify_eventfd.get() };
        let pool = match ThreadPool::new(notify, Sstring::from(format!("syscall-{}", id))) {
            Ok(p) => p,
            Err(e) => {
                // SAFETY: undo the construction so that the storage holds nothing.
                unsafe { std::ptr::drop_in_place(place) };
                return Err(e);
            }
        };
        // SAFETY: as above, `place` holds the reactor's fields and no reference to it is live.
        unsafe { (*place)._thread_pool = Some(pool) };
        /*
         * The _backend assignment is here, not on the initialization list as
         * the chosen backend constructor may want to handle signals and thus
         * needs the _signals._signal_handlers map to be initialized.
         */
        // SAFETY: as above.
        let backend = match unsafe { rbs.create(rp) } {
            Ok(b) => {
                // SAFETY: a shared reborrow of the new, live backend, to read its table; no
                // other reference to it exists yet.
                let e = unsafe { (*b.as_ptr()).entries() };
                (b, e)
            }
            Err(e) => {
                // SAFETY: undo the construction so that the storage holds nothing.
                unsafe { std::ptr::drop_in_place(place) };
                return Err(e);
            }
        };
        // SAFETY: see above for every `(*place)`; the stored pointers are taken with
        // `addr_of!` from `place`.
        unsafe {
            (*place)._backend = Some(backend);
            *crate::engine::core::scheduling_specific::internal::get_scheduling_group_specific_thread_local_data_ptr() = std::ptr::addr_of_mut!((*place)._scheduling_group_specific_data);
            let queues = TaskQueue::new(0, Sstring::from("main"), Sstring::from("main"), 1000.0)
                .and_then(|m| {
                    Ok((
                        m,
                        TaskQueue::new(1, Sstring::from("atexit"), Sstring::from("exit"), 1000.0)?,
                    ))
                });
            let (main, atexit) = match queues {
                Ok(q) => q,
                Err(e) => {
                    // undo the construction so that the storage holds nothing
                    std::ptr::drop_in_place(place);
                    return Err(e);
                }
            };
            let atexit = task_queue_node(atexit);
            (*place)._at_destroy_tasks = atexit.as_ptr();
            (*place)._task_queues[0] = Some(task_queue_node(main));
            (*place)._task_queues[1] = Some(atexit);
            internal_preempt::set_need_preempt_var(std::ptr::addr_of!(
                (*place)._preemption_monitor
            ));
        }
        crate::engine::core::thread::thread_impl::init();
        // SAFETY: the backend was just set; no reference to it is live.
        unsafe {
            let (b, e) = Self::backend_entries(place);
            (e.start_tick)(b)
        };

        let mut mask = SigsetT::default();
        // SAFETY: `mask` is a valid signal set.
        let r = unsafe {
            sigemptyset(&mut mask);
            sigaddset(&mut mask, CpuStallDetector::signal_number());
            pthread_sigmask(SIG_UNBLOCK, &mask, std::ptr::null_mut())
        };
        engine_assert!(r == 0);
        memory::set_reclaim_hook(Box::new(move |reclaim_fn: Box<dyn FnOnce()>| {
            // SAFETY: the hook is reset in the reactor's destructor.
            unsafe {
                (*rp).add_high_priority_task(make_task_ptr(default_scheduling_group(), reclaim_fn))
            };
        }));
        Ok(())
    }
}

/// Moves a task queue made by [`TaskQueue::new`] into the node that `_task_queues` keeps.
#[inline]
fn task_queue_node(q: Box<TaskQueue>) -> crate::engine::net::node_ptr::NodePtr<TaskQueue> {
    // SAFETY: the pointer comes from `Box::into_raw` and the node is its only owner.
    unsafe { crate::engine::net::node_ptr::NodePtr::from_raw(Box::into_raw(q)) }
}

/// Appends the current backtrace to `buf` and flushes it (see [`print_with_backtrace`]).
pub(crate) fn print_with_backtrace_into(buf: &mut BacktraceBuffer, oneline: bool) {
    print_with_backtrace_buf(buf, oneline)
}

impl Reactor {
    /// Destroys the reactor: runs the shutdown body, then drops the fields in the order that
    /// matters (the reverse of their declaration), then the rest.
    ///
    /// Not a `Drop`: the body cancels timers, whose `del_timer` reaches the reactor through
    /// `engine()`, so it works through the pointer with no reference to the reactor kept
    /// across those calls; and Rust would drop the fields in declaration order. A constructor
    /// that fails only drops the fields built so far (`drop_in_place`), without running this.
    ///
    /// # Safety
    /// `this` is a fully constructed reactor, with no reference to it live; it is left
    /// uninitialized.
    pub(crate) unsafe fn destroy(this: *mut Self) {
        let mut mask = SigsetT::default();
        // SAFETY: `mask` is a valid signal set.
        let r = unsafe {
            sigemptyset(&mut mask);
            sigaddset(&mut mask, CpuStallDetector::signal_number());
            pthread_sigmask(SIG_BLOCK, &mask, std::ptr::null_mut())
        };
        engine_assert!(r == 0);

        // SAFETY: the contract above; every `(*this)` below is one place expression, so no
        // reference to the reactor is live while a timer is cancelled.
        unsafe {
            if let Some((backend, entries)) = (*this)._backend.as_ref() {
                // the backend is live; no reference to it is live
                (entries.stop_tick)(backend.as_ptr().cast::<()>());
            }
            // Cancelling an expired timer unlinks it from its expired list (del_timer).
            // The timers in the expired lists are live (a timer leaves its list before it is
            // destroyed), and no reference to them is live.
            while let Some(timer) = (*this)._expired_timers.front() {
                <TimerImpl<SteadyClockType> as TimerSetEntry>::cancel(timer.as_ptr());
            }
            while let Some(timer) = (*this)._expired_lowres_timers.front() {
                <TimerImpl<LowresClock> as TimerSetEntry>::cancel(timer.as_ptr());
            }
            while let Some(timer) = (*this)._expired_manual_timers.front() {
                <TimerImpl<ManualClock> as TimerSetEntry>::cancel(timer.as_ptr());
            }
            for i in 0..(*this)._task_queues.len() {
                let id = match &(*this)._task_queues[i] {
                    // the queue is live while its node is; one field read
                    Some(tq) => (*tq.as_ptr())._id as u32,
                    None => continue,
                };
                // The following line will preserve the convention that constructor and destructor functions
                // for the per sg values are called in the context of the containing scheduling group.
                set_current_scheduling_group(SchedulingGroup::from_id(id));
                let vals = std::mem::take(&mut (*this).get_sg_data_by_id(id).specific_vals);
                drop(vals);
            }

            // The members, last declared first: the metrics (they read the reactor), the
            // syscall thread (it writes the notify eventfd, closed below), the socket and
            // timer descriptors, the network stack and the tasks (their pollable_fds go back
            // to the backend), and the backend before the descriptors it uses.
            (*this)._metric_groups = metrics::MetricGroups::new();
            (*this)._thread_pool = None;
            (*this)._aio_eventfd = None;
            (*this)._network_stack = None;
            (*this)._idle_cpu_handler =
                IdleCpuHandler::new(|_: WorkWaitingOnReactor| IdleCpuHandlerResult::NoMoreWork);
            (*this)._current_task = None;
            for i in 0..(*this)._task_queues.len() {
                (*this)._task_queues[i] = None;
            }
            (*this)._network_stack_ready = None;
            (*this)._exit_funcs.clear();
            (*this)._cpu_stall_detector = None;
            (*this)._io_queues.clear();
            (*this)._backend = None;
            std::ptr::drop_in_place(this);
        }
    }
}

/// Milliseconds as kept by the reactor (a `Duration`).
pub type Milliseconds = Duration;

impl Reactor {
    /// Returns a snapshot of this shard's scheduling statistics (see [`SchedStats`] for
    /// each of them).
    pub fn get_sched_stats(&self) -> SchedStats {
        SchedStats { tasks_processed: self.tasks_processed() }
    }

    /// Resolves when `fd` is readable: on the state's root pointer, with no
    /// reference to the reactor (the backend's poll may run completions that re-enter it).
    ///
    /// # Safety
    /// On this shard's reactor thread; `fd` is the root pointer of a live state made by the
    /// reactor's backend, which outlives the returned future, with no reference to it live.
    pub unsafe fn readable(fd: *mut PollableFdState) -> Future<()> {
        // SAFETY: the contract above.
        unsafe {
            let (b, e) = engine_entries();
            (e.readable)(b, fd)
        }
    }

    /// Resolves when `fd` is writeable.
    ///
    /// # Safety
    /// As [`Reactor::readable`].
    pub unsafe fn writeable(fd: *mut PollableFdState) -> Future<()> {
        // SAFETY: the contract above.
        unsafe {
            let (b, e) = engine_entries();
            (e.writeable)(b, fd)
        }
    }

    /// Resolves when `fd` is readable or writeable.
    ///
    /// # Safety
    /// As [`Reactor::readable`].
    pub unsafe fn readable_or_writeable(fd: *mut PollableFdState) -> Future<()> {
        // SAFETY: the contract above.
        unsafe {
            let (b, e) = engine_entries();
            (e.readable_or_writeable)(b, fd)
        }
    }

    /// Resolves when the peer of `fd` shut down its side.
    ///
    /// # Safety
    /// As [`Reactor::readable`].
    pub unsafe fn poll_rdhup(fd: *mut PollableFdState) -> Future<()> {
        // SAFETY: the contract above.
        unsafe {
            let (b, e) = engine_entries();
            (e.poll_rdhup)(b, fd)
        }
    }

    /// Sets the "Strict DMA" flag.
    ///
    /// When true (default), file I/O operations must use DMA.  This is
    /// the most performant option, but does not work on some file systems
    /// such as tmpfs or aufs (used in some Docker setups).
    ///
    /// When false, file I/O operations can fall back to buffered I/O if
    /// DMA is not available.  This can result in dramatic reducation in
    /// performance and an increase in memory consumption.
    /// Sets whether `O_DIRECT` is required (no fallback to buffered I/O).
    pub fn set_strict_dma(&mut self, value: bool) {
        self._cfg.strict_o_direct = value;
    }

    /// Sets whether `fsync` is skipped.
    pub fn set_bypass_fsync(&mut self, value: bool) {
        self._cfg.bypass_fsync = value;
    }

    /// The backend runs the preempting completions here, among them the high resolution
    /// timer's, which runs expired timers that re-enter the reactor.
    ///
    /// # Safety
    /// `this` is the live reactor of this thread, with no reference to it or to its backend
    /// live.
    pub(crate) unsafe fn reset_preemption_monitor(this: *mut Self) {
        // SAFETY: the contract above; the entries' contract holds.
        unsafe {
            let (b, e) = Self::backend_entries(this);
            (e.reset_preemption_monitor)(b)
        }
    }

    /// Makes [`need_preempt`](crate::engine::core::preempt::need_preempt) return `true` now.
    pub(crate) fn request_preemption(&mut self) {
        // SAFETY: the backend only queues its timer completion and waits for the preemption
        // flag, without running completions, so no reference to the backend is live and the
        // reactor's reference is not re-entered.
        unsafe {
            let (b, e) = Self::backend_entries(self);
            (e.request_preemption)(b)
        }
    }

    /// Sets the stall detector threshold.
    ///
    /// # Panics
    ///
    /// Panics if the reactor has no stall detector.
    pub fn update_blocked_reactor_notify_ms(&mut self, ms: Milliseconds) {
        let det = self._cpu_stall_detector.as_mut().unwrap();
        let mut cfg = det.get_config();
        if ms != cfg.threshold {
            cfg.threshold = ms;
            det.update_config(cfg);
            engine_logger().info(
                crate::engine::util::log::FormatInfo::new("updated: blocked-reactor-notify-ms={}"),
                format_args!("updated: blocked-reactor-notify-ms={}", ms.as_millis()),
            );
        }
    }

    /// The stall detector threshold.
    ///
    /// # Panics
    ///
    /// Panics if the reactor has no stall detector.
    pub fn get_blocked_reactor_notify_ms(&self) -> Milliseconds {
        let d = self._cpu_stall_detector.as_ref().unwrap().get_config().threshold;
        Duration::from_millis(d.as_millis() as u64)
    }
}

impl ReactorTest {
    /// Sets the stall reporting function which is called when a stall
    /// is detected (and not suppressed). Setting the function also
    /// resets the supression state.
    pub fn set_stall_detector_report_function(report: Option<Arc<dyn Fn() + Send + Sync>>) {
        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see
        // `engine()`), and its detector exists from its construction on.
        let det = unsafe { (*engine())._cpu_stall_detector.as_deref_mut().unwrap_unchecked() };
        let mut cfg = det.get_config();
        cfg.report = report;
        det.update_config(cfg);
        det.reset_suppression_state(Reactor::now());
    }

    /// The stall detector's test reporting function, if any.
    pub fn get_stall_detector_report_function() -> Option<Arc<dyn Fn() + Send + Sync>> {
        // SAFETY: on this shard's reactor thread; a shared access to the detector, which the
        // constructor creates.
        unsafe { (*engine())._cpu_stall_detector.as_deref().unwrap_unchecked() }.get_config().report
    }
}

impl Reactor {
    /// The stall detector's signal handler.
    ///
    /// It interrupts the reactor thread anywhere, so it forms no reference to the reactor:
    /// the detector is reached through a raw pointer. The detector's
    /// signal path and its task-run hooks share this thread and are not reentrant with each
    /// other in a way that matters: the hooks only store atomics and plain counters.
    pub(crate) extern "C" fn block_notifier(_: i32) {
        let r = local_engine();
        if r.is_null() {
            return;
        }
        // SAFETY: `r` is the live reactor of this thread, whose detector exists for its
        // whole life; only the detector is reached.
        unsafe {
            let det = (*std::ptr::addr_of_mut!((*r)._cpu_stall_detector))
                .as_deref_mut()
                .unwrap_unchecked() as *mut CpuStallDetector;
            CpuStallDetector::on_signal(det);
        }
    }
}

/// Creates a network stack from its options.
pub struct NetworkStackFactory {
    /// The factory function; the cell gives the `&mut` that `NoncopyableFunction::call`
    /// takes from the shared `call`.
    _func: std::cell::UnsafeCell<crate::engine::net::api::FactoryFunc>,
}

impl NetworkStackFactory {
    /// Wraps `func`.
    pub fn new(func: crate::engine::net::api::FactoryFunc) -> Self {
        NetworkStackFactory { _func: std::cell::UnsafeCell::new(func) }
    }
    /// Creates the network stack configured by `opts`.
    pub fn call(
        &self,
        opts: &crate::engine::util::program_options::OptionGroup,
    ) -> Future<Box<dyn NetworkStack>> {
        // SAFETY: the factories of the network stacks build a stack from their options
        // and never reach this factory again, so this is the only access while it runs.
        unsafe { (*self._func.get()).call(opts) }
    }
}

/// Socket type: stream.
pub const SOCK_STREAM: i32 = 1;
/// Socket type: datagram.
pub const SOCK_DGRAM: i32 = 2;
/// Socket option: reuse local addresses.
pub const SO_REUSEADDR: i32 = 2;
/// Socket option: send buffer size.
pub const SO_SNDBUF: i32 = 7;
/// Socket option: receive buffer size.
pub const SO_RCVBUF: i32 = 8;
/// errno: connection aborted.
pub const ECONNABORTED: i32 = 103;
/// errno: connection reset by peer.
pub const ECONNRESET: i32 = 104;
/// errno: broken pipe.
pub const EPIPE: i32 = 32;

unsafe extern "C" {
    fn chmod(path: *const std::ffi::c_char, mode: u32) -> i32;
    fn unlink(path: *const std::ffi::c_char) -> i32;
}

impl Reactor {
    /// Applies the reactor options: starts creating the network stack and sets the run-time
    /// configuration.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid option value.
    pub(crate) fn configure(&mut self, opts: &ReactorOptions) -> Result<(), ExceptionPtr> {
        // Every network stack registers its options (see
        // create_network_stacks_option), so the selected candidate always has them.
        let stack_opts = opts
            .network_stack
            .get_selected_candidate_opts()
            .expect("every network stack candidate has its options");
        self._network_stack_ready =
            Some(opts.network_stack.get_selected_candidate().call(stack_opts));

        let blocked_time =
            Duration::from_millis(u64::from(*opts.blocked_reactor_notify_ms.get_value()));
        let csdc = CpuStallDetectorConfig {
            threshold: blocked_time,
            stall_detector_reports_per_minute: *opts.blocked_reactor_reports_per_minute.get_value(),
            oneline: *opts.blocked_reactor_report_format_oneline.get_value(),
            ..CpuStallDetectorConfig::default()
        };
        // SAFETY: the detector is created with the reactor and lives as long as it.
        unsafe { self._cpu_stall_detector.as_deref_mut().unwrap_unchecked() }.update_config(csdc);

        if self._cfg.no_poll_aio {
            self._aio_eventfd = Some(PollableFd::from_file_desc(FileDesc::eventfd(0, 0)?));
        }
        Ok(())
    }

    /// Creates a listening socket bound to `sa` with the options `opts` (for a Unix socket
    /// path, optionally setting its permissions).
    ///
    /// # Errors
    ///
    /// Returns the system error of `socket`, `setsockopt`, `bind`, `listen` or `chmod`.
    pub fn posix_listen(
        &mut self,
        sa: SocketAddress,
        opts: ListenOptions,
    ) -> Result<PollableFd, ExceptionPtr> {
        let mut specific_protocol = opts.proto as i32;
        if sa.is_af_unix() {
            // no type-safe way to create listen_opts with proto=0
            specific_protocol = 0;
        }
        static SOMAXCONN: std::sync::LazyLock<Option<i32>> = std::sync::LazyLock::new(|| {
            match std::fs::read_to_string("/proc/sys/net/core/somaxconn") {
                Ok(s) => {
                    Some(s.split_whitespace().next().and_then(|t| t.parse().ok()).unwrap_or(0))
                }
                Err(_) => None,
            }
        });
        if let Some(somaxconn) = *SOMAXCONN
            && somaxconn < opts.listen_backlog
        {
            println!(
                "Warning: /proc/sys/net/core/somaxconn is set to {} which is lower than the backlog parameter {} used for listen(), please change it with `sysctl -w net.core.somaxconn={}`",
                somaxconn, opts.listen_backlog, opts.listen_backlog
            );
        }

        let fd = FileDesc::socket(
            sa.family() as i32,
            SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC,
            specific_protocol,
        )?;
        if opts.reuse_address {
            fd.setsockopt_int(SOL_SOCKET, SO_REUSEADDR, 1)?;
        }

        if let Some(sndbuf) = opts.so_sndbuf {
            fd.setsockopt_int(SOL_SOCKET, SO_SNDBUF, sndbuf)?;
        }

        if let Some(rcvbuf) = opts.so_rcvbuf {
            fd.setsockopt_int(SOL_SOCKET, SO_RCVBUF, rcvbuf)?;
        }

        let r = (|| -> Result<(), ExceptionPtr> {
            // SAFETY: the address passed is at least as long as the length passed.
            unsafe { fd.bind(sa.as_posix_sockaddr(), sa.length()) }?;

            if sa.is_af_unix()
                && let Some(perms) = opts.unix_domain_socket_permissions
            {
                // After bind the socket is created in the file system.
                let mode = perms.bits() as u32;
                // SAFETY: an AF_UNIX address, so `un` is the active member of the union.
                let path = unsafe { sa.u.un.sun_path.as_ptr() } as *const std::ffi::c_char;
                // SAFETY: `sun_path` is NUL-terminated (the address is zero-filled past
                // the path).
                let result = unsafe { chmod(path, mode) };
                if result < 0 {
                    let errno_copy = errno();
                    // SAFETY: as above.
                    unsafe { unlink(path) };
                    return Err(ExceptionPtr::from(SystemError::new(errno_copy, "chmod failed")));
                }
            }

            fd.listen(opts.listen_backlog)?;
            Ok(())
        })();
        if let Err(e) = r {
            if let Some(s) = e.downcast_ref::<SystemError>() {
                return Err(ExceptionPtr::from(SystemError::new(
                    s.code(),
                    &format!("posix_listen failed for address {}", sa),
                )));
            }
            return Err(e);
        }

        Ok(PollableFd::from_file_desc(fd))
    }
}

impl PollableFdState {
    /// Fails with `ECONNABORTED` if the receive side was shut down.
    pub fn maybe_no_more_recv(&mut self) -> Result<(), ExceptionPtr> {
        if self.shutdown_mask & RCV_SHUTDOWN != 0 {
            return Err(ExceptionPtr::from(SystemError::from_errno(ECONNABORTED)));
        }
        Ok(())
    }
}

impl PollableFdState {
    /// Fails with `ECONNABORTED` if the send side was shut down.
    pub fn maybe_no_more_send(&mut self) -> Result<(), ExceptionPtr> {
        if self.shutdown_mask & SND_SHUTDOWN != 0 {
            return Err(ExceptionPtr::from(SystemError::from_errno(ECONNABORTED)));
        }
        Ok(())
    }

    /// Unregisters the state from the backend, which destroys it.
    ///
    /// # Safety
    /// `this` is the root pointer of a live state without references (no reference to it is
    /// live); it is freed.
    pub unsafe fn forget(this: *mut Self) {
        // SAFETY: the contract above; the backend is reached through its table.
        unsafe {
            let (b, e) = engine_entries();
            (e.forget)(b, this)
        };
    }
}

/// Drops a reference to the state `fd`; the last one makes the backend destroy it.
///
/// # Safety
/// `fd` is the root pointer of a live, reference-counted state, and no reference to it is
/// live.
pub unsafe fn intrusive_ptr_release(fd: *mut PollableFdState) {
    // SAFETY: the contract above.
    unsafe {
        (*fd)._refs -= 1;
        if (*fd)._refs == 0 {
            PollableFdState::forget(fd);
        }
    }
}

impl PollableFd {
    /// Wraps `fd` in a state made by the current shard's backend, with speculation
    /// `speculate`.
    pub fn new(fd: FileDesc, speculate: Speculation) -> Self {
        // SAFETY: on this shard's reactor thread; the backend is reached through its table.
        let s = unsafe {
            let (b, e) = engine_entries();
            (e.make_pollable_fd_state)(b, fd, speculate)
        };
        PollableFd::from_state(s)
    }

    /// Shuts down part of the connection (`SHUT_RD`, `SHUT_WR` or `SHUT_RDWR`); unless
    /// `kernel_only`, the state records it so later operations fail.
    ///
    /// # Errors
    ///
    /// Returns the backend's shutdown error.
    pub fn shutdown(
        &mut self,
        how: i32,
        kernel_only: crate::engine::core::internal::pollable_fd::ShutdownKernelOnly,
    ) -> Result<(), ExceptionPtr> {
        if !kernel_only.0 {
            // TCP will respond to shutdown() by returning ECONNABORT on the next IO,
            // but UDP responds by returning AGAIN. The shutdown_mask tells us to convert
            // EAGAIN to ECONNABORT in that case.
            // SAFETY: the state is live while `self` is; one field write.
            unsafe { (*self.state_ptr()).shutdown_mask |= shutdown_mask(how) };
        }
        // SAFETY: on this shard's reactor thread; the state is live while `self` is, and is
        // passed by its root pointer to the backend's table.
        unsafe {
            let (b, e) = engine_entries();
            (e.shutdown)(b, self.state_ptr(), how)
        }
    }
}

/// Socket option level: IP.
pub const SOL_IP: i32 = 0;
/// IP option: bind without reserving a port.
pub const IP_BIND_ADDRESS_NO_PORT: i32 = 24;
/// errno: protocol option not available.
pub const ENOPROTOOPT: i32 = 92;

impl Reactor {
    /// A non-blocking stream socket for the family of `sa`, driven by the reactor.
    ///
    /// # Errors
    ///
    /// Returns the system error of `socket`.
    pub fn make_pollable_fd(
        &mut self,
        sa: SocketAddress,
        proto: i32,
    ) -> Result<PollableFd, ExceptionPtr> {
        let maybe_nonblock = if self.backend().do_blocking_io() { 0 } else { SOCK_NONBLOCK };
        let fd = FileDesc::socket(
            sa.family() as i32,
            SOCK_STREAM | maybe_nonblock | SOCK_CLOEXEC,
            proto,
        )?;
        Ok(PollableFd::from_file_desc(fd))
    }

    /// Connects `pfd` to `sa`, binding it to `local` first when given.
    pub fn posix_connect(
        &mut self,
        mut pfd: PollableFd,
        mut sa: SocketAddress,
        local: SocketAddress,
    ) -> Future<()> {
        if !sa.is_af_unix() {
            // do not reserve an ephemeral port when using bind() with port number 0.
            // connect() will handle it later. The reason for that is that bind() may fail
            // to allocate a port while connect will success, this is because bind() does not
            // know dst address and has to find globally unique local port.
            if let Err(err) = pfd.get_file_desc().setsockopt_int(SOL_IP, IP_BIND_ADDRESS_NO_PORT, 1)
            {
                let is_enoprotoopt =
                    err.downcast_ref::<SystemError>().is_some_and(|e| e.code() == ENOPROTOOPT);
                if !is_enoprotoopt {
                    return make_exception_future(err);
                }
            }
        }
        if !local.is_wildcard() {
            // call bind() only if local address is not wildcard
            // SAFETY: the address passed is at least as long as the length passed.
            fut_try!(unsafe {
                pfd.get_file_desc().bind(local.as_posix_sockaddr(), local.length())
            });
        }
        let keep = pfd.clone();
        pfd.connect(&mut sa).finally(move || {
            drop(keep);
        })
    }

    /// Listens on `sa` with the network stack.
    ///
    /// # Errors
    ///
    /// Returns the stack's listen error.
    pub fn listen(
        &mut self,
        sa: SocketAddress,
        opt: ListenOptions,
    ) -> Result<ServerSocket, ExceptionPtr> {
        self.net().listen(sa, opt)
    }

    /// Connects to `sa` over TCP with the network stack.
    pub fn connect(&mut self, sa: SocketAddress) -> Future<ConnectedSocket> {
        // The defaults: any local address and TCP.
        self.net().connect(sa, SocketAddress::default(), Transport::TCP)
    }

    /// Connects to `sa` from `local` with transport `proto`, with the network stack.
    pub fn connect_from(
        &mut self,
        sa: SocketAddress,
        local: SocketAddress,
        proto: Transport,
    ) -> Future<ConnectedSocket> {
        self.net().connect(sa, local, proto)
    }
}

/// Delivers a kernel result to an I/O completion.
///
/// # Safety
///
/// `this` must point to a live `IoCompletion`; the call may destroy it.
unsafe fn io_completion_complete_with_impl(this: *mut IoCompletion, res: isize) {
    if res >= 0 {
        // SAFETY: forwarded from the caller.
        unsafe { IoCompletion::complete(this, res as usize) };
        return;
    }

    // SAFETY: on this shard's reactor thread; a short access to a counter.
    unsafe { (*engine())._io_stats.aio_errors += 1 };
    // A negative `res` is always an error.
    let e = crate::engine::core::posix::kernel_error(res);
    // SAFETY: forwarded from the caller.
    unsafe { IoCompletion::set_exception(this, e) };
}

impl Reactor {
    /// Submits the disk requests queued in the I/O sink; returns `true` if any were.
    ///
    /// # Safety
    /// `this` is the live reactor of this thread, with no reference to it live.
    pub(crate) unsafe fn flush_pending_aio(this: *mut Self) -> bool {
        // SAFETY: guaranteed by the caller. Dispatching only queues requests in the sink
        // (it runs no user code), so the map is not changed during the loop; each queue is
        // engaged and reached through its own pointer only here.
        unsafe {
            for ioq in (*std::ptr::addr_of!((*this)._io_queues)).values() {
                IoQueue::poll_io_queue(io_queue_ptr(ioq));
            }
        }
        false
    }

    /// When the next I/O queue needs to dispatch again.
    pub(crate) fn next_pending_aio(
        &self,
    ) -> <SteadyClockType as crate::engine::core::timer::Clock>::TimePoint {
        let mut next = SteadyTimePoint::MAX;

        for ioq in self._io_queues.values() {
            // SAFETY: the queues in the map are engaged and live; a shared access through the
            // root pointer.
            next = next.min(SteadyTimePoint(unsafe { (*io_queue_ptr(ioq)).next_pending_aio() }));
        }

        next
    }

    /// Completes the finished asynchronous work; returns `true` if any.
    ///
    /// # Safety
    /// As [`Reactor::flush_pending_aio`]: the completions resolve promises.
    pub(crate) unsafe fn reap_kernel_completions(this: *mut Self) -> bool {
        // SAFETY: guaranteed by the caller.
        unsafe {
            let (b, e) = Self::backend_entries(this);
            (e.reap_kernel_completions)(b)
        }
    }

    /// The backend's root pointer and its table of entries.
    ///
    /// # Safety
    /// As [`Reactor::backend_of`].
    #[inline]
    pub(crate) unsafe fn backend_entries(
        this: *mut Self,
    ) -> (*mut (), &'static crate::engine::core::reactor_backend::BackendEntries) {
        // SAFETY: guaranteed by the caller; the reference to the field ends at once.
        unsafe {
            let (b, e) = (*std::ptr::addr_of!((*this)._backend)).as_ref().unwrap_unchecked();
            (b.as_ptr().cast::<()>(), *e)
        }
    }

    /// The backend, as a raw pointer: the
    /// backend calls out to completions that re-enter the reactor.
    ///
    /// # Safety
    /// `this` is the live reactor of this thread; the backend is set by
    /// `construct_in_place` before anything else runs and is cleared only by the
    /// destructor.
    #[inline]
    pub(crate) unsafe fn backend_of(this: *mut Self) -> *mut dyn ReactorBackend {
        // SAFETY: guaranteed by the caller; the reference to the field ends at once, and the
        // pointer is the node's (the backend's root pointer).
        unsafe { (*std::ptr::addr_of!((*this)._backend)).as_ref().unwrap_unchecked().0.as_ptr() }
    }

    /// The backend, for the trait's methods, which only read it.
    #[inline]
    pub(crate) fn backend(&self) -> &dyn ReactorBackend {
        // SAFETY: the backend is set for the reactor's whole life (see backend_of); the
        // shared reference is tied to the reactor's borrow, and the trait's methods run no
        // completion.
        unsafe { &*Self::backend_of(self as *const Self as *mut Self) }
    }
}

/// Iovec helpers for disk I/O.
pub mod internal_iovecs {
    use super::*;

    /// Truncates `iov` to `IOV_MAX` entries and its total length to a multiple of
    /// `disk_alignment`; returns the resulting length in bytes.
    pub fn sanitize_iovecs(iov: &mut Vec<Iovec>, disk_alignment: usize) -> usize {
        if iov.len() > IOV_MAX {
            iov.truncate(IOV_MAX);
        }
        let mut length = iovec_len(iov);
        loop {
            let rest = length & (disk_alignment - 1);
            if rest == 0 {
                break;
            }
            if iov.last().unwrap().iov_len <= rest {
                length -= iov.last().unwrap().iov_len;
                iov.pop();
            } else {
                iov.last_mut().unwrap().iov_len -= rest;
                length -= rest;
            }
        }
        length
    }
}

/// File open flags.
pub use crate::engine::core::internal::linux_abi::{
    O_CLOEXEC, O_DIRECT, O_DIRECTORY, O_DSYNC, O_NONBLOCK, O_RDONLY,
};
/// File status structures.
pub use crate::engine::core::posix::{Stat, Statfs, Statvfs};
/// `fcntl` command: set the file status flags.
pub const F_SETFL: i32 = 4;
/// errno: no such file or directory.
pub const ENOENT: i32 = 2;
/// errno: not a directory.
pub const ENOTDIR: i32 = 20;
/// errno: file exists.
pub const EEXIST: i32 = 17;
/// File type mask of `st_mode`.
pub const S_IFMT: u32 = 0o170000;
/// File type: directory.
pub const S_IFDIR: u32 = 0o040000;
/// File type: block device.
pub const S_IFBLK: u32 = 0o060000;
/// File type: character device.
pub const S_IFCHR: u32 = 0o020000;
/// File type: FIFO.
pub const S_IFIFO: u32 = 0o010000;
/// File type: symbolic link.
pub const S_IFLNK: u32 = 0o120000;
/// File type: socket.
pub const S_IFSOCK: u32 = 0o140000;
/// File type: regular file.
pub const S_IFREG: u32 = 0o100000;
/// XFS `ioctl`: read the extended attributes.
pub const XFS_IOC_FSGETXATTR: u64 = 0x801c581f;
/// XFS `ioctl`: write the extended attributes.
pub const XFS_IOC_FSSETXATTR: u64 = 0x401c5820;
/// XFS flag: the extent size hint is set.
pub const XFS_XFLAG_EXTSIZE: u32 = 0x00000800;
/// The `getdents64` and `pidfd_open` system call numbers.
pub use crate::engine::core::internal::linux_abi::nr::{
    GETDENTS64 as NR_GETDENTS64, PIDFD_OPEN as NR_PIDFD_OPEN,
};
/// `waitpid` flag: do not block.
pub const WNOHANG: i32 = 1;
/// `ioctl`: set non-blocking mode.
pub const FIONBIO: u64 = 0x5421;
/// Standard input descriptor.
pub const STDIN_FILENO: i32 = 0;
/// Standard output descriptor.
pub const STDOUT_FILENO: i32 = 1;
/// Standard error descriptor.
pub const STDERR_FILENO: i32 = 2;
/// Interrupt signal.
pub const SIGINT: i32 = 2;
/// Termination signal.
pub const SIGTERM: i32 = 15;
/// Hang-up signal.
pub const SIGHUP: i32 = 1;
/// `posix_spawn` flag: reset the listed signals to their default handling.
pub const POSIX_SPAWN_SETSIGDEF: i16 = 0x04;
/// `posix_spawn` flag: set the child's signal mask.
pub const POSIX_SPAWN_SETSIGMASK: i16 = 0x08;

/// The extended file attributes of the XFS `ioctl`s (`struct fsxattr`).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Fsxattr {
    /// `XFS_XFLAG_*` flags.
    pub fsx_xflags: u32,
    /// Extent size hint.
    pub fsx_extsize: u32,
    /// Number of extents.
    pub fsx_nextents: u32,
    /// Project id.
    pub fsx_projid: u32,
    /// Copy-on-write extent size hint.
    pub fsx_cowextsize: u32,
    /// Padding.
    pub fsx_pad: [u8; 8],
}

/// A group database entry (`struct group`).
#[repr(C)]
pub struct Group {
    /// Group name.
    pub gr_name: *mut std::ffi::c_char,
    /// Group password.
    pub gr_passwd: *mut std::ffi::c_char,
    /// Group id.
    pub gr_gid: u32,
    /// Member names, null-terminated.
    pub gr_mem: *mut *mut std::ffi::c_char,
}

/// `posix_spawn_file_actions_t` (opaque, 80 bytes in glibc).
#[repr(C, align(8))]
pub struct PosixSpawnFileActionsT(pub [u8; 80]);
/// `posix_spawnattr_t` (opaque, 336 bytes in glibc).
#[repr(C, align(8))]
pub struct PosixSpawnattrT(pub [u8; 336]);

unsafe extern "C" {
    fn open(path: *const std::ffi::c_char, flags: i32, ...) -> i32;
    fn close(fd: i32) -> i32;
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    fn ioctl(fd: i32, req: u64, ...) -> i32;
    fn fstat(fd: i32, st: *mut Stat) -> i32;
    fn stat(path: *const std::ffi::c_char, st: *mut Stat) -> i32;
    fn lstat(path: *const std::ffi::c_char, st: *mut Stat) -> i32;
    fn fstatfs(fd: i32, buf: *mut Statfs) -> i32;
    fn remove(path: *const std::ffi::c_char) -> i32;
    fn rename(old: *const std::ffi::c_char, new: *const std::ffi::c_char) -> i32;
    fn link(old: *const std::ffi::c_char, new: *const std::ffi::c_char) -> i32;
    #[link_name = "chmod"]
    fn libc_chmod(path: *const std::ffi::c_char, mode: u32) -> i32;
    #[link_name = "inotify_add_watch"]
    fn libc_inotify_add_watch(fd: i32, path: *const std::ffi::c_char, mask: u32) -> i32;
    fn pipe2(fds: *mut i32, flags: i32) -> i32;
    fn posix_spawn_file_actions_init(a: *mut PosixSpawnFileActionsT) -> i32;
    fn posix_spawn_file_actions_destroy(a: *mut PosixSpawnFileActionsT) -> i32;
    fn posix_spawnattr_init(a: *mut PosixSpawnattrT) -> i32;
    fn posix_spawnattr_destroy(a: *mut PosixSpawnattrT) -> i32;
    fn posix_spawnattr_setsigdefault(a: *mut PosixSpawnattrT, s: *const SigsetT) -> i32;
    fn posix_spawnattr_setsigmask(a: *mut PosixSpawnattrT, s: *const SigsetT) -> i32;
    fn posix_spawnattr_setflags(a: *mut PosixSpawnattrT, flags: i16) -> i32;
    fn posix_spawn(
        pid: *mut i32,
        path: *const std::ffi::c_char,
        a: *const PosixSpawnFileActionsT,
        at: *const PosixSpawnattrT,
        argv: *const *const std::ffi::c_char,
        envp: *const *const std::ffi::c_char,
    ) -> i32;
    #[link_name = "waitpid"]
    fn libc_waitpid(pid: i32, wstatus: *mut i32, options: i32) -> i32;
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
    fn getgrnam_r(
        name: *const std::ffi::c_char,
        grp: *mut Group,
        buf: *mut std::ffi::c_char,
        buflen: usize,
        result: *mut *mut Group,
    ) -> i32;
    #[link_name = "chown"]
    fn libc_chown(path: *const std::ffi::c_char, owner: u32, group: u32) -> i32;
    fn mkdir(path: *const std::ffi::c_char, mode: u32) -> i32;
    #[link_name = "access"]
    fn libc_access(path: *const std::ffi::c_char, mode: i32) -> i32;
    #[link_name = "statvfs"]
    fn libc_statvfs(path: *const std::ffi::c_char, buf: *mut Statvfs) -> i32;
    #[link_name = "statfs"]
    fn libc_statfs(path: *const std::ffi::c_char, buf: *mut Statfs) -> i32;
    fn fdatasync(fd: i32) -> i32;
}

#[inline]
fn cstr(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).unwrap_or_default()
}

use crate::engine::core::align::align_up;
use crate::engine::core::syscall_result::{
    SyscallResult, SyscallResultExtra, wrap_syscall, wrap_syscall_extra,
};

impl Reactor {
    /// Opens `nameref` for DMA I/O with `flags` and `options` (on the syscall thread),
    /// falling back to buffered I/O when allowed.
    pub fn open_file_dma(
        &mut self,
        nameref: &str,
        flags: OpenFlags,
        options: FileOpenOptions,
    ) -> Future<File> {
        let name = Sstring::from(nameref);
        let strict_o_direct = self._cfg.strict_o_direct;
        let bypass_fsync = self._cfg.bypass_fsync;
        let kernel_page_cache = self._cfg.kernel_page_cache;
        // The flags are adjusted (direct I/O, data sync) before the file is made; the result
        // `make_file_impl` reads them; the result does not depend on the thread, so it is
        // computed once here and both closures get it.
        let mut open_flags: i32 = flags.bits();
        open_flags |= O_CLOEXEC;
        if bypass_fsync {
            open_flags &= !O_DSYNC;
        }
        let opts = options;
        let name2 = name.clone();
        self.thread_pool()
            .submit::<SyscallResultExtra<Stat>, _>(move || {
                // We want O_DIRECT, except in three cases:
                //   - tmpfs (which doesn't support it, but works fine anyway)
                //   - strict_o_direct == false (where we forgive it being not supported)
                //   - kernel_page_cache == true (where we disable it for short-lived test processes)
                // Because open() with O_DIRECT will fail, we open it without O_DIRECT, try
                // to update it to O_DIRECT with fcntl(), and if that fails, see if we
                // can forgive it.
                let is_tmpfs = |fd: i32| -> bool {
                    // SAFETY: `Statfs` is plain data; fstatfs fills it.
                    let mut buf: Statfs = unsafe { std::mem::zeroed() };
                    // SAFETY: `buf` is a valid, writable `Statfs`; an invalid `fd` only makes the call fail.
                    let r = unsafe { fstatfs(fd, &mut buf) };
                    if r == -1 {
                        return false;
                    }
                    buf.f_type as u64
                        == crate::engine::util::internal::magic::internal::FsMagic::TMPFS
                };
                // SAFETY: `Stat` is plain data.
                let mut st: Stat = unsafe { std::mem::zeroed() };
                let mode = opts.create_permissions.bits() as u32;
                let cname = cstr(name2.as_str());
                // SAFETY: `cname` is a NUL-terminated path.
                let fd = unsafe { open(cname.as_ptr(), open_flags, mode) };
                if fd == -1 {
                    return wrap_syscall_extra(fd as i64, st);
                }
                let mut close_fd = crate::engine::util::defer::defer(move || {
                    // SAFETY: closing the descriptor opened above.
                    unsafe { close(fd) };
                });
                let o_direct_flag = if kernel_page_cache { 0 } else { O_DIRECT };
                // SAFETY: plain fcntl on our descriptor.
                let mut r = unsafe { fcntl(fd, F_SETFL, open_flags | o_direct_flag) };
                if r == -1 && strict_o_direct {
                    let maybe_ret = wrap_syscall_extra(r as i64, st); // capture errno (should be EINVAL)
                    if !is_tmpfs(fd) {
                        return maybe_ret;
                    }
                }
                if fd != -1 && opts.extent_allocation_size_hint != 0 && !kernel_page_cache {
                    let mut attr = Fsxattr::default();
                    // SAFETY: `attr` is a valid fsxattr.
                    let r = unsafe { ioctl(fd, XFS_IOC_FSGETXATTR, &mut attr as *mut Fsxattr) };
                    // xfs delayed allocation is disabled when extent size hints are present.
                    // This causes tons of xfs log fsyncs. Given that extent size hints are
                    // unneeded when delayed allocation is available (which is the case
                    // when not using O_DIRECT), disable them.
                    //
                    // Ignore error; may be !xfs, and just a hint anyway
                    if r != -1 {
                        attr.fsx_xflags |= XFS_XFLAG_EXTSIZE;
                        attr.fsx_extsize = opts
                            .extent_allocation_size_hint
                            .min(FileOpenOptions::MAX_EXTENT_ALLOCATION_SIZE_HINT)
                            as u32;

                        attr.fsx_extsize = align_up::<u32>(
                            attr.fsx_extsize,
                            FileOpenOptions::MIN_EXTENT_SIZE_HINT_ALIGNMENT,
                        );

                        // Ignore error; may be !xfs, and just a hint anyway
                        // SAFETY: as above.
                        unsafe { ioctl(fd, XFS_IOC_FSSETXATTR, &attr as *const Fsxattr) };
                    }
                }
                // SAFETY: `st` is a valid stat buffer.
                r = unsafe { fstat(fd, &mut st) };
                if r == -1 {
                    return wrap_syscall_extra(r as i64, st);
                }
                close_fd.cancel();
                wrap_syscall_extra(fd as i64, st)
            })
            .then(move |sr: SyscallResultExtra<Stat>| {
                fut_try!(sr.throw_fs_exception_if_error("open failed", name.as_str()));
                crate::engine::core::file::make_file_impl(sr.result, options, open_flags, sr.extra)
            })
            .then(|impl_: SharedPtr<dyn FileImpl>| make_ready_future(File::from_impl(impl_)))
    }

    /// Removes the file or empty directory `pathname` (on the syscall thread).
    pub fn remove_file(&mut self, pathname: &str) -> Future<()> {
        // Allocating memory for a string can fail, hence the futurize_invoke.
        let p1 = Sstring::from(pathname);
        let p2 = p1.clone();
        self.thread_pool()
            .submit::<SyscallResult<i32>, _>(move || {
                let c = cstr(p1.as_str());
                // SAFETY: NUL-terminated path.
                wrap_syscall::<i32>(unsafe { remove(c.as_ptr()) })
            })
            .then(move |sr: SyscallResult<i32>| {
                fut_try!(sr.throw_fs_exception_if_error("remove failed", p2.as_str()));
                make_ready_future(())
            })
    }

    /// Renames `old_pathname` to `new_pathname` (on the syscall thread).
    pub fn rename_file(&mut self, old_pathname: &str, new_pathname: &str) -> Future<()> {
        // Allocating memory for a sstring can throw, hence the futurize_invoke
        let (o1, n1) = (Sstring::from(old_pathname), Sstring::from(new_pathname));
        let (o2, n2) = (o1.clone(), n1.clone());
        self.thread_pool()
            .submit::<SyscallResult<i32>, _>(move || {
                let (co, cn) = (cstr(o1.as_str()), cstr(n1.as_str()));
                // SAFETY: NUL-terminated paths.
                wrap_syscall::<i32>(unsafe { rename(co.as_ptr(), cn.as_ptr()) })
            })
            .then(move |sr: SyscallResult<i32>| {
                fut_try!(sr.throw_fs_exception_if_error2(
                    "rename failed",
                    o2.as_str(),
                    n2.as_str()
                ));
                make_ready_future(())
            })
    }

    /// Creates the hard link `newpath` to `oldpath` (on the syscall thread).
    pub fn link_file(&mut self, oldpath: &str, newpath: &str) -> Future<()> {
        // Allocating memory for a sstring can throw, hence the futurize_invoke
        let (o1, n1) = (Sstring::from(oldpath), Sstring::from(newpath));
        let (o2, n2) = (o1.clone(), n1.clone());
        self.thread_pool()
            .submit::<SyscallResult<i32>, _>(move || {
                let (co, cn) = (cstr(o1.as_str()), cstr(n1.as_str()));
                // SAFETY: NUL-terminated paths.
                wrap_syscall::<i32>(unsafe { link(co.as_ptr(), cn.as_ptr()) })
            })
            .then(move |sr: SyscallResult<i32>| {
                fut_try!(sr.throw_fs_exception_if_error2("link failed", o2.as_str(), n2.as_str()));
                make_ready_future(())
            })
    }

    /// Sets the permissions of `name` (on the syscall thread).
    pub fn chmod(&mut self, name: &str, permissions: FilePermissions) -> Future<()> {
        let mode = permissions.bits() as u32;
        // Allocating memory for a sstring can throw, hence the futurize_invoke
        let n1 = Sstring::from(name);
        let n2 = n1.clone();
        self.thread_pool()
            .submit::<SyscallResult<i32>, _>(move || {
                let c = cstr(n1.as_str());
                // SAFETY: NUL-terminated path.
                wrap_syscall::<i32>(unsafe { libc_chmod(c.as_ptr(), mode) })
            })
            .then(move |sr: SyscallResult<i32>| {
                if sr.result == -1 {
                    let reason = format!("chmod(0{:o}) failed", mode);
                    return make_exception_future(
                        sr.throw_fs_exception(&reason, std::path::Path::new(n2.as_str())),
                    );
                }
                make_ready_future(())
            })
    }
}

/// The directory entry type of a `st_mode` file type.
pub fn stat_to_entry_type(type_: u32) -> DirectoryEntryType {
    match type_ & S_IFMT {
        S_IFDIR => DirectoryEntryType::Directory,
        S_IFBLK => DirectoryEntryType::BlockDevice,
        S_IFCHR => DirectoryEntryType::CharDevice,
        S_IFIFO => DirectoryEntryType::Fifo,
        S_IFLNK => DirectoryEntryType::Link,
        S_IFSOCK => DirectoryEntryType::Socket,
        S_IFREG => DirectoryEntryType::Regular,
        _ => DirectoryEntryType::Unknown,
    }
}

impl Reactor {
    /// The type of the file `name`, or `None` if it does not exist (on the syscall thread);
    /// symbolic links are followed if `follow`.
    pub fn file_type(
        &mut self,
        name: &str,
        follow: FollowSymlink,
    ) -> Future<Option<DirectoryEntryType>> {
        // Allocating memory for a sstring can throw, hence the futurize_invoke
        let n1 = Sstring::from(name);
        let n2 = n1.clone();
        self.thread_pool()
            .submit::<SyscallResultExtra<Stat>, _>(move || {
                // SAFETY: `Stat` is plain data.
                let mut st: Stat = unsafe { std::mem::zeroed() };
                let c = cstr(n1.as_str());
                // SAFETY: NUL-terminated path and a valid stat buffer.
                let ret = unsafe {
                    if follow.as_bool() {
                        stat(c.as_ptr(), &mut st)
                    } else {
                        lstat(c.as_ptr(), &mut st)
                    }
                };
                wrap_syscall_extra(ret as i64, st)
            })
            .then(move |sr: SyscallResultExtra<Stat>| {
                if sr.result as i64 == -1 {
                    if sr.error != ENOENT && sr.error != ENOTDIR {
                        fut_try!(sr.throw_fs_exception_if_error("stat failed", n2.as_str()));
                    }
                    return make_ready_future(None);
                }
                make_ready_future(Some(stat_to_entry_type(sr.extra.st_mode)))
            })
    }
}

/// The type of the file `name` (see [`Reactor::file_type`]).
pub fn file_type(name: &str, follow: FollowSymlink) -> Future<Option<DirectoryEntryType>> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).file_type(name, follow) }
}

fn timespec_to_time_point(ts: &crate::engine::core::posix::Timespec) -> std::time::SystemTime {
    let d = Duration::from_secs(ts.tv_sec as u64) + Duration::from_nanos(ts.tv_nsec as u64);
    std::time::UNIX_EPOCH + d
}

fn next_waitpid_timeout(this_timeout: Duration) -> Duration {
    const STEP_TIMEOUT: Duration = Duration::from_millis(20);
    const MAX_TIMEOUT: Duration = Duration::from_millis(1000);
    if this_timeout >= MAX_TIMEOUT {
        return MAX_TIMEOUT;
    }
    this_timeout + STEP_TIMEOUT
}

impl Reactor {
    /// Reads directory entries of `fd` into `buffer` with `getdents64` (on the syscall
    /// thread); resolves with the number of bytes read.
    pub fn read_directory(
        &mut self,
        fd: i32,
        buffer: *mut u8,
        buffer_size: usize,
    ) -> Future<usize> {
        let buf = buffer as usize;
        self.thread_pool()
            .submit::<SyscallResult<i64>, _>(move || {
                // SAFETY: `buffer` holds `buffer_size` bytes owned by the caller until the future resolves.
                let ret = unsafe { syscall(NR_GETDENTS64, fd, buf as *mut u8, buffer_size) };
                wrap_syscall::<i64>(ret)
            })
            .then(|ret: SyscallResult<i64>| {
                fut_try!(ret.throw_if_error());
                make_ready_future(ret.result as usize)
            })
    }

    /// Adds an inotify watch on `path` (on the syscall thread); resolves with the watch
    /// descriptor.
    pub fn inotify_add_watch(&mut self, fd: i32, path: &str, flags: u32) -> Future<i32> {
        // Allocating memory for a string can fail, hence the futurize_invoke.
        let p = Sstring::from(path);
        self.thread_pool()
            .submit::<SyscallResult<i32>, _>(move || {
                let c = cstr(p.as_str());
                // SAFETY: NUL-terminated path.
                let ret = unsafe { libc_inotify_add_watch(fd, c.as_ptr(), flags) };
                wrap_syscall::<i32>(ret)
            })
            .then(|ret: SyscallResult<i32>| {
                fut_try!(ret.throw_if_error());
                make_ready_future(ret.result)
            })
    }

    /// Creates a pipe (on the syscall thread); resolves with its read and write ends.
    pub fn make_pipe(&mut self) -> Future<(FileDesc, FileDesc)> {
        // The descriptor array travels with the system call's result.
        self.thread_pool()
            .submit::<(SyscallResult<i32>, [i32; 2]), _>(move || {
                let mut pipe = [0i32; 2];
                // SAFETY: `pipe` holds two ints.
                let r = wrap_syscall::<i32>(unsafe { pipe2(pipe.as_mut_ptr(), O_NONBLOCK) });
                (r, pipe)
            })
            .then(move |(ret, pipe): (SyscallResult<i32>, [i32; 2])| {
                fut_try!(ret.throw_if_error());
                make_ready_future((FileDesc::from_fd(pipe[0]), FileDesc::from_fd(pipe[1])))
            })
    }

    /// Spawns `pathname` with arguments `argv` and environment `env`; resolves with the
    /// child's pid and its standard input, output and error pipes.
    pub fn spawn(
        &mut self,
        pathname: &str,
        argv: Vec<Sstring>,
        env: Vec<Sstring>,
    ) -> Future<(i32, FileDesc, FileDesc, FileDesc)> {
        let this = self as *mut Reactor;
        let pathname = Sstring::from(pathname);
        crate::engine::core::coroutine::make_coroutine(async move {
            // SAFETY: the reactor outlives every future it runs.
            let me = unsafe { &mut *this };
            let (cin_pipe, cout_pipe, cerr_pipe) = crate::engine::core::when_all::when_all_succeed(
                (me.make_pipe(), me.make_pipe(), me.make_pipe()),
            )
            .await?;
            const PIPEFD_READ_END: usize = 0;
            const PIPEFD_WRITE_END: usize = 1;
            let cin = [cin_pipe.0, cin_pipe.1];
            let cout = [cout_pipe.0, cout_pipe.1];
            let cerr = [cerr_pipe.0, cerr_pipe.1];
            let mut actions = Box::new(PosixSpawnFileActionsT([0; 80]));
            let mut attr = Box::new(PosixSpawnattrT([0; 336]));
            let mut child_pid: i32 = 0;
            // Allocating memory for spawn {file actions,attributes} objects can throw, hence the futurize_invoke
            let setup =
                (|| -> Result<(Vec<std::ffi::CString>, Vec<std::ffi::CString>), ExceptionPtr> {
                    // the args and envs parameters passed to posix_spawn() should be array of pointers, and
                    // the last one should be a null pointer.
                    let argvc: Vec<std::ffi::CString> =
                        argv.iter().map(|s| cstr(s.as_str())).collect();
                    let envc: Vec<std::ffi::CString> =
                        env.iter().map(|s| cstr(s.as_str())).collect();

                    // SAFETY: `actions` and `attr` are storage for the opaque glibc objects.
                    let mut r = unsafe { posix_spawn_file_actions_init(&mut *actions) };
                    crate::engine::core::posix::throw_pthread_error(r)?;
                    // the child process does not write to stdin
                    // SAFETY: `actions` was initialized by posix_spawn_file_actions_init.
                    unsafe { cin[PIPEFD_WRITE_END].spawn_actions_add_close(&mut *actions) }?;
                    // the child process does not read from stdout
                    // SAFETY: `actions` was initialized by posix_spawn_file_actions_init.
                    unsafe { cout[PIPEFD_READ_END].spawn_actions_add_close(&mut *actions) }?;
                    // the child process does not read from stderr
                    // SAFETY: `actions` was initialized by posix_spawn_file_actions_init.
                    unsafe { cerr[PIPEFD_READ_END].spawn_actions_add_close(&mut *actions) }?;
                    // redirect stdin, stdout and stderr to cin_pipe, cout_pipe and cerr_pipe respectively
                    // SAFETY: `actions` was initialized by posix_spawn_file_actions_init.
                    unsafe {
                        cin[PIPEFD_READ_END].spawn_actions_add_dup2(&mut *actions, STDIN_FILENO)
                    }?;
                    // SAFETY: `actions` was initialized by posix_spawn_file_actions_init.
                    unsafe {
                        cout[PIPEFD_WRITE_END].spawn_actions_add_dup2(&mut *actions, STDOUT_FILENO)
                    }?;
                    // SAFETY: `actions` was initialized by posix_spawn_file_actions_init.
                    unsafe {
                        cerr[PIPEFD_WRITE_END].spawn_actions_add_dup2(&mut *actions, STDERR_FILENO)
                    }?;
                    // after dup2() the interesting ends of pipes, close them
                    // SAFETY: `actions` was initialized by posix_spawn_file_actions_init.
                    unsafe { cin[PIPEFD_READ_END].spawn_actions_add_close(&mut *actions) }?;
                    // SAFETY: `actions` was initialized by posix_spawn_file_actions_init.
                    unsafe { cout[PIPEFD_WRITE_END].spawn_actions_add_close(&mut *actions) }?;
                    // SAFETY: `actions` was initialized by posix_spawn_file_actions_init.
                    unsafe { cerr[PIPEFD_WRITE_END].spawn_actions_add_close(&mut *actions) }?;
                    // tools like "cat" expect a fd opened in blocking mode when performing I/O
                    // `FIONBIO` with an int argument passed by address.
                    // SAFETY: FIONBIO reads one int from the address it is given.
                    unsafe { cin[PIPEFD_READ_END].ioctl_data(FIONBIO, &mut 0i32) }?;
                    // SAFETY: as above.
                    unsafe { cout[PIPEFD_WRITE_END].ioctl_data(FIONBIO, &mut 0i32) }?;
                    // SAFETY: as above.
                    unsafe { cerr[PIPEFD_WRITE_END].ioctl_data(FIONBIO, &mut 0i32) }?;

                    // SAFETY: as above.
                    r = unsafe { posix_spawnattr_init(&mut *attr) };
                    crate::engine::core::posix::throw_pthread_error(r)?;
                    // make sure the following signals are not ignored by the child process
                    let mut default_signals = SigsetT::default();
                    // SAFETY: valid signal set and attribute storage.
                    unsafe {
                        sigemptyset(&mut default_signals);
                        sigaddset(&mut default_signals, SIGINT);
                        sigaddset(&mut default_signals, SIGTERM);
                        r = posix_spawnattr_setsigdefault(&mut *attr, &default_signals);
                    }
                    crate::engine::core::posix::throw_pthread_error(r)?;
                    // make sure no signals are marked in the child process
                    let mut mask_signals = SigsetT::default();
                    // SAFETY: as above.
                    unsafe {
                        sigemptyset(&mut mask_signals);
                        r = posix_spawnattr_setsigmask(&mut *attr, &mask_signals);
                    }
                    crate::engine::core::posix::throw_pthread_error(r)?;
                    // SAFETY: as above.
                    r = unsafe {
                        posix_spawnattr_setflags(
                            &mut *attr,
                            POSIX_SPAWN_SETSIGDEF | POSIX_SPAWN_SETSIGMASK,
                        )
                    };
                    crate::engine::core::posix::throw_pthread_error(r)?;
                    Ok((argvc, envc))
                })();
            let result = match setup {
                Ok((argvc, envc)) => {
                    let cpath = cstr(pathname.as_str());
                    let actions_p = &*actions as *const PosixSpawnFileActionsT as usize;
                    let attr_p = &*attr as *const PosixSpawnattrT as usize;
                    let pid_p = &mut child_pid as *mut i32 as usize;
                    me.thread_pool()
                        .submit::<SyscallResult<i32>, _>(move || {
                            let mut argvp: Vec<*const std::ffi::c_char> =
                                argvc.iter().map(|s| s.as_ptr()).collect();
                            argvp.push(std::ptr::null());
                            let mut envp: Vec<*const std::ffi::c_char> =
                                envc.iter().map(|s| s.as_ptr()).collect();
                            envp.push(std::ptr::null());
                            // SAFETY: every pointer refers to storage kept alive by the awaiting coroutine.
                            wrap_syscall::<i32>(unsafe {
                                posix_spawn(
                                    pid_p as *mut i32,
                                    cpath.as_ptr(),
                                    actions_p as *const PosixSpawnFileActionsT,
                                    attr_p as *const PosixSpawnattrT,
                                    argvp.as_ptr(),
                                    envp.as_ptr(),
                                )
                            })
                        })
                        .await
                }
                Err(e) => Err(e),
            };
            // finally
            // SAFETY: destroying the objects initialized above (destroy on zeroed storage is harmless in glibc).
            unsafe {
                posix_spawn_file_actions_destroy(&mut *actions);
                posix_spawnattr_destroy(&mut *attr);
            }
            let ret = result?;
            crate::engine::core::posix::throw_pthread_error(ret.result)?;
            let [_cin_r, cin_w] = cin;
            let [cout_r, _cout_w] = cout;
            let [cerr_r, _cerr_w] = cerr;
            Ok((child_pid, cin_w, cout_r, cerr_r))
        })
    }

    /// Waits for the child `pid` to exit; resolves with its wait status.
    pub fn waitpid(&mut self, pid: i32) -> Future<i32> {
        let this = self as *mut Reactor;
        crate::engine::core::coroutine::make_coroutine(async move {
            // SAFETY: the reactor outlives every future it runs.
            let me = unsafe { &mut *this };
            let pidfd: SyscallResult<i32> = me
                .thread_pool()
                .submit::<SyscallResult<i32>, _>(move || {
                    // SAFETY: plain pidfd_open system call.
                    wrap_syscall::<i32>(unsafe { syscall(NR_PIDFD_OPEN, pid, O_NONBLOCK) } as i32)
                })
                .await?;
            // pidfd_open() was introduced in linux 5.3, so the pidfd.error could be ENOSYS on
            // older kernels. But it could be other error like EMFILE or ENFILE. anyway, we
            // should always waitpid().
            let mut pfd: Option<PollableFd> = None;
            if pidfd.result != -1 {
                pfd = Some(PollableFd::from_file_desc(FileDesc::from_fd(pidfd.result)));
                pfd.as_mut().unwrap().readable().await?;
            }

            let do_waitpid = |pid: i32| {
                let tp = me.thread_pool() as *const ThreadPool;
                async move {
                    let ws = Arc::new(AtomicI32::new(0));
                    let ws2 = ws.clone();
                    let wait = move || {
                        let mut wstatus = 0;
                        // SAFETY: `wstatus` is a valid int.
                        let r = wrap_syscall::<i32>(unsafe {
                            libc_waitpid(pid, &mut wstatus, WNOHANG)
                        });
                        ws2.store(wstatus, Ordering::Relaxed);
                        r
                    };
                    // SAFETY: the thread pool lives as long as the reactor.
                    let ret: SyscallResult<i32> =
                        unsafe { (*tp).submit::<SyscallResult<i32>, _>(wait) }.await?;
                    if ret.result == 0 {
                        // Result not ready yet (with WNOHANG)
                        Ok::<Option<i32>, ExceptionPtr>(None)
                    } else if ret.result > 0 {
                        // Success.  Return the waited pid status
                        Ok(Some(ws.load(Ordering::Relaxed)))
                    } else {
                        // Error.  Maybe throw exception, or return -1 status.
                        ret.throw_if_error()?;
                        Ok(Some(-1))
                    }
                }
            };

            let mut wait_timeout = Duration::from_millis(0);
            let ret_opt;
            loop {
                if let Some(v) = do_waitpid(pid).await? {
                    ret_opt = v;
                    break;
                }
                wait_timeout = next_waitpid_timeout(wait_timeout);
                crate::engine::core::sleep::sleep_steady(wait_timeout).await?;
            }
            drop(pfd);
            Ok(ret_opt)
        })
    }

    /// Sends signal `sig` to process `pid`.
    ///
    /// # Errors
    ///
    /// Returns the system error of `kill`.
    pub fn kill(&mut self, pid: i32, sig: i32) -> Result<(), ExceptionPtr> {
        // SAFETY: plain kill system call.
        let ret = wrap_syscall::<i32>(unsafe { libc_kill(pid, sig) });
        ret.throw_if_error()
    }

    /// Looks up the group `name` (on the syscall thread); resolves with its details, or
    /// `None` if it does not exist.
    pub fn getgrnam(&mut self, name: &str) -> Future<Option<GroupDetails>> {
        let this = self as *mut Reactor;
        let name = Sstring::from(name);
        crate::engine::core::coroutine::make_coroutine(async move {
            // SAFETY: the reactor outlives every future it runs.
            let me = unsafe { &mut *this };
            let sr: SyscallResultExtra<Option<GroupDetails>> = me
                .thread_pool()
                .submit::<SyscallResultExtra<Option<GroupDetails>>, _>(move || {
                    // SAFETY: `Group` is plain data.
                    let mut grp: Group = unsafe { std::mem::zeroed() };
                    let mut result: *mut Group = std::ptr::null_mut();
                    let mut buf = [0 as std::ffi::c_char; 1024];
                    crate::engine::core::reactor_backend::set_errno(0);
                    let c = cstr(name.as_str());
                    // SAFETY: all buffers are valid for the call.
                    let ret = unsafe {
                        getgrnam_r(c.as_ptr(), &mut grp, buf.as_mut_ptr(), buf.len(), &mut result)
                    };
                    if result.is_null() {
                        return wrap_syscall_extra(ret as i64, None);
                    }

                    let mut gd = GroupDetails::default();
                    // SAFETY: getgrnam_r filled `grp` with NUL-terminated strings in `buf`.
                    unsafe {
                        gd.group_name = Sstring::from(
                            std::ffi::CStr::from_ptr(grp.gr_name).to_string_lossy().into_owned(),
                        );
                        gd.group_passwd = Sstring::from(
                            std::ffi::CStr::from_ptr(grp.gr_passwd).to_string_lossy().into_owned(),
                        );
                        gd.group_id = grp.gr_gid;
                        let mut members = grp.gr_mem;
                        while !(*members).is_null() {
                            gd.group_members.push(Sstring::from(
                                std::ffi::CStr::from_ptr(*members).to_string_lossy().into_owned(),
                            ));
                            members = members.add(1);
                        }
                    }
                    wrap_syscall_extra(ret as i64, Some(gd))
                })
                .await?;

            // A group that does not exist may give 0 or ENOENT, depending on the configuration
            // of the system (with sssd, for one): both mean "not found".
            const ENOENT: i32 = 2;
            if sr.result != 0 && sr.result != ENOENT {
                return Err(ExceptionPtr::from(SystemError::from_errno(sr.ec())));
            }

            Ok(sr.extra)
        })
    }

    /// Changes the owner and group of `filepath` (on the syscall thread).
    pub fn chown(&mut self, filepath: &str, owner: u32, group: u32) -> Future<()> {
        let p = Sstring::from(filepath);
        self.thread_pool()
            .submit::<SyscallResult<i32>, _>(move || {
                let c = cstr(p.as_str());
                // SAFETY: NUL-terminated path.
                let ret = unsafe { libc_chown(c.as_ptr(), owner, group) };
                wrap_syscall::<i32>(ret)
            })
            .then(|sr: SyscallResult<i32>| {
                fut_try!(sr.throw_if_error());
                make_ready_future(())
            })
    }

    /// The status of `pathname` (on the syscall thread); symbolic links are followed if
    /// `follow`.
    pub fn file_stat(&mut self, pathname: &str, follow: FollowSymlink) -> Future<StatData> {
        // Allocating memory for a sstring can throw, hence the futurize_invoke
        let p1 = Sstring::from(pathname);
        let p2 = p1.clone();
        self.thread_pool()
            .submit::<SyscallResultExtra<Stat>, _>(move || {
                // SAFETY: `Stat` is plain data.
                let mut st: Stat = unsafe { std::mem::zeroed() };
                let c = cstr(p1.as_str());
                // SAFETY: NUL-terminated path and a valid stat buffer.
                let ret = unsafe {
                    if follow.as_bool() {
                        stat(c.as_ptr(), &mut st)
                    } else {
                        lstat(c.as_ptr(), &mut st)
                    }
                };
                wrap_syscall_extra(ret as i64, st)
            })
            .then(move |sr: SyscallResultExtra<Stat>| {
                fut_try!(sr.throw_fs_exception_if_error("stat failed", p2.as_str()));
                let st = &sr.extra;
                // `st_nlink` is u64 on x86_64 and u32 on aarch64.
                #[allow(clippy::unnecessary_cast)]
                let number_of_links = st.st_nlink as u64;
                let sd = StatData {
                    device_id: st.st_dev,
                    inode_number: st.st_ino,
                    mode: st.st_mode as u64,
                    type_: stat_to_entry_type(st.st_mode),
                    number_of_links,
                    uid: st.st_uid as u64,
                    gid: st.st_gid as u64,
                    rdev: st.st_rdev,
                    size: st.st_size as u64,
                    block_size: st.st_blksize as u64,
                    allocated_size: st.st_blocks as u64 * 512,
                    time_accessed: timespec_to_time_point(&st.st_atim),
                    time_modified: timespec_to_time_point(&st.st_mtim),
                    time_changed: timespec_to_time_point(&st.st_ctim),
                };
                make_ready_future(sd)
            })
    }

    /// The size of `pathname`.
    pub fn file_size(&mut self, pathname: &str) -> Future<u64> {
        self.file_stat(pathname, FollowSymlink::YES).then(|sd: StatData| make_ready_future(sd.size))
    }

    /// Whether `pathname` is accessible with `flags` (on the syscall thread).
    pub fn file_accessible(&mut self, pathname: &str, flags: AccessFlags) -> Future<bool> {
        const EACCES: i32 = 13;
        // Allocating memory for a sstring can throw, hence the futurize_invoke: a failed
        // allocation of the copies fails the future instead.
        let copies = std::panic::catch_unwind(|| {
            let p1 = Sstring::from(pathname);
            let p2 = p1.clone();
            (p1, p2)
        });
        let (p1, p2) = match copies {
            Ok(copies) => copies,
            Err(p) => return make_exception_future(ExceptionPtr::from_panic(p)),
        };
        self.thread_pool()
            .submit::<SyscallResult<i32>, _>(move || {
                let aflags = flags.bits();
                let c = cstr(p1.as_str());
                // SAFETY: NUL-terminated path.
                let ret = unsafe { libc_access(c.as_ptr(), aflags) };
                wrap_syscall::<i32>(ret)
            })
            .then(move |sr: SyscallResult<i32>| {
                if sr.result < 0 {
                    if (sr.error == ENOENT && flags == AccessFlags::EXISTS)
                        || (sr.error == EACCES && flags != AccessFlags::EXISTS)
                    {
                        return make_ready_future(false);
                    }
                    return make_exception_future(
                        sr.throw_fs_exception("access failed", std::path::Path::new(p2.as_str())),
                    );
                }

                make_ready_future(true)
            })
    }

    /// The type of the file system holding `pathname`.
    pub fn file_system_at(&mut self, pathname: &str) -> Future<FsType> {
        use crate::engine::util::internal::magic::internal::FsMagic as fs_magic;
        // Allocating memory for a sstring can throw, hence the futurize_invoke
        let p1 = Sstring::from(pathname);
        let p2 = p1.clone();
        self.thread_pool()
            .submit::<SyscallResultExtra<Statfs>, _>(move || {
                // SAFETY: `Statfs` is plain data.
                let mut st: Statfs = unsafe { std::mem::zeroed() };
                let c = cstr(p1.as_str());
                // SAFETY: NUL-terminated path and a valid buffer.
                let ret = unsafe { libc_statfs(c.as_ptr(), &mut st) };
                wrap_syscall_extra(ret as i64, st)
            })
            .then(move |sr: SyscallResultExtra<Statfs>| {
                fut_try!(sr.throw_fs_exception_if_error("statfs failed", p2.as_str()));

                let ret = match sr.extra.f_type {
                    t if t == fs_magic::XFS as i64 => FsType::Xfs,
                    t if t == fs_magic::EXT2 as i64 => FsType::Ext2,
                    t if t == fs_magic::EXT3 as i64 => FsType::Ext3,
                    t if t == fs_magic::EXT4 as i64 => FsType::Ext4,
                    t if t == fs_magic::BTRFS as i64 => FsType::Btrfs,
                    t if t == fs_magic::HFS as i64 => FsType::Hfs,
                    t if t == fs_magic::TMPFS as i64 => FsType::Tmpfs,
                    _ => FsType::Other,
                };
                make_ready_future(ret)
            })
    }

    /// `fstatfs` of `fd` (on the syscall thread).
    pub(crate) fn fstatfs(&mut self, fd: i32) -> Future<Statfs> {
        self.thread_pool()
            .submit::<SyscallResultExtra<Statfs>, _>(move || {
                // SAFETY: `Statfs` is plain data.
                let mut st: Statfs = unsafe { std::mem::zeroed() };
                // SAFETY: valid buffer.
                let ret = unsafe { fstatfs(fd, &mut st) };
                wrap_syscall_extra(ret as i64, st)
            })
            .then(|sr: SyscallResultExtra<Statfs>| {
                fut_try!(sr.throw_if_error());
                make_ready_future(sr.extra)
            })
    }

    /// The capacity, free and available space of the file system holding `pathname`, from
    /// `statvfs`.
    pub fn file_system_space(
        &mut self,
        pathname: &str,
    ) -> Future<crate::engine::core::file::SpaceInfo> {
        let p1 = Sstring::from(pathname);
        let p2 = p1.clone();
        self.thread_pool()
            .submit::<SyscallResultExtra<crate::engine::core::file::SpaceInfo>, _>(move || {
                // SAFETY: `Statvfs` is plain data.
                let mut st: Statvfs = unsafe { std::mem::zeroed() };
                let c = cstr(p1.as_str());
                // SAFETY: NUL-terminated path and a valid buffer.
                let r = unsafe { libc_statvfs(c.as_ptr(), &mut st) };
                let ec = if r == -1 { errno() } else { 0 };
                let si = if r == -1 {
                    crate::engine::core::file::SpaceInfo {
                        capacity: u64::MAX,
                        free: u64::MAX,
                        available: u64::MAX,
                    }
                } else {
                    crate::engine::core::file::SpaceInfo {
                        capacity: st.f_blocks as u64 * st.f_frsize as u64,
                        free: st.f_bfree as u64 * st.f_frsize as u64,
                        available: st.f_bavail as u64 * st.f_frsize as u64,
                    }
                };
                // wrap_syscall(ec.value(), si)
                let mut res = wrap_syscall_extra(ec as i64, si);
                res.error = ec;
                res
            })
            .then(move |sr: SyscallResultExtra<crate::engine::core::file::SpaceInfo>| {
                fut_try!(
                    sr.throw_fs_exception_if_error("filesystem space query failed", p2.as_str())
                );
                make_ready_future(sr.extra)
            })
    }

    /// `statvfs` of `pathname` (on the syscall thread).
    pub fn statvfs(&mut self, pathname: &str) -> Future<Statvfs> {
        // Allocating memory for a sstring can throw, hence the futurize_invoke
        let p1 = Sstring::from(pathname);
        let p2 = p1.clone();
        self.thread_pool()
            .submit::<SyscallResultExtra<Statvfs>, _>(move || {
                // SAFETY: `Statvfs` is plain data.
                let mut st: Statvfs = unsafe { std::mem::zeroed() };
                let c = cstr(p1.as_str());
                // SAFETY: NUL-terminated path and a valid buffer.
                let ret = unsafe { libc_statvfs(c.as_ptr(), &mut st) };
                wrap_syscall_extra(ret as i64, st)
            })
            .then(move |sr: SyscallResultExtra<Statvfs>| {
                fut_try!(sr.throw_fs_exception_if_error("statvfs failed", p2.as_str()));
                make_ready_future(sr.extra)
            })
    }

    /// Opens the directory `name`.
    pub fn open_directory(&mut self, name: &str) -> Future<File> {
        // Allocating memory for a sstring can throw, hence the futurize_invoke
        let oflags = O_DIRECTORY | O_CLOEXEC | O_RDONLY;
        let n1 = Sstring::from(name);
        let n2 = n1.clone();
        self.thread_pool()
            .submit::<SyscallResultExtra<Stat>, _>(move || {
                // SAFETY: `Stat` is plain data.
                let mut st: Stat = unsafe { std::mem::zeroed() };
                let c = cstr(n1.as_str());
                // SAFETY: NUL-terminated path; valid stat buffer.
                let mut fd = unsafe { open(c.as_ptr(), oflags) };
                if fd != -1 {
                    // SAFETY: as above.
                    let r = unsafe { fstat(fd, &mut st) };
                    if r == -1 {
                        // SAFETY: closing the descriptor opened above.
                        unsafe { close(fd) };
                        fd = r;
                    }
                }
                wrap_syscall_extra(fd as i64, st)
            })
            .then(move |sr: SyscallResultExtra<Stat>| {
                fut_try!(sr.throw_fs_exception_if_error("open failed", n2.as_str()));
                crate::engine::core::file::make_file_impl(
                    sr.result,
                    FileOpenOptions::default(),
                    oflags,
                    sr.extra,
                )
            })
            .then(|file_impl: SharedPtr<dyn FileImpl>| {
                make_ready_future(File::from_impl(file_impl))
            })
    }

    /// Creates the directory `name` with `permissions`.
    pub fn make_directory(&mut self, name: &str, permissions: FilePermissions) -> Future<()> {
        // Allocating memory for a sstring can throw, hence the futurize_invoke
        let n1 = Sstring::from(name);
        let n2 = n1.clone();
        self.thread_pool()
            .submit::<SyscallResult<i32>, _>(move || {
                let mode = permissions.bits() as u32;
                let c = cstr(n1.as_str());
                // SAFETY: NUL-terminated path.
                wrap_syscall::<i32>(unsafe { mkdir(c.as_ptr(), mode) })
            })
            .then(move |sr: SyscallResult<i32>| {
                fut_try!(sr.throw_fs_exception_if_error("mkdir failed", n2.as_str()));
                make_ready_future(())
            })
    }

    /// Creates the directory `name` with `permissions` if it does not exist.
    pub fn touch_directory(&mut self, name: &str, permissions: FilePermissions) -> Future<()> {
        // Allocating memory for a sstring can throw, hence the futurize_invoke
        let n1 = Sstring::from(name);
        let n2 = n1.clone();
        self.thread_pool()
            .submit::<SyscallResult<i32>, _>(move || {
                let mode = permissions.bits() as u32;
                let c = cstr(n1.as_str());
                // SAFETY: NUL-terminated path.
                wrap_syscall::<i32>(unsafe { mkdir(c.as_ptr(), mode) })
            })
            .then(move |sr: SyscallResult<i32>| {
                if sr.result == -1 && sr.error != EEXIST {
                    return make_exception_future(
                        sr.throw_fs_exception("mkdir failed", std::path::Path::new(n2.as_str())),
                    );
                }
                make_ready_future(())
            })
    }

    /// `fdatasync` of `fd`, with linux-aio when supported, otherwise on the syscall thread;
    /// a no-op when `fsync` is bypassed.
    pub(crate) fn fdatasync(&mut self, fd: i32) -> Future<()> {
        self._fsyncs += 1;
        if self._cfg.bypass_fsync {
            return make_ready_future(());
        }
        if self._cfg.have_aio_fsync {
            // Does not go through the I/O queue, but has to be deleted
            let desc = Box::into_raw(Box::new(FsyncIoDesc {
                base: IoCompletion::new::<FsyncIoDesc>(),
                _pr: Promise::new(),
            }));
            // SAFETY: `desc` was just allocated; it deletes itself when it completes.
            let fut = unsafe { (*desc)._pr.get_future() };
            let req = IoRequest::make_fdatasync(fd);
            self._io_sink.submit(desc.cast::<IoCompletion>(), req);
            return fut;
        }
        self.thread_pool()
            .submit::<SyscallResult<i32>, _>(move || {
                // SAFETY: plain fdatasync.
                wrap_syscall::<i32>(unsafe { fdatasync(fd) })
            })
            .then(|sr: SyscallResult<i32>| {
                fut_try!(sr.throw_if_error());
                make_ready_future(())
            })
    }

    // Note: terminate if arm_highres_timer throws
    // `when` should always be valid
    /// Arms the high resolution timer for `when`.
    pub fn enable_timer(
        &mut self,
        when: <SteadyClockType as crate::engine::core::timer::Clock>::TimePoint,
    ) {
        let its = crate::engine::core::posix::Itimerspec {
            it_interval: Default::default(),
            it_value: to_timespec(when),
        };
        // SAFETY: the backend is set for the reactor's life; it is reached through its table
        // for one call that runs no completion.
        unsafe {
            let (b, e) = Self::backend_entries(self);
            (e.arm_highres_timer)(b, &its)
        }
    }

    /// Registers `func` to run when the reactor exits.
    ///
    /// Aborts the process if the reactor is already stopping.
    pub(crate) fn do_at_exit(&mut self, func: NoncopyableFunction<fn() -> Future<()>>) {
        engine_assert!(!self._stopping);
        self._exit_funcs.push(func);
    }

    /// Deprecated. Use following sequence instead:
    ///
    /// ```text
    ///    return teseladb::app_template::run([] {
    ///         return teseladb::async([] {
    ///              // Since the function runs in a thread, it can wait for futures using
    ///              // future::get().
    ///              auto deferred_task = defer(/* the function you want to run at exit */);
    ///         });
    ///    });
    /// ```
    #[deprecated(
        note = "Use teseladb::app_template::run(), teseladb::async(), and teseladb::defer() for orderly shutdown"
    )]
    pub fn at_exit(&mut self, func: NoncopyableFunction<fn() -> Future<()>>) {
        self.do_at_exit(func);
    }

    /// Wakes the waiters of a stop request and runs the exit functions.
    ///
    /// # Safety
    /// `this` is the live reactor of this thread, with no reference to it live: waking the
    /// waiters of `_stop_requested` and the exit functions re-enter it.
    unsafe fn run_exit_tasks(this: *mut Self) -> Future<()> {
        // SAFETY: guaranteed by the caller; each access is short. The condition variable
        // has interior state and is reached by `&`.
        let n = unsafe {
            (*std::ptr::addr_of!((*this)._stop_requested)).broadcast();
            (*this).stop_aio_eventfd_loop();
            (*this)._exit_funcs.len()
        };
        // do_for_each over _exit_funcs in reverse order
        crate::engine::core::loop_::do_for_each((0..n).rev(), move |i: usize| {
            // SAFETY: the exit functions live in the reactor, which outlives this loop, and are
            // not added to once it is stopping (do_at_exit asserts it).
            unsafe { (&mut *std::ptr::addr_of_mut!((*this)._exit_funcs))[i].call() }
        })
    }

    /// Stops the application from shard 0: runs the exit tasks locally, then stops the other
    /// shards in the background and waits for them before marking this reactor stopped.
    ///
    /// # Safety
    /// As [`Reactor::run_exit_tasks`]; `this` is the reactor of shard 0.
    unsafe fn stop(this: *mut Self) {
        // SAFETY: guaranteed by the caller; each access is short.
        let already_stopping = unsafe {
            engine_assert!((*this)._id == 0);
            (*this)._smp.cleanup_cpu();
            std::mem::replace(&mut (*this)._stopping, true)
        };
        if !already_stopping {
            // Run exit tasks locally and then stop all other engines
            // in the background and wait on semaphore for all to complete.
            // Finally, set _stopped on cpu 0.
            // SAFETY (closures): the reactor of shard 0 outlives its stop sequence, and each
            // closure runs as a task with no reference to a reactor live.
            let _ = Self::drain().then(move |()| {
                // SAFETY: `this` is the reactor of shard 0, which outlives its stop sequence; this
                // continuation runs as a task, with no reference to the reactor live.
                unsafe { Self::run_exit_tasks(this) }.then(move |()| {
                    crate::engine::core::do_with::do_with(
                        Semaphore::new(0),
                        move |sem: &mut Semaphore| {
                            // The semaphore lives in the closure until `sem.wait()` resolves, which is
                            // after the signal below.
                            let semp: *mut Semaphore = sem;
                            // Stop other cpus asynchronously, signal when done.
                            let _ = Smp::invoke_on_others_of(0, || {
                                let r = engine();
                                // SAFETY: on this shard's reactor thread `r` is its live reactor; the
                                // task holds no reference to it, so the short accesses and
                                // `run_exit_tasks` (whose contract this is) alias nothing.
                                unsafe {
                                    (*r)._smp.cleanup_cpu();
                                    (*r)._stopping = true;
                                    Self::run_exit_tasks(r)
                                }
                                .then(|()| {
                                    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
                                    unsafe { (*engine())._stopped = true };
                                    make_ready_future(())
                                })
                            })
                            .then(move |()| {
                                // SAFETY: see `semp`: the semaphore is alive until it is signalled.
                                unsafe { (*semp).signal(1) };
                                make_ready_future(())
                            });
                            // SAFETY: as above.
                            unsafe { (*semp).wait(1) }.then(move |()| {
                                // SAFETY: `this` is the reactor of shard 0, which outlives its stop sequence; a short
                                // field write from a task on its thread, with no reference to it live.
                                unsafe { (*this)._stopped = true };
                                make_ready_future(())
                            })
                        },
                    )
                })
            });
        }
    }

    /// Stops the application with exit code `ret`: shard 0's reactor stops, which stops the
    /// others.
    pub fn exit(ret: i32) {
        // Run stop() asynchronously on cpu 0.
        let _ = Smp::submit_to(0, move || {
            let r = engine();
            // SAFETY: runs on shard 0 with no reference to its reactor live (the caller holds
            // none), so the reactor is reached through its pointer.
            unsafe {
                (*r)._return = ret;
                Self::stop(r);
            }
        });
    }

    fn pending_task_count(&self) -> u64 {
        let mut ret = 0u64;
        for tq in self._task_queues.iter().flatten() {
            // SAFETY: the queue is live while its node is; no `&mut` to it is live here.
            ret += unsafe { (*tq.as_ptr())._q.size() } as u64;
        }
        ret
    }

    /// Total tasks processed.
    pub(crate) fn tasks_processed(&self) -> u64 {
        self._global_tasks_processed
    }

    /// Registers the reactor's metrics.
    ///
    /// # Errors
    ///
    /// Returns the error of registering a metric group.
    fn register_metrics(&mut self) -> Result<(), ExceptionPtr> {
        use crate::engine::core::metrics as sm;
        let this = self as *const Reactor;
        // SAFETY (every closure below): the metric groups are owned by the reactor and are
        // unregistered before it is destroyed, so `this` is valid whenever a metric is read.
        macro_rules! r {
            () => {
                // SAFETY: as above, `this` is valid whenever a metric is read; the metrics are
                // read on this reactor's thread while no `&mut` to the reactor is live.
                unsafe { &*this }
            };
        }

        self._metric_groups.add_group("reactor", vec![
            sm::make_gauge("tasks_pending", move || r!().pending_task_count() as f64, sm::description("Number of pending tasks in the queue"), vec![]),
            // total_operations value:DERIVE:0:U
            sm::make_counter("tasks_processed", move || r!().tasks_processed(), sm::description("Total tasks processed"), vec![]),
            sm::make_counter("polls", move || r!()._polls, sm::description("Number of times pollers were executed"), vec![]),
            sm::make_gauge("timers_pending", move || r!()._timers.size() as f64, sm::description("Number of tasks in the timer-pending queue"), vec![]),
            sm::make_gauge("utilization", move || (1.0 - r!()._load) * 100.0, sm::description("CPU utilization"), vec![]),
            // SAFETY: `this` is the reactor, valid whenever a metric is read (see above); the read is
            // on its thread while no reference to it is live.
            sm::make_counter("cpu_busy_ms", move || unsafe { (*(this as *mut Reactor)).total_busy_time() }.as_millis(),
                sm::description("Total cpu busy time in milliseconds"), vec![]),
            sm::make_counter("sleep_time_ms_total", move || r!()._total_sleep.as_millis(),
                sm::description("Total reactor sleep time (wall clock)"), vec![]),
            sm::make_counter("awake_time_ms_total", move || r!().total_awake_time().as_millis(),
                sm::description("Total reactor awake time (wall_clock)"), vec![]),
            sm::make_counter("cpu_used_time_ms", move || r!().total_cpu_time().as_millis(),
                sm::description("Total reactor thread CPU time (from CLOCK_THREAD_CPUTIME)"), vec![]),
            // SAFETY: `this` is the reactor, valid whenever a metric is read (see above); the read is
            // on its thread while no reference to it is live.
            sm::make_counter("cpu_steal_time_ms", move || unsafe { (*(this as *mut Reactor)).total_steal_time() }.as_millis(),
                sm::description("Total steal time, the time in which something else was running while the reactor was runnable (not sleeping).Because this is in userspace, some time that could be legitimally thought as steal time is not accounted as such. For example, if we are sleeping and can wake up but the kernel hasn't woken us up yet."), vec![]),
            // total_operations value:DERIVE:0:U
            sm::make_counter("aio_reads", move || r!()._io_stats.aio_reads, sm::description("Total aio-reads operations"), vec![]),

            sm::make_total_bytes("aio_bytes_read", move || r!()._io_stats.aio_read_bytes, sm::description("Total aio-reads bytes"), vec![]),
            // total_operations value:DERIVE:0:U
            sm::make_counter("aio_writes", move || r!()._io_stats.aio_writes, sm::description("Total aio-writes operations"), vec![]),
            sm::make_total_bytes("aio_bytes_write", move || r!()._io_stats.aio_write_bytes, sm::description("Total aio-writes bytes"), vec![]),
            sm::make_counter("aio_outsizes", move || r!()._io_stats.aio_outsizes, sm::description("Total number of aio operations that exceed IO limit"), vec![]),
            sm::make_counter("aio_errors", move || r!()._io_stats.aio_errors, sm::description("Total aio errors"), vec![]),
            {
                let mut stalls = sm::make_histogram_fn("stalls", sm::description("A histogram of reactor stall durations"),
                    move || r!()._stalls_histogram.to_metrics_histogram());
                stalls.aggregate(&[sm::shard_label()]);
                stalls.set_skip_when_empty(true)
            },
            // total_operations value:DERIVE:0:U
            sm::make_counter("fsyncs", move || r!()._fsyncs, sm::description("Total number of fsync operations"), vec![]),
            // total_operations value:DERIVE:0:U
            sm::make_counter("io_threaded_fallbacks", move || r!().thread_pool().operation_count(),
                sm::description("Total number of io-threaded-fallbacks operations"), vec![]),
        ])?;

        self._metric_groups.add_group(
            "memory",
            vec![
                sm::make_counter(
                    "malloc_operations",
                    || memory::stats().mallocs(),
                    sm::description("Total number of malloc operations"),
                    vec![],
                ),
                sm::make_counter(
                    "free_operations",
                    || memory::stats().frees(),
                    sm::description("Total number of free operations"),
                    vec![],
                ),
                sm::make_counter(
                    "cross_cpu_free_operations",
                    || memory::stats().cross_cpu_frees(),
                    sm::description("Total number of cross cpu free"),
                    vec![],
                ),
                sm::make_gauge(
                    "malloc_live_objects",
                    || memory::stats().live_objects() as f64,
                    sm::description("Number of live objects"),
                    vec![],
                ),
                sm::make_current_bytes(
                    "free_memory",
                    || memory::stats().free_memory(),
                    sm::description("Free memory size in bytes"),
                    vec![],
                ),
                sm::make_current_bytes(
                    "total_memory",
                    || memory::stats().total_memory(),
                    sm::description("Total memory size in bytes"),
                    vec![],
                ),
                sm::make_current_bytes(
                    "allocated_memory",
                    || memory::stats().allocated_memory(),
                    sm::description("Allocated memory size in bytes"),
                    vec![],
                ),
                sm::make_counter(
                    "reclaims_operations",
                    || memory::stats().reclaims(),
                    sm::description("Total reclaims operations"),
                    vec![],
                ),
                sm::make_counter(
                    "malloc_failed",
                    || memory::stats().failed_allocations(),
                    sm::description("Total count of failed memory allocations"),
                    vec![],
                ),
                sm::make_counter(
                    "oversized_allocs",
                    || memory::stats().large_allocations(),
                    sm::description("Total count of oversized memory allocations"),
                    vec![],
                ),
            ],
        )?;

        self._metric_groups.add_group("reactor", vec![
            sm::make_counter("logging_failures", || crate::engine::util::log::LOGGING_FAILURES.with(|f| f.get()), sm::description("Total number of logging failures"), vec![]),
            // total_operations value:DERIVE:0:U
            sm::make_counter("exceptions", move || r!()._exceptions_thrown, sm::description("Total number of exceptions thrown"), vec![]),
            sm::make_counter("abandoned_failed_futures", move || r!()._abandoned_failed_futures,
                sm::description("Total number of abandoned failed futures, futures destroyed while still containing an exception"), vec![]),
        ])?;

        self._metric_groups.add_group("reactor", vec![
            sm::make_counter("fstream_reads", move || r!()._io_stats.fstream_reads,
                sm::description("Counts reads from disk file streams.  A high rate indicates high disk activity. Contrast with other fstream_read* counters to locate bottlenecks."), vec![]),
            sm::make_counter("fstream_read_bytes", move || r!()._io_stats.fstream_read_bytes,
                sm::description("Counts bytes read from disk file streams.  A high rate indicates high disk activity. Divide by fstream_reads to determine average read size."), vec![]),
            sm::make_counter("fstream_reads_blocked", move || r!()._io_stats.fstream_reads_blocked,
                sm::description("Counts the number of times a disk read could not be satisfied from read-ahead buffers, and had to block. Indicates short streams, or incorrect read ahead configuration."), vec![]),
            sm::make_counter("fstream_read_bytes_blocked", move || r!()._io_stats.fstream_read_bytes_blocked,
                sm::description("Counts the number of bytes read from disk that could not be satisfied from read-ahead buffers, and had to block. Indicates short streams, or incorrect read ahead configuration."), vec![]),
            sm::make_counter("fstream_reads_aheads_discarded", move || r!()._io_stats.fstream_read_aheads_discarded,
                sm::description("Counts the number of times a buffer that was read ahead of time and was discarded because it was not needed, wasting disk bandwidth. Indicates over-eager read ahead configuration."), vec![]),
            sm::make_counter("fstream_reads_ahead_bytes_discarded", move || r!()._io_stats.fstream_read_ahead_discarded_bytes,
                sm::description("Counts the number of buffered bytes that were read ahead of time and were discarded because they were not needed, wasting disk bandwidth. Indicates over-eager read ahead configuration."), vec![]),
        ])?;
        Ok(())
    }

    /// Runs the tasks of `tq` until it is empty or preemption is needed.
    ///
    /// The tasks re-enter the reactor (they add tasks to this very queue), so the reactor
    /// and the queue are reached through raw pointers, and no reference to either is live
    /// while a task runs.
    ///
    /// # Safety
    /// `this` is the live reactor of this thread and `tq` one of its task queues, with no
    /// reference to either live.
    unsafe fn run_tasks(this: *mut Self, tq: *mut TaskQueue) {
        // SAFETY: guaranteed by the caller; every reference below ends before the task runs
        // or is formed after it returned.
        unsafe {
            // Make sure new tasks will inherit our scheduling group
            set_current_scheduling_group(SchedulingGroup::from_id((*tq)._id as u32));
            let q = std::ptr::addr_of_mut!((*tq)._q);
            while !(*q).empty() {
                let tsk = *(*q).front();
                (*q).pop_front();
                // STAP_PROBE(engine, reactor_run_tasks_single_start);
                // Queued tasks are live until they run and dispose of themselves.
                task_histogram_add_task(tsk);
                (*this)._current_task = Some(tsk);
                tsk.run_and_dispose();
                (*this)._current_task = None;
                // STAP_PROBE(engine, reactor_run_tasks_single_end);
                (*tq)._tasks_processed += 1;
                (*this)._global_tasks_processed += 1;
                // check at end of loop, to allow at least one task to run
                if crate::engine::core::preempt::internal::scheduler_need_preempt() {
                    if (*q).size() <= (*this)._cfg.max_task_backlog as usize {
                        break;
                    } else {
                        // While need_preempt() is set, task execution is inefficient due to
                        // need_preempt() checks breaking out of loops and .then() calls. See
                        // #302.
                        Self::reset_preemption_monitor(this);
                        LowresClock::update();

                        thread_local! {
                            static RATE_LIMIT: RefCell<crate::engine::util::log::RateLimit> =
                                RefCell::new(crate::engine::util::log::RateLimit::new(Duration::from_secs(10)));
                        }
                        RATE_LIMIT.with(|rate_limit| {
                            // The queue is not running a task while it is dumped.
                            let mut writer = crate::engine::util::log::LambdaLogWriter::new(
                                |it: InserterIterator| do_dump_task_queue(it, &*tq),
                            );
                            engine_logger().log_writer(
                                crate::engine::util::log::LogLevel::Warn,
                                &mut rate_limit.borrow_mut(),
                                &mut writer,
                                crate::engine::util::log::FormatInfo::empty(),
                            );
                        });
                    }
                }
            }
        }
    }
}

#[cfg(engine_shuffle_task_queue)]
fn shuffle(t: &mut TaskPtr, q: &mut CircularBuffer<TaskPtr>) {
    thread_local! {
        static GEN: Cell<u64> = Cell::new({
            let s = std::collections::hash_map::RandomState::new();
            use std::hash::{BuildHasher, Hasher};
            let mut h = s.build_hasher();
            h.write_u64(0);
            h.finish() | 1
        });
    }
    // xorshift64*, uniform over [0, q.size() - 1]
    let r = GEN.with(|g| {
        let mut x = g.get();
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        g.set(x);
        x.wrapping_mul(0x2545F4914F6CDD1D)
    });
    let idx = (r % q.size() as u64) as usize;
    std::mem::swap(&mut q[idx], t);
}

#[cfg(not(engine_shuffle_task_queue))]
#[inline(always)]
fn shuffle_back(_q: &mut CircularBuffer<TaskPtr>) {}

#[cfg(not(engine_shuffle_task_queue))]
#[inline(always)]
fn shuffle_front(_q: &mut CircularBuffer<TaskPtr>) {}

#[cfg(engine_shuffle_task_queue)]
fn shuffle_back(q: &mut CircularBuffer<TaskPtr>) {
    let n = q.size() - 1;
    let mut t = q[n];
    shuffle(&mut t, q);
    q[n] = t;
}

#[cfg(engine_shuffle_task_queue)]
fn shuffle_front(q: &mut CircularBuffer<TaskPtr>) {
    let mut t = q[0];
    shuffle(&mut t, q);
    q[0] = t;
}

impl Reactor {
    /// Makes the main loop poll as soon as the current task yields.
    pub fn force_poll(&mut self) {
        self.request_preemption();
    }

    /// Flushes the TCP streams waiting for a batched flush; returns `true` if any.
    ///
    /// # Safety
    /// `this` is the live reactor of this thread, with no reference to it live: a flush
    /// re-enters the reactor.
    unsafe fn flush_tcp_batches(this: *mut Self) -> bool {
        // SAFETY: guaranteed by the caller; each access to the list ends before the stream
        // is flushed.
        unsafe {
            let list = std::ptr::addr_of_mut!((*this)._flush_batching);
            let work = !(*list).empty();
            while !(*list).empty() {
                let os = (*list).front();
                (*list).pop_front();
                // Streams in the flush list are live until they are flushed.
                (*os).poll_flush();
            }
            work
        }
    }

    /// Runs the expired low-resolution timers; returns `true` if any.
    ///
    /// # Safety
    /// As [`Reactor::flush_tcp_batches`]: the timer callbacks re-enter the reactor.
    unsafe fn do_expire_lowres_timers(this: *mut Self) -> bool {
        let now = LowresClock::now();
        // SAFETY: guaranteed by the caller.
        if now < unsafe { (*this)._lowres_next_timeout } {
            return false;
        }
        // SAFETY: the timer set and the expired list are fields of the live reactor, which
        // is constructed in place and does not move; no reference to the reactor is live
        // while `complete` runs the callbacks, and the closure runs after the last one.
        unsafe {
            TimerSet::complete(
                std::ptr::addr_of_mut!((*this)._lowres_timers),
                std::ptr::addr_of_mut!((*this)._expired_lowres_timers),
                || {
                    (*this)._lowres_next_timeout = if !(*this)._lowres_timers.empty() {
                        (*this)._lowres_timers.get_next_timeout()
                    } else {
                        <LowresClock as crate::engine::core::timer::Clock>::max_time_point()
                    };
                },
            )
        };
        true
    }

    /// Runs the expired manual-clock timers.
    ///
    /// # Safety
    /// As [`Reactor::do_expire_lowres_timers`].
    pub(crate) unsafe fn expire_manual_timers(this: *mut Self) {
        // SAFETY: as in do_expire_lowres_timers.
        unsafe {
            TimerSet::complete(
                std::ptr::addr_of_mut!((*this)._manual_timers),
                std::ptr::addr_of_mut!((*this)._expired_manual_timers),
                || {},
            )
        };
    }

    /// Whether a low-resolution timer has expired.
    fn do_check_lowres_timers(&self) -> bool {
        LowresClock::now() > self._lowres_next_timeout
    }
}

impl ManualClock {
    /// Runs the expired manual-clock timers of this shard.
    pub fn expire_timers() {
        // SAFETY: called on a reactor thread (from a task or from advance()), with no
        // reference to the reactor live.
        unsafe { Reactor::expire_manual_timers(engine()) };
    }

    /// Advances the manual clock by `d` on every shard and runs the timers that expired.
    pub fn advance(d: Duration) {
        crate::engine::core::manual_clock::MANUAL_CLOCK_NOW
            .fetch_add(d.as_nanos() as i64, Ordering::SeqCst);
        if engine_is_ready() {
            // Expire timers on all cores in the background.
            // SAFETY: the engine of this thread is set; the reference lasts for the call,
            // which only queues the future behind the background gate.
            unsafe { &mut *engine() }.run_in_background_fn(|| {
                crate::engine::core::task::schedule_urgent(make_task_ptr(
                    default_scheduling_group(),
                    ManualClock::expire_timers,
                ));
                Smp::invoke_on_all(|| {
                    ManualClock::expire_timers();
                    make_ready_future(())
                })
            });
        }
    }
}

use crate::engine::core::internal::poll::SimplePollfn;

// The pollers of the main loop. Each holds the reactor as a raw
// pointer: a poller runs inside the main loop and calls out to code that re-enters the
// reactor, so it never forms a `&mut Reactor` that outlives a single field access.
//
// Every constructor is `unsafe`: `r` must be the live reactor of this thread, which outlives
// the poller (the pollers are locals of `do_run`).

/// Poller: submits pending kernel work through the backend.
pub struct KernelSubmitWorkPollfn {
    _r: *mut Reactor,
}

impl KernelSubmitWorkPollfn {
    /// # Safety
    /// See the pollers' contract above.
    pub unsafe fn new(r: *mut Reactor) -> Self {
        KernelSubmitWorkPollfn { _r: r }
    }
}

impl SimplePollfn for KernelSubmitWorkPollfn {
    const PASSIVE: bool = true;
    #[inline]
    fn poll(&mut self) -> bool {
        // SAFETY: pollers' contract; the backend is set for the reactor's whole life.
        unsafe {
            let (b, e) = Reactor::backend_entries(self._r);
            (e.kernel_submit_work)(b)
        }
    }
}

/// Poller: runs the handlers of pending signals.
pub struct SignalPollfn {
    _r: *mut Reactor,
}

impl SignalPollfn {
    /// # Safety
    /// See the pollers' contract above.
    pub unsafe fn new(r: *mut Reactor) -> Self {
        SignalPollfn { _r: r }
    }
}

impl Pollfn for SignalPollfn {
    #[inline]
    fn poll(&mut self) -> bool {
        // SAFETY: pollers' contract; the handlers re-enter the reactor, so the signals are
        // reached through a raw pointer.
        unsafe { Signals::poll_signal(std::ptr::addr_of_mut!((*self._r)._signals)) }
    }
    #[inline]
    fn pure_poll(&mut self) -> bool {
        // SAFETY: pollers' contract; a shared access to an atomic.
        unsafe { (*std::ptr::addr_of!((*self._r)._signals)).pure_poll_signal() }
    }
    fn try_enter_interrupt_mode(&mut self) -> bool {
        // Signals will interrupt our epoll_pwait() call, but
        // disable them now to avoid a signal between this point
        // and epoll_pwait()
        let mut block_all = SigsetT::default();
        // SAFETY: valid signal sets; `_active_sigmask` is a field of the live reactor.
        unsafe {
            sigfillset(&mut block_all);
            pthread_sigmask(
                SIG_SETMASK,
                &block_all,
                std::ptr::addr_of_mut!((*self._r)._active_sigmask),
            );
        }
        if self.poll() {
            // raced already, and lost
            self.exit_interrupt_mode();
            return false;
        }
        true
    }
    fn exit_interrupt_mode(&mut self) {
        // SAFETY: valid signal set in the live reactor.
        unsafe {
            pthread_sigmask(
                SIG_SETMASK,
                std::ptr::addr_of!((*self._r)._active_sigmask),
                std::ptr::null_mut(),
            )
        };
    }
}

/// Poller: flushes batched TCP streams.
pub struct BatchFlushPollfn {
    _r: *mut Reactor,
}

impl BatchFlushPollfn {
    /// # Safety
    /// See the pollers' contract above.
    pub unsafe fn new(r: *mut Reactor) -> Self {
        BatchFlushPollfn { _r: r }
    }
}

impl SimplePollfn for BatchFlushPollfn {
    const PASSIVE: bool = true;
    #[inline]
    fn poll(&mut self) -> bool {
        // SAFETY: pollers' contract.
        unsafe { Reactor::flush_tcp_batches(self._r) }
    }
}

/// Poller: reaps kernel completions through the backend.
pub struct ReapKernelCompletionsPollfn {
    _r: *mut Reactor,
}

impl ReapKernelCompletionsPollfn {
    /// # Safety
    /// See the pollers' contract above.
    pub unsafe fn new(r: *mut Reactor) -> Self {
        ReapKernelCompletionsPollfn { _r: r }
    }
}

impl Pollfn for ReapKernelCompletionsPollfn {
    #[inline]
    fn poll(&mut self) -> bool {
        // SAFETY: pollers' contract.
        unsafe { Reactor::reap_kernel_completions(self._r) }
    }
    #[inline]
    fn pure_poll(&mut self) -> bool {
        self.poll() // actually performs work, but triggers no user continuations, so okay
    }
    fn try_enter_interrupt_mode(&mut self) -> bool {
        // SAFETY: pollers' contract.
        unsafe { (*Reactor::backend_of(self._r)).kernel_events_can_sleep() }
    }
    fn exit_interrupt_mode(&mut self) {}
}

/// Poller: dispatches requests from the I/O queues into the sink.
pub struct IoQueueSubmissionPollfn {
    _r: *mut Reactor,
    // Wake-up the reactor with highres timer when the io-queue
    // decides to delay dispatching until some time point in
    // the future
    _nearest_wakeup: Timer<SteadyClockType>,
    _armed: Cell<bool>,
}

impl IoQueueSubmissionPollfn {
    /// The pollfn; its timer gets its callback (`[this] { _armed = false; }`) from
    /// [`bind_timer`](Self::bind_timer) once the pollfn sits in its poller.
    ///
    /// # Safety
    /// See the pollers' contract above.
    pub unsafe fn new(r: *mut Reactor) -> Self {
        IoQueueSubmissionPollfn { _r: r, _nearest_wakeup: Timer::new(), _armed: Cell::new(false) }
    }

    /// Sets the timer callback, which refers to `_armed` at its final address.
    ///
    /// # Safety
    /// `this` must be the pollfn inside a live poller, which does not move until it is
    /// dropped.
    pub unsafe fn bind_timer(this: *mut Self) {
        // SAFETY: guaranteed by the caller: the pollfn, and so its timer, stays in place.
        unsafe {
            let armed = std::ptr::addr_of!((*this)._armed);
            // The timer is owned by the pollfn, so `armed` outlives every callback.
            Pin::new_unchecked(&mut (*this)._nearest_wakeup)
                .set_callback(move || (*armed).set(false));
        }
    }

    /// The timer, pinned: the pollfn sits in its poller's block, which does not move.
    #[inline]
    fn nearest_wakeup(&mut self) -> Pin<&mut Timer<SteadyClockType>> {
        // SAFETY: the pollfn lives in the poller's allocation until it is dropped, and the
        // timer is cancelled by its own destructor.
        unsafe { Pin::new_unchecked(&mut self._nearest_wakeup) }
    }
}

impl Pollfn for IoQueueSubmissionPollfn {
    #[inline]
    fn poll(&mut self) -> bool {
        // SAFETY: pollers' contract.
        unsafe { Reactor::flush_pending_aio(self._r) }
    }
    #[inline]
    fn pure_poll(&mut self) -> bool {
        self.poll()
    }
    fn try_enter_interrupt_mode(&mut self) -> bool {
        // SAFETY: pollers' contract; a short shared access.
        let next = unsafe { (*self._r).next_pending_aio() };
        let now = SteadyClockType::now();
        if next <= now {
            return false;
        }
        self.nearest_wakeup().arm(next, None);
        self._armed.set(true);
        true
    }
    fn exit_interrupt_mode(&mut self) {
        if self._armed.get() {
            self.nearest_wakeup().cancel();
            self._armed.set(false);
        }
    }
}

// Other cpus can queue items for us to free; and they won't notify
// us about them.  But it's okay to ignore those items, freeing them
// doesn't have any side effects.
//
// We'll take care of those items when we wake up for another reason.
/// Poller: returns memory freed by other shards to this shard's allocator.
pub struct DrainCrossCpuFreelistPollfn;

impl SimplePollfn for DrainCrossCpuFreelistPollfn {
    const PASSIVE: bool = true;
    #[inline]
    fn poll(&mut self) -> bool {
        memory::drain_cross_cpu_freelist()
    }
}

/// Poller: runs the expired low-resolution timers.
pub struct LowresTimerPollfn {
    _r: *mut Reactor,
    // A highres timer is implemented as a waking  signal; so
    // we arm one when we have a lowres timer during sleep, so
    // it can wake us up.
    _nearest_wakeup: Timer<SteadyClockType>,
    _armed: Cell<bool>,
}

impl LowresTimerPollfn {
    /// The pollfn; its timer gets its callback (`[this] { _armed = false; }`) from
    /// [`bind_timer`](Self::bind_timer) once the pollfn sits in its poller.
    ///
    /// # Safety
    /// See the pollers' contract above.
    pub unsafe fn new(r: *mut Reactor) -> Self {
        LowresTimerPollfn { _r: r, _nearest_wakeup: Timer::new(), _armed: Cell::new(false) }
    }

    /// Sets the timer callback, which refers to `_armed` at its final address.
    ///
    /// # Safety
    /// As [`IoQueueSubmissionPollfn::bind_timer`].
    pub unsafe fn bind_timer(this: *mut Self) {
        // SAFETY: as in IoQueueSubmissionPollfn::bind_timer.
        unsafe {
            let armed = std::ptr::addr_of!((*this)._armed);
            Pin::new_unchecked(&mut (*this)._nearest_wakeup)
                .set_callback(move || (*armed).set(false));
        }
    }

    /// The timer, pinned: the pollfn sits in its poller's block, which does not move.
    #[inline]
    fn nearest_wakeup(&mut self) -> Pin<&mut Timer<SteadyClockType>> {
        // SAFETY: as in IoQueueSubmissionPollfn::nearest_wakeup.
        unsafe { Pin::new_unchecked(&mut self._nearest_wakeup) }
    }
}

impl Pollfn for LowresTimerPollfn {
    #[inline]
    fn poll(&mut self) -> bool {
        // SAFETY: pollers' contract.
        unsafe { Reactor::do_expire_lowres_timers(self._r) }
    }
    #[inline]
    fn pure_poll(&mut self) -> bool {
        // SAFETY: pollers' contract; a short shared access.
        unsafe { (*self._r).do_check_lowres_timers() }
    }
    fn try_enter_interrupt_mode(&mut self) -> bool {
        // arm our highres timer so a signal will wake us up
        // SAFETY: pollers' contract.
        let next = unsafe { (*self._r)._lowres_next_timeout };
        if next == <LowresClock as crate::engine::core::timer::Clock>::max_time_point() {
            // no pending timers
            return true;
        }
        let now = LowresClock::now();
        if next <= now {
            // whoops, go back
            return false;
        }
        // `next > now`, so the difference is positive.
        self.nearest_wakeup().arm_in(Duration::from_nanos((next - now) as u64));
        self._armed.set(true);
        true
    }
    fn exit_interrupt_mode(&mut self) {
        if self._armed.get() {
            self.nearest_wakeup().cancel();
            self._armed.set(false);
        }
    }
}

/// Poller: processes the inter-shard and alien message queues; it decides whether the reactor
/// may sleep and publishes the sleeping flag the senders read.
pub struct SmpPollfn {
    _r: *mut Reactor,
}

impl SmpPollfn {
    /// # Safety
    /// See the pollers' contract above.
    pub unsafe fn new(r: *mut Reactor) -> Self {
        SmpPollfn { _r: r }
    }

    #[inline]
    fn alien(&self) -> &alien::Instance {
        // SAFETY: pollers' contract; the alien instance outlives the reactor and is shared
        // with the alien threads.
        unsafe { &*(*self._r)._alien }
    }

    #[inline]
    fn sleeping(&self) -> &AtomicBool {
        // SAFETY: pollers' contract; a shared access to an atomic of the live reactor.
        unsafe { &(*std::ptr::addr_of!((*self._r)._sleeping)).0 }
    }
}

impl Pollfn for SmpPollfn {
    #[inline]
    fn poll(&mut self) -> bool {
        // Avoid short-circuiting with `||` since there are side effects
        // we want to take place (instantiating tasks from the alien queue).
        Smp::poll_queues() | self.alien().poll_queues()
    }
    #[inline]
    fn pure_poll(&mut self) -> bool {
        Smp::pure_poll_queues() || self.alien().pure_poll_queues()
    }
    fn try_enter_interrupt_mode(&mut self) -> bool {
        // systemwide_memory_barrier() is very slow if run concurrently,
        // so don't go to sleep if it is running now.
        self.sleeping().store(true, Ordering::Relaxed);
        let barrier_done =
            crate::engine::core::systemwide_memory_barrier::try_systemwide_memory_barrier();
        if !barrier_done {
            self.sleeping().store(false, Ordering::Relaxed);
            return false;
        }
        if self.poll() {
            // raced
            self.sleeping().store(false, Ordering::Relaxed);
            return false;
        }
        true
    }
    fn exit_interrupt_mode(&mut self) {
        self.sleeping().store(false, Ordering::Relaxed);
    }
}

/// Poller: runs the pending batches of the execution stages.
pub struct ExecutionStagePollfn {
    _esm: *mut crate::engine::core::execution_stage::internal::ExecutionStageManager,
}

impl Default for ExecutionStagePollfn {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecutionStagePollfn {
    /// The poller.
    pub fn new() -> Self {
        ExecutionStagePollfn {
            _esm: crate::engine::core::execution_stage::internal::ExecutionStageManager::get(),
        }
    }
}

impl Pollfn for ExecutionStagePollfn {
    #[inline]
    fn poll(&mut self) -> bool {
        // SAFETY: the execution stage manager is a thread-local singleton.
        unsafe { (*self._esm).flush() }
    }
    #[inline]
    fn pure_poll(&mut self) -> bool {
        // SAFETY: as above.
        unsafe { (*self._esm).poll() }
    }
    fn try_enter_interrupt_mode(&mut self) -> bool {
        // This is a passive poller, so if a previous poll
        // returned false (idle), there's no more work to do.
        true
    }
    fn exit_interrupt_mode(&mut self) {}
}

/// Poller: completes the work done by the syscall thread.
pub struct SyscallPollfn {
    _r: *mut Reactor,
}

impl SyscallPollfn {
    /// # Safety
    /// See the pollers' contract above.
    pub unsafe fn new(r: *mut Reactor) -> Self {
        SyscallPollfn { _r: r }
    }

    #[inline]
    fn pool(&self) -> &ThreadPool {
        // SAFETY: pollers' contract; the pool is set for the reactor's whole life and is
        // shared with its syscall thread, so only a shared reference is formed.
        unsafe { (*std::ptr::addr_of!((*self._r)._thread_pool)).as_deref().unwrap_unchecked() }
    }
}

impl Pollfn for SyscallPollfn {
    #[inline]
    fn poll(&mut self) -> bool {
        self.pool().complete()
    }
    #[inline]
    fn pure_poll(&mut self) -> bool {
        self.poll() // actually performs work, but triggers no user continuations, so okay
    }
    fn try_enter_interrupt_mode(&mut self) -> bool {
        self.pool().enter_interrupt_mode();
        if self.poll() {
            // raced
            self.pool().exit_interrupt_mode();
            return false;
        }
        true
    }
    fn exit_interrupt_mode(&mut self) {
        self.pool().exit_interrupt_mode();
    }
}

unsafe extern "C" {
    fn write(fd: i32, buf: *const std::ffi::c_void, n: usize) -> isize;
    fn read(fd: i32, buf: *mut std::ffi::c_void, n: usize) -> isize;
}

impl Reactor {
    /// Wakes the reactor if it sleeps.
    pub fn wakeup(&self) {
        // SAFETY: `self` is a live reactor.
        unsafe { Self::wakeup_from(self) }
    }

    /// Wakes the reactor from another thread (an alien thread or another shard):
    /// only the atomic flag and the eventfd's descriptor are reached, as places of `this`,
    /// so no reference to the reactor is formed while its own thread holds one.
    ///
    /// # Safety
    /// `this` points to a live reactor.
    pub(crate) unsafe fn wakeup_from(this: *const Self) {
        // SAFETY: the contract above; the flag is an atomic and the descriptor is written
        // once, at construction, before any other thread can reach the reactor.
        let (sleeping, notify_fd) = unsafe {
            (
                &*std::ptr::addr_of!((*this)._sleeping),
                (*std::ptr::addr_of!((*this)._notify_eventfd)).get(),
            )
        };
        if !sleeping.load(Ordering::Relaxed) {
            return;
        }

        // We are free to clear it, because we're sending a signal now
        sleeping.store(false, Ordering::Relaxed);

        let one: u64 = 1;
        // SAFETY: writing 8 bytes from a local to our eventfd.
        let res = unsafe { write(notify_fd, &one as *const u64 as *const std::ffi::c_void, 8) };
        engine_assert!(res == 8 /* "write(2) failed on _reactor._notify_eventfd" */);
    }

    /// Starts the loop that reads the aio eventfd, when one is used.
    ///
    /// # Safety
    /// `this` is the live reactor of this thread, with no reference to it live.
    unsafe fn start_aio_eventfd_loop(this: *mut Self) {
        // SAFETY: guaranteed by the caller.
        if unsafe { (*this)._aio_eventfd.is_none() } {
            return;
        }
        // SAFETY (closures): the reactor outlives the loop, which ends at exit; each access
        // is a short one through `this`.
        let loop_done = crate::engine::core::loop_::repeat(move || {
            // SAFETY: as above; `_aio_eventfd` was checked to be `Some` and is never cleared while
            // the loop runs.
            let efd = unsafe { (*this)._aio_eventfd.as_mut().unwrap_unchecked() };
            efd.readable().then(move |()| {
                let mut garbage = [0u8; 8];
                // Reading 8 bytes into a local buffer; totally uninteresting.
                // SAFETY: as above; the descriptor is open and `garbage` is valid for writes of
                // 8 bytes.
                let _ = unsafe {
                    read(
                        (*this)._aio_eventfd.as_ref().unwrap_unchecked().get_fd(),
                        garbage.as_mut_ptr() as *mut std::ffi::c_void,
                        8,
                    )
                };
                // SAFETY: as above, a short field read through `this`.
                make_ready_future(if unsafe { (*this)._stopping } {
                    crate::engine::core::loop_::StopIteration::YES
                } else {
                    crate::engine::core::loop_::StopIteration::NO
                })
            })
        });
        // must use make_lw_shared, because at_exit expects a copyable function
        let mut loop_done = Some(loop_done);
        // SAFETY: guaranteed by the caller; the reference lasts for the call.
        unsafe {
            (*this).do_at_exit(NoncopyableFunction::<fn() -> Future<()>>::new(move || {
                loop_done.take().unwrap_or_else(|| make_ready_future(()))
            }))
        };
    }

    /// Stops the aio eventfd loop by writing to the eventfd.
    fn stop_aio_eventfd_loop(&mut self) {
        let Some(efd) = &self._aio_eventfd else {
            return;
        };
        let one: u64 = 1;
        // SAFETY: writing 8 bytes from a local to the eventfd.
        let res = unsafe { write(efd.get_fd(), &one as *const u64 as *const std::ffi::c_void, 8) };
        engine_assert!(res == 8 /* "write(2) failed on _reactor._aio_eventfd" */);
    }

    #[inline]
    fn have_more_tasks(&self) -> bool {
        self._active_task_queues.size() + self._activating_task_queues.size() != 0
    }

    fn insert_active_task_queue(&mut self, tq: *mut TaskQueue) {
        // SAFETY: `tq` points to a task queue owned by `_task_queues`.
        unsafe { (*tq)._active = true };
        let atq = &mut self._active_task_queues;
        let less = task_queue_indirect_compare;
        if atq.empty() || less(atq.back(), &tq) {
            // Common case: idle->working
            // Common case: CPU intensive task queue going to the back
            atq.push_back(tq);
        } else {
            // Common case: newly activated queue preempting everything else
            atq.push_front(tq);
            // Less common case: newly activated queue behind something already active
            let mut i = 0usize;
            while i + 1 != atq.size() && !less(&atq[i], &atq[i + 1]) {
                // Swap entries i and i + 1.
                let t = atq[i];
                atq[i] = atq[i + 1];
                atq[i + 1] = t;
                i += 1;
            }
        }
    }

    /// Removes and returns the first active task queue (the lowest virtual runtime), adding
    /// the time since its last state change to its starve time.
    fn pop_active_task_queue(&mut self, now: SchedTimePoint) -> *mut TaskQueue {
        let tq = *self._active_task_queues.front();
        self._active_task_queues.pop_front();
        // SAFETY: `tq` points to a task queue owned by `_task_queues`.
        unsafe { (*tq)._starvetime += now - (*tq)._ts };
        tq
    }

    fn insert_activating_task_queues(&mut self) {
        // Quadratic, but since we expect the common cases in insert_active_task_queue() to dominate, faster
        for i in 0..self._activating_task_queues.size() {
            let tq = self._activating_task_queues[i];
            self.insert_active_task_queue(tq);
        }
        self._activating_task_queues.clear();
    }

    /// The task queue of a scheduling group.
    #[inline]
    fn task_queue_of(&mut self, sg: SchedulingGroup) -> *mut TaskQueue {
        // SAFETY: every scheduling group id that a task can carry has a task queue (the
        // group is created before any task of it exists and destroyed after the last one).
        unsafe {
            let slot = self._task_queues.get_unchecked(sg.id as usize);
            slot.as_ref().unwrap_unchecked().as_ptr()
        }
    }

    /// Queues `t` at the back of its scheduling group's task queue.
    #[inline]
    pub fn add_task(&mut self, t: TaskPtr) {
        // The queue may grow here, and nothing can handle a failure: the reference's add_task
        // is noexcept, so a failed allocation terminates there. The growth is a critical
        // allocation, which the failure injector never fails (a real failure aborts), as in
        // add_urgent_task: a failure injected here would unwind out of the scheduling of a
        // ready promise, in the middle of its state change.
        let _section =
            crate::engine::util::critical_alloc_section::memory::ScopedCriticalAllocSection::new();
        // SAFETY: `t` is a live task handed to the reactor.
        let sg = unsafe { t.group() };
        let q = self.task_queue_of(sg);
        // SAFETY: `q` is the root pointer of a queue node of `_task_queues`; the run loop
        // and the active lists hold copies of that same pointer and form no reference that
        // spans a task, so this short `&mut` overlaps no other reference.
        let qr = unsafe { &mut *q };
        let was_empty = qr._q.empty();
        qr._q.push_back(t);
        shuffle_back(&mut qr._q);
        if was_empty {
            self.activate(q);
        }
    }

    /// Queues `t` at the front of its scheduling group's task queue.
    pub fn add_urgent_task(&mut self, t: TaskPtr) {
        let _section =
            crate::engine::util::critical_alloc_section::memory::ScopedCriticalAllocSection::new();
        // SAFETY: `t` is a live task handed to the reactor.
        let sg = unsafe { t.group() };
        let q = self.task_queue_of(sg);
        // SAFETY: as in add_task.
        let qr = unsafe { &mut *q };
        let was_empty = qr._q.empty();
        qr._q.push_front(t);
        shuffle_front(&mut qr._q);
        if was_empty {
            self.activate(q);
        }
    }

    /// Runs `f` in the background: nobody waits for it, a failure is logged, and the
    /// reactor's background gate waits for it when the reactor closes.
    pub fn run_in_background(&mut self, f: Future<()>) {
        // The background gate is closed when the reactor closes.
        // SAFETY: the reactor is constructed in place and never moves, so its gate is pinned.
        let g = unsafe { std::pin::Pin::new_unchecked(&self._background_gate) };
        let r = crate::engine::core::gate::with_gate(g, move || {
            f.handle_exception(|ex: ExceptionPtr| {
                engine_logger().warn(
                    crate::engine::util::log::FormatInfo::new(
                        "Ignored background task failure: {}",
                    ),
                    format_args!(
                        "Ignored background task failure: {}",
                        crate::engine::util::log::DisplayExceptionPtr(Some(&ex))
                    ),
                );
            })
        });
        if let Err(e) = r {
            // Swallow gate_closed_exception in particular
            engine_logger().error(
                crate::engine::util::log::FormatInfo::new("run_in_background: {}"),
                format_args!(
                    "run_in_background: {}",
                    crate::engine::util::log::DisplayExceptionPtr(Some(&e))
                ),
            );
        }
    }

    // Waits for all background tasks on all shards
    fn drain() -> Future<()> {
        engine_logger().debug(
            crate::engine::util::log::FormatInfo::new("reactor::drain"),
            format_args!("reactor::drain"),
        );
        Smp::invoke_on_all(|| {
            // SAFETY: runs on a reactor thread; the gate is a field of the reactor, which is
            // constructed in place, and is reached with a shared reference (its state is
            // interior).
            let g = unsafe { &*std::ptr::addr_of!((*engine())._background_gate) };
            if g.is_closed() {
                return make_ready_future(());
            }
            g.close()
        })
    }

    /// Runs tasks from the active task queues until preemption is needed or none is left.
    /// An error starting the stall detector's run escapes to `run()`.
    ///
    /// # Safety
    /// `this` is the live reactor of this thread, with no reference to it live: the tasks
    /// re-enter it.
    unsafe fn run_some_tasks(this: *mut Self) -> Result<(), ExceptionPtr> {
        // SAFETY: guaranteed by the caller; every `&mut *this` below is a temporary that
        // ends before run_tasks() runs a task.
        unsafe {
            if !(*this).have_more_tasks() {
                return Ok(());
            }
            // do_run() ends the idle period when a poll reports work, but tasks can become
            // runnable without one: the events handled when waking from sleep, or work a
            // poller picks up in its last check before sleeping. End it here too, so time
            // spent running tasks is never counted as idle.
            (*this).end_idle();
            Self::reset_preemption_monitor(this);
            LowresClock::update();

            let mut t_run_completed = Self::now();
            // STAP_PROBE(engine, reactor_run_tasks_start);
            (*this).cpu_stall_detector().start_task_run(t_run_completed)?;
            loop {
                let t_run_started = t_run_completed;
                (*this).insert_activating_task_queues();
                let tq = (*this).pop_active_task_queue(t_run_started);
                // `tq` is the root pointer of a queue node of `_task_queues` (add_task reaches
                // the queue through the same pointer, so a task that schedules into it leaves
                // `tq` valid); the queues are only destroyed by destroy_scheduling_group, from
                // a task of another group.
                (*this)._last_vruntime = (*tq)._vruntime.max((*this)._last_vruntime);
                Self::run_tasks(this, tq);
                t_run_completed = Self::now();
                let delta = t_run_completed - t_run_started;
                (*this).account_runtime(&mut *tq, delta);
                (*tq)._ts = t_run_completed;
                if !(*tq)._q.empty() {
                    (*this).insert_active_task_queue(tq);
                } else {
                    (*tq)._active = false;
                }
                // We must not use internal::scheduler_need_preempt() below,
                // since in debug mode we'll never have two successive calls
                // to internal::scheduler_need_preempt() both return true, so
                // an infinite loop with yield() will never terminate.
                //
                // Settle on a regular need_preempt(), which will return true in
                // debug mode.
                if !((*this).have_more_tasks() && !crate::engine::core::preempt::need_preempt()) {
                    break;
                }
            }
            (*this).cpu_stall_detector().end_task_run(t_run_completed);
        }
        // STAP_PROBE(engine, reactor_run_tasks_end);
        set_current_scheduling_group(default_scheduling_group()); // Prevent inheritance from last group run
        Ok(())
    }

    /// The stall detector, which
    /// the constructor always creates.
    #[inline]
    fn cpu_stall_detector(&mut self) -> &mut CpuStallDetector {
        // SAFETY: set by construct_in_place before the reactor runs, cleared only by drop.
        unsafe { self._cpu_stall_detector.as_deref_mut().unwrap_unchecked() }
    }

    fn activate(&mut self, tq: *mut TaskQueue) {
        // SAFETY: `tq` points to a task queue owned by `_task_queues`, with no other
        // reference to it live.
        let tq = unsafe { &mut *tq };
        if tq._active {
            return;
        }
        // If activate() was called, the task queue is likely network-bound or I/O bound, not CPU-bound. As
        // such its vruntime will be low, and it will have a large advantage over other task queues. Limit
        // the advantage so it doesn't dominate scheduling for a long time, in case it _does_ become CPU
        // bound later.
        //
        // FIXME: different scheduling groups have different sensitivity to jitter, take advantage
        tq._vruntime = self._last_vruntime.max(tq._vruntime);
        let now = Reactor::now();
        tq._waittime += now - tq._ts;
        tq._ts = now;
        self._activating_task_queues.push_back(tq as *mut TaskQueue);
    }

    /// Runs the expired high resolution timers.
    ///
    /// # Safety
    /// As [`Reactor::do_expire_lowres_timers`].
    pub(crate) unsafe fn service_highres_timer(this: *mut Self) {
        // SAFETY: as in do_expire_lowres_timers.
        unsafe {
            TimerSet::complete(
                std::ptr::addr_of_mut!((*this)._timers),
                std::ptr::addr_of_mut!((*this)._expired_timers),
                || {
                    // The callback runs inside `complete`, after the last timer callback.
                    if !(*this)._timers.empty() {
                        let next = (*this)._timers.get_next_timeout();
                        (*this).enable_timer(next);
                    }
                },
            )
        };
    }

    /// Runs the reactor's main loop until it stops; returns the exit code.
    ///
    /// # Safety
    /// `this` is the live reactor of this thread (`engine()`), with no reference to it live
    /// during the call: the loop runs every task of the shard.
    pub unsafe fn run(this: *mut Self) -> i32 {
        // SAFETY: guaranteed by the caller.
        match unsafe { Self::do_run(this) } {
            Ok(r) => r,
            Err(e) => {
                engine_logger().error(
                    crate::engine::util::log::FormatInfo::new("{}"),
                    format_args!("{}", crate::engine::util::log::DisplayExceptionPtr(Some(&e))),
                );
                print_with_backtrace("exception running reactor main loop", false, false);
                // SAFETY: `_exit` terminates the process.
                unsafe { _exit(1) }
            }
        }
    }

    /// The main loop: registers the pollers, runs tasks and polls until the reactor stops.
    ///
    /// # Safety
    /// As [`Reactor::run`]. Every `(*this)` below is a short access that ends before code
    /// that re-enters the reactor runs.
    unsafe fn do_run(this: *mut Self) -> Result<i32, ExceptionPtr> {
        let _signal_stack = install_signal_handler_stack()?;

        // SAFETY: guaranteed by the caller.
        unsafe { (*this).register_metrics()? };

        // The order in which we execute the pollers is very important for performance.
        //
        // This is because events that are generated in one poller may feed work into others. If
        // they were reversed, we'd only be able to do that work in the next task quota.
        //
        // One example is the relationship between the smp poller and the I/O submission poller:
        // If the smp poller runs first, requests from remote I/O queues can be dispatched right away
        //
        // We will run the pollers in the following order:
        //
        // 1. SMP: any remote event arrives before anything else
        // 2. reap kernel events completion: storage related completions may free up space in the I/O
        //                                   queue.
        // 4. I/O queue: must be after reap, to free up events. If new slots are freed may submit I/O
        // 5. kernel submission: for I/O, will submit what was generated from last step.
        // 6. reap kernel events completion: some of the submissions from last step may return immediately.
        //                                   For example if we are dealing with poll() on a fd that has events.
        // SAFETY: `this` is the live reactor of this thread, which outlives these locals (the
        // pollers' contract); the same holds for each poller constructor below.
        let _smp_poller = Poller::new(unsafe { SmpPollfn::new(this) });

        // SAFETY: as above, the pollers' contract: `this` outlives this local.
        let _reap_kernel_completions_poller =
            Poller::new(unsafe { ReapKernelCompletionsPollfn::new(this) });
        // SAFETY: as above, the pollers' contract: `this` outlives this local.
        let _io_queue_submission_poller =
            Poller::new(unsafe { IoQueueSubmissionPollfn::new(this) });
        // SAFETY: the pollfn sits in the poller's allocation, which outlives its timer.
        unsafe {
            IoQueueSubmissionPollfn::bind_timer(_io_queue_submission_poller.pollfn_ptr().cast())
        };
        // SAFETY: as above, the pollers' contract: `this` outlives this local.
        let _kernel_submit_work_poller = Poller::new(unsafe { KernelSubmitWorkPollfn::new(this) });
        // SAFETY: as above, the pollers' contract: `this` outlives this local.
        let _final_real_kernel_completions_poller =
            Poller::new(unsafe { ReapKernelCompletionsPollfn::new(this) });

        // SAFETY: as above, the pollers' contract: `this` outlives this local.
        let _batch_flush_poller = Poller::new(unsafe { BatchFlushPollfn::new(this) });
        let _execution_stage_poller = Poller::new(ExecutionStagePollfn::new());

        // SAFETY: guaranteed by the caller.
        unsafe { Self::start_aio_eventfd_loop(this) };

        // SAFETY: short accesses through `this`; the handlers run on this reactor's thread
        // (from the signal poller) while the reactor lives, with no reference to it live.
        unsafe {
            if (*this)._id == 0 && (*this)._cfg.auto_handle_sigint_sigterm {
                if (*this)._cfg.handle_sigint {
                    (*this)._signals.handle_signal_once(
                        SIGINT,
                        NoncopyableFunction::<fn()>::new(move || Self::stop(this)),
                    )?;
                }
                (*this)._signals.handle_signal_once(
                    SIGTERM,
                    NoncopyableFunction::<fn()>::new(move || Self::stop(this)),
                )?;
            }
        }

        // SAFETY: as above.
        if unsafe { (*this)._id } == 0 {
            // Start initialization in the background.
            // Wait for network stack to appear on all cpus.
            // Communicate when done using _start_promise
            // SAFETY (closures): each runs as a task on a reactor thread, with no reference
            // to that reactor live; the accesses are short.
            let _ = Smp::invoke_on_all(|| {
                // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
                let Some(ready) = (unsafe { (*engine())._network_stack_ready.take() }) else {
                    return make_ready_future(());
                };
                ready.then(|stack: Box<dyn NetworkStack>| {
                    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
                    unsafe { (*engine())._network_stack = Some(stack) };
                    make_ready_future(())
                })
            })
            .then(|()| {
                Smp::invoke_on_all(|| {
                    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
                    unsafe { (*engine()).net() }.initialize().then(|()| {
                        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
                        unsafe { (*engine())._start_promise.set_value(()) };
                        make_ready_future(())
                    })
                })
            });
        }

        // SAFETY: as above, the pollers' contract: `this` outlives this local.
        let _syscall_poller = Poller::new(unsafe { SyscallPollfn::new(this) });

        let _drain_cross_cpu_freelist = Poller::new(DrainCrossCpuFreelistPollfn);

        // SAFETY: as above, the pollers' contract: `this` outlives this local.
        let _expire_lowres_timers = Poller::new(unsafe { LowresTimerPollfn::new(this) });
        // SAFETY: the pollfn sits in the poller's allocation, which outlives its timer.
        unsafe { LowresTimerPollfn::bind_timer(_expire_lowres_timers.pollfn_ptr().cast()) };
        // SAFETY: as above, the pollers' contract: `this` outlives this local.
        let _sig_poller = Poller::new(unsafe { SignalPollfn::new(this) });

        let mut load_timer = std::pin::pin!(Timer::<LowresClock>::new());
        let mut last_idle = SchedDuration::default();
        load_timer.as_mut().set_callback(move || {
            // SAFETY: the timer is cancelled before do_run() returns; its callback runs from
            // the lowres poller, with no reference to the reactor live.
            let me = unsafe { &mut *this };
            me.update_idle();
            let mut load = (me._total_idle - last_idle).count() as f64
                / SchedDuration::from_std(Duration::from_secs(1)).count() as f64;
            last_idle = me._total_idle;
            load = load.min(1.0);
            me._loads.push_front(load);
            if me._loads.size() > 5 {
                let drop = *me._loads.back();
                me._loads.pop_back();
                me._load -= drop / 5.0;
            }
            me._load += load / 5.0;
        });
        load_timer.as_mut().arm_periodic(Duration::from_secs(1));

        // SAFETY: short accesses to the live reactor.
        let task_quote_itimerspec = unsafe {
            let its = crate::engine::core::posix::to_relative_itimerspec(
                (*this)._cfg.task_quota.to_std(),
                (*this)._cfg.task_quota.to_std(),
            );
            (*this)._task_quota_timer.timerfd_settime(0, &its)?;
            its
        };

        let sa_block_notifier = Sigaction {
            sa_sigaction: Reactor::block_notifier as *const () as usize,
            sa_flags: SA_RESTART,
            ..Sigaction::default()
        };
        // SAFETY: `sa_block_notifier` is a valid sigaction.
        let r = unsafe {
            sigaction(CpuStallDetector::signal_number(), &sa_block_notifier, std::ptr::null_mut())
        };
        engine_assert!(r == 0);

        // While the reactor is in a run of empty polls, when the run began, which decides when
        // to sleep. Tasks end the idle period but not the run, so when the tasks a wakeup made
        // runnable leave nothing else to do, the reactor sleeps again after one empty poll
        // rather than polling for max_poll_time first.
        let mut idle_poll_start: Option<SchedTimePoint> = None;

        let check_for_work = move || -> bool {
            // SAFETY: called from this loop, with no reference to the reactor live; the pollers
            // re-enter it through `this`, as `poll_once` requires.
            unsafe { Self::poll_once(this) || (*this).have_more_tasks() }
        };
        let pure_check_for_work = move || -> bool {
            // SAFETY: as for `check_for_work`, as `pure_poll_once` requires.
            unsafe { Self::pure_poll_once(this) || (*this).have_more_tasks() }
        };
        // SAFETY: every access below is a short one through `this`, and the calls that
        // re-enter the reactor take `this`.
        unsafe {
            loop {
                Self::run_some_tasks(this)?;
                if (*this)._stopped {
                    load_timer.as_mut().cancel();
                    // Final tasks may include sending the last response to cpu 0, so run them
                    while (*this).have_more_tasks() {
                        Self::run_some_tasks(this)?;
                    }
                    // `_at_destroy_tasks` points into `_task_queues`.
                    while !(*(*this)._at_destroy_tasks)._q.empty() {
                        Self::run_tasks(this, (*this)._at_destroy_tasks);
                    }
                    (*this)._finished_running_tasks = true;
                    (*this)._smp.arrive_at_event_loop_end();
                    if (*this)._id == 0 {
                        (*this)._smp.join_all();
                    }
                    break;
                }

                (*this)._polls += 1;

                LowresClock::update(); // Don't delay expiring lowres timers
                if check_for_work() {
                    (*this).end_idle();
                    idle_poll_start = None;
                } else {
                    let t = Self::now();
                    (*this).mark_idle(t);
                    let poll_start = *idle_poll_start.get_or_insert(t);
                    // we can't run check_for_work(), because that can run tasks in the context
                    // of the idle handler which change its state, without the idle handler expecting
                    // it.  So run pure_check_for_work() instead.
                    let handler = std::ptr::addr_of_mut!((*this)._idle_cpu_handler);
                    let handler_result =
                        (*handler).call(WorkWaitingOnReactor::new(&pure_check_for_work));
                    let go_to_sleep = handler_result == IdleCpuHandlerResult::NoMoreWork;
                    if go_to_sleep {
                        crate::engine::util::spinlock::internal::cpu_relax();
                        // Only when every poller agrees to sleep are the task quota timer and the
                        // stall detector's timer touched: a refused sleep costs no system call.
                        if t - poll_start > (*this)._cfg.max_poll_time
                            && Self::pollers_enter_interrupt_mode(this)
                        {
                            // Turn off the task quota timer to avoid spurious wakeups
                            let zero_itimerspec = crate::engine::core::posix::Itimerspec::default();
                            (*this)._task_quota_timer.timerfd_settime(0, &zero_itimerspec)?;
                            if let Some(d) = (*this)._cpu_stall_detector.as_mut() {
                                d.start_sleep()?;
                            }

                            Self::wait_and_process_events(this);
                            Self::pollers_exit_interrupt_mode(this);

                            if let Some(d) = (*this)._cpu_stall_detector.as_mut() {
                                d.end_sleep();
                            }
                            // We may have slept for a while, so freshen _idle_end
                            (*this).mark_idle(Self::now());
                            (*this)._task_quota_timer.timerfd_settime(0, &task_quote_itimerspec)?;
                        }
                    } else {
                        // We previously ran pure_check_for_work(), might not actually have performed
                        // any work.
                        check_for_work();
                    }
                }
            }
            // To prevent ordering issues from rising, destroy the I/O queue explicitly at this point.
            // This is needed because the reactor is destroyed from the thread_local destructors. If
            // the I/O queue happens to use any other infrastructure that is also kept this way (for
            // instance, collectd), we will not have any way to guarantee who is destroyed first.
            (*this)._io_queues.clear();
            Ok((*this)._return)
        }
    }

    /// Sleeps in the backend when every poller agrees (entering interrupt mode), otherwise
    /// returns at once.
    ///
    /// # Safety
    /// As `Reactor::run_some_tasks`: entering and leaving interrupt mode, and processing
    /// the events, re-enter the reactor.
    pub unsafe fn try_sleep(this: *mut Self) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            if !Self::pollers_enter_interrupt_mode(this) {
                return;
            }
            Self::wait_and_process_events(this);
            Self::pollers_exit_interrupt_mode(this);
        }
    }

    /// Puts every poller in interrupt mode and returns `true`, or, when one refuses, takes
    /// the ones before it out again and returns `false`.
    ///
    /// # Safety
    /// As [`Reactor::try_sleep`].
    unsafe fn pollers_enter_interrupt_mode(this: *mut Self) -> bool {
        // SAFETY: guaranteed by the caller. Registered pollers are live (they unregister
        // through a task, which cannot run here), and the vector is not changed meanwhile.
        unsafe {
            let pollers = std::ptr::addr_of!((*this)._pollers);
            let n = (&*pollers).len();
            for i in 0..n {
                let p: *mut dyn Pollfn = (&*pollers)[i];
                let ok = (*p).try_enter_interrupt_mode();
                if !ok {
                    for j in (0..i).rev() {
                        let p: *mut dyn Pollfn = (&*pollers)[j];
                        (*p).exit_interrupt_mode();
                    }
                    return false;
                }
            }
            true
        }
    }

    /// Takes every poller out of interrupt mode, in reverse order.
    ///
    /// # Safety
    /// As [`Reactor::try_sleep`], after [`Reactor::pollers_enter_interrupt_mode`] succeeded.
    unsafe fn pollers_exit_interrupt_mode(this: *mut Self) {
        // SAFETY: as in pollers_enter_interrupt_mode.
        unsafe {
            let pollers = std::ptr::addr_of!((*this)._pollers);
            for i in (0..(&*pollers).len()).rev() {
                let p: *mut dyn Pollfn = (&*pollers)[i];
                (*p).exit_interrupt_mode();
            }
        }
    }

    /// Waits in the backend for events and processes them.
    ///
    /// # Safety
    /// As [`Reactor::try_sleep`].
    unsafe fn wait_and_process_events(this: *mut Self) {
        // SAFETY: as in try_sleep; processing the events re-enters the reactor.
        unsafe {
            let mask = (*this)._active_sigmask;
            let (b, e) = Self::backend_entries(this);
            (e.wait_and_process_events)(b, Some(&mask));
        }
    }

    /// Returns `true` if all pollers allow blocking.
    ///
    /// Returns `false` if at least one of the blockers requires a non-blocking execution.
    ///
    /// # Safety
    /// As [`Reactor::try_sleep`].
    #[inline]
    unsafe fn poll_once(this: *mut Self) -> bool {
        let mut work = false;
        // SAFETY: guaranteed by the caller. A poller may not add or remove pollers (it
        // schedules a task for that), so the vector is stable during the loop; `i < len`.
        unsafe {
            let pollers = std::ptr::addr_of!((*this)._pollers);
            for i in 0..(&*pollers).len() {
                let p: *mut dyn Pollfn = *(&*pollers).get_unchecked(i);
                work |= (*p).poll();
            }
        }

        work
    }

    /// Whether any poller has work, without doing it.
    ///
    /// # Safety
    /// As [`Reactor::try_sleep`].
    #[inline]
    unsafe fn pure_poll_once(this: *mut Self) -> bool {
        // SAFETY: as in poll_once().
        unsafe {
            let pollers = std::ptr::addr_of!((*this)._pollers);
            for i in 0..(&*pollers).len() {
                let p: *mut dyn Pollfn = *(&*pollers).get_unchecked(i);
                if (*p).pure_poll() {
                    return true;
                }
            }
        }
        false
    }

    /// Adds the poller `p` to the main loop.
    ///
    /// A poller ([`Pollfn`]) is non-blocking and is called on every iteration of the main
    /// loop. The loop sleeps only when every poller agrees to enter interrupt mode: one
    /// that refuses keeps the loop from blocking in that iteration.
    pub(crate) fn register_poller(&mut self, p: *mut dyn Pollfn) {
        self._pollers.push(p);
    }

    /// Removes a poller from the main loop.
    pub(crate) fn unregister_poller(&mut self, p: *mut dyn Pollfn) {
        if let Some(pos) = self._pollers.iter().position(|&x| std::ptr::addr_eq(x, p)) {
            self._pollers.remove(pos);
        }
    }

    /// Replaces a poller in the main loop (used when a poller is dropped while the loop may
    /// be iterating).
    pub(crate) fn replace_poller(&mut self, old: *mut dyn Pollfn, neww: *mut dyn Pollfn) {
        for x in self._pollers.iter_mut() {
            if std::ptr::addr_eq(*x, old) {
                *x = neww;
            }
        }
    }
}

unsafe extern "C" {
    fn _exit(status: i32) -> !;
}

// (poller, poller::registration_task and poller::deregistration_task are in
// core/internal/poll.rs; syscall_work_queue's constructor, submit_item() and complete() are
// in core/syscall_work_queue.rs.)

/// The completion of an `fdatasync` submitted with linux-aio by [`Reactor::fdatasync`].
#[repr(C)]
struct FsyncIoDesc {
    base: IoCompletion,
    _pr: Promise<()>,
}

// SAFETY: `#[repr(C)]` with the `IoCompletion` built for `Self` at offset 0.
unsafe impl IoCompletionImpl for FsyncIoDesc {
    unsafe fn complete(this: *mut Self, _res: usize) {
        // SAFETY: the trait's contract: `this` is live and no reference to it is live;
        // `me` is not used after the object is destroyed.
        let me = unsafe { &mut *this };
        me._pr.set_value(());
        // SAFETY: `delete this`: the descriptor was leaked by `fdatasync`.
        drop(unsafe { Box::from_raw(this) });
    }

    unsafe fn set_exception(this: *mut Self, eptr: ExceptionPtr) {
        // SAFETY: the trait's contract: `this` is live and no reference to it is live;
        // `me` is not used after the object is destroyed.
        let me = unsafe { &mut *this };
        me._pr.set_exception(eptr);
        // SAFETY: as above.
        drop(unsafe { Box::from_raw(this) });
    }
}

// Adding, queueing and removing timers, for the three clocks a timer can
// use. A timer is armed and cancelled from anywhere, timer callbacks included, so the reactor
// and its timer set are reached through `engine()` with short accesses: no reference to the
// reactor is live while `TimerSet::complete` runs the callbacks (see service_highres_timer).

impl TimerClock for SteadyClockType {
    fn add_timer(tmr: NonNull<TimerImpl<Self>>) {
        if Self::queue_timer(tmr) {
            let r = engine();
            // SAFETY: on the reactor thread; short accesses to the live reactor, and arming
            // the highres timer runs no user code.
            unsafe {
                let next = (*r)._timers.get_next_timeout();
                (*r).enable_timer(next);
            }
        }
    }

    fn queue_timer(tmr: NonNull<TimerImpl<Self>>) -> bool {
        // SAFETY: on the reactor thread; the timer is live and pinned while it is armed (lane
        // 1's Timer contract), and the set is reached for this call only.
        unsafe {
            let r = engine();
            (*std::ptr::addr_of_mut!((*r)._timers)).insert(tmr)
        }
    }

    fn del_timer(tmr: NonNull<TimerImpl<Self>>) {
        // SAFETY: as in queue_timer; a queued timer is in the set or in the expired list.
        unsafe {
            let r = engine();
            (*std::ptr::addr_of_mut!((*r)._timers))
                .remove_or_expired(tmr, &mut *std::ptr::addr_of_mut!((*r)._expired_timers));
        }
    }
}

impl TimerClock for LowresClock {
    fn add_timer(tmr: NonNull<TimerImpl<Self>>) {
        if Self::queue_timer(tmr) {
            let r = engine();
            // SAFETY: on the reactor thread; short accesses to the live reactor.
            unsafe { (*r)._lowres_next_timeout = (*r)._lowres_timers.get_next_timeout() };
        }
    }

    fn queue_timer(tmr: NonNull<TimerImpl<Self>>) -> bool {
        // SAFETY: as for SteadyClockType.
        unsafe {
            let r = engine();
            (*std::ptr::addr_of_mut!((*r)._lowres_timers)).insert(tmr)
        }
    }

    fn del_timer(tmr: NonNull<TimerImpl<Self>>) {
        // SAFETY: as for SteadyClockType.
        unsafe {
            let r = engine();
            (*std::ptr::addr_of_mut!((*r)._lowres_timers))
                .remove_or_expired(tmr, &mut *std::ptr::addr_of_mut!((*r)._expired_lowres_timers));
        }
    }
}

impl TimerClock for ManualClock {
    fn add_timer(tmr: NonNull<TimerImpl<Self>>) {
        Self::queue_timer(tmr);
    }

    fn queue_timer(tmr: NonNull<TimerImpl<Self>>) -> bool {
        // SAFETY: as for SteadyClockType.
        unsafe {
            let r = engine();
            (*std::ptr::addr_of_mut!((*r)._manual_timers)).insert(tmr)
        }
    }

    fn del_timer(tmr: NonNull<TimerImpl<Self>>) {
        // SAFETY: as for SteadyClockType.
        unsafe {
            let r = engine();
            (*std::ptr::addr_of_mut!((*r)._manual_timers))
                .remove_or_expired(tmr, &mut *std::ptr::addr_of_mut!((*r)._expired_manual_timers));
        }
    }
}

/// Appends the task types waiting in a queue that grew too long to `it`.
pub fn do_dump_task_queue<'a>(
    mut it: InserterIterator<'a>,
    tq: &TaskQueue,
) -> InserterIterator<'a> {
    use crate::engine::util::log_impl::internal::format_to;
    let _section =
        crate::engine::util::critical_alloc_section::memory::ScopedCriticalAllocSection::new();
    let mut infos: HashMap<&'static str, u32> = HashMap::new();
    for &tp in tq._q.iter() {
        // SAFETY: the queue holds live tasks.
        let ti = unsafe { tp.type_name() };
        *infos.entry(ti).or_insert(0) += 1;
    }
    it = format_to(
        it,
        format_args!("Too long queue accumulated for {} ({} tasks)\n", tq._name, tq._q.size()),
    );
    for (name, count) in &infos {
        it = format_to(it, format_args!(" {}: {}\n", count, name));
    }
    it
}

use crate::engine::util::log_impl::internal::InserterIterator;

// ---------------------------------------------------------------------------
// Inter-shard message queues.
// ---------------------------------------------------------------------------

use crate::engine::core::cacheline::CacheLineAligned;
use crate::engine::core::smp::{
    LfQueue, RxStats, SmpMessageQueue, SmpTimeoutClock, TxSide, TxStats, WorkItemPtr,
};

// Each queue `_qs[t][from]` is used by two shards at once: the sending
// shard `from` (submit, submit_item, move_pending, flush_request_batch, process_completions,
// pure_poll_tx, start, stop: `_tx`, `tx_stats`, `_metrics`, pushing `_pending` and popping
// `_completed`) and the receiving shard `t` (respond, flush_response_batch,
// has_unflushed_responses, process_incoming, pure_poll_rx: `_completed_fifo`, `rx_stats`,
// popping `_pending` and pushing `_completed`). The methods therefore take `this: *mut Self`
// and reach only their side's fields by raw place, never a reference to the whole queue; the
// two `LfQueue`s are shared by `&` (their ring buffers synchronize through atomics).
impl SmpMessageQueue {
    /// A queue for messages from reactor `from` to reactor `to`.
    pub fn new(from: *mut Reactor, to: *mut Reactor) -> Self {
        SmpMessageQueue {
            _pending: LfQueue::new(to),
            _completed: LfQueue::new(from),
            tx_stats: CacheLineAligned(TxStats::default()),
            _metrics: metrics::MetricGroups::new(),
            rx_stats: CacheLineAligned(RxStats::default()),
            // The tx side is left unconstructed until start(); the `Option` drops it only if
            // it was constructed.
            _tx: None,
            _completed_fifo: Vec::new(),
        }
    }

    /// Stops the queue, on the sending side.
    ///
    /// # Safety
    /// `this` is a live queue whose sending side is the current shard.
    pub unsafe fn stop(this: *mut Self) {
        // SAFETY: by the caller; `_metrics` belongs to the sending side.
        unsafe { (*this)._metrics.clear() };
    }

    /// The sending side: constructed by start() before any request is submitted.
    ///
    /// # Safety
    /// As for [`SmpMessageQueue::stop`]; the reference ends before any other access to `_tx`.
    #[inline(always)]
    unsafe fn tx_a<'a>(this: *mut Self) -> &'a mut TxSide {
        // SAFETY: by the caller; start() constructs the tx side before the queue is used to
        // send.
        unsafe { (*this)._tx.as_mut().unwrap_unchecked() }
    }

    /// # Safety
    /// As for [`SmpMessageQueue::stop`].
    pub(crate) unsafe fn move_pending(this: *mut Self) {
        // Pushes the pending items over the deque, in its two contiguous halves.
        // SAFETY: by the caller; the fifo and the stats are sending-side fields, `_pending`
        // is shared by `&`.
        unsafe {
            let fifo = &mut Self::tx_a(this).pending_fifo;
            let pending = &(*this)._pending;
            let (front, back) = fifo.as_slices();
            let mut nr = pending.base.push_range(front);
            if nr == front.len() {
                nr += pending.base.push_range(back);
            }
            if nr == 0 {
                return;
            }
            pending.maybe_wakeup();
            fifo.drain(..nr);
            let tx = &mut (*this).tx_stats.0;
            tx._current_queue_length += nr;
            tx._last_snt_batch = nr;
            tx._sent += nr;
        }
    }

    /// # Safety
    /// As for [`SmpMessageQueue::stop`].
    pub(crate) unsafe fn pure_poll_tx(this: *mut Self) -> bool {
        // Reads the ring's indices directly.
        // empty() is not const, so need const_cast.
        // SAFETY: by the caller; `_completed` is shared by `&`.
        unsafe { !(*this)._completed.base.empty() }
    }

    /// Queues `item` for shard `t` once its service group has a free unit, failing it if
    /// none frees up before `timeout`.
    ///
    /// # Safety
    /// As for [`SmpMessageQueue::stop`]; `item` is a live item owned by the caller, which
    /// hands it over.
    pub(crate) unsafe fn submit_item(
        this: *mut Self,
        t: ShardId,
        timeout: <SmpTimeoutClock as crate::engine::core::timer::Clock>::TimePoint,
        item: WorkItemPtr,
    ) {
        // matching signal() in process_completions()
        // SAFETY: the item is live and owned by this shard.
        let ssg_id =
            crate::engine::core::smp::internal::smp_service_group_id(unsafe { item.ssg() });
        // SAFETY: no smp service group is created or destroyed during this call.
        let sem = unsafe { crate::engine::core::smp::get_smp_service_groups_semaphore(ssg_id, t) };
        let this_addr = this as usize;
        // Future indirectly forwarded to `item`.
        // SAFETY: the service group semaphore outlives the units and the wait (a group is
        // destroyed only once its requests have completed).
        let _ = unsafe { crate::engine::core::semaphore::get_units_until(sem, 1, timeout) }
            .then_wrapped(
                move |units_fut: Future<
                    crate::engine::core::smp::SmpServiceGroupSemaphoreUnits,
                >| {
                    let this = this_addr as *mut SmpMessageQueue;
                    // SAFETY: the queue outlives the submissions to it (it is destroyed after the
                    // reactors stop) and this continuation runs on the sending shard; the item is still
                    // owned by it.
                    unsafe {
                        if units_fut.failed() {
                            item.fail_with(units_fut.get_exception());
                            let tx = &mut (*this).tx_stats.0;
                            tx._compl += 1;
                            tx._last_cmpl_batch += 1;
                            // The unique_ptr goes out of scope here.
                            item.destroy();
                            return;
                        }
                        Self::tx_a(this).pending_fifo.push_back(item);
                        // no exceptions from this point
                        if let Ok(mut units) = units_fut.get() {
                            units.release();
                        }
                        if Self::tx_a(this).pending_fifo.len() >= SmpMessageQueue::BATCH_SIZE {
                            Self::move_pending(this);
                        }
                    }
                },
            );
    }

    /// Sends the response of `item` back, on the receiving side.
    ///
    /// # Safety
    /// `this` is a live queue whose receiving side is the current shard, and `item` an item
    /// it received.
    pub(crate) unsafe fn respond(this: *mut Self, item: WorkItemPtr) {
        // SAFETY: by the caller; `_completed_fifo` belongs to the receiving side.
        let len = unsafe {
            let fifo = &mut (*this)._completed_fifo;
            fifo.push(item);
            fifo.len()
        };
        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
        if len >= SmpMessageQueue::BATCH_SIZE || unsafe { (*engine()).stopped() } {
            // SAFETY: as above.
            unsafe { Self::flush_response_batch(this) };
        }
    }

    /// # Safety
    /// As for [`SmpMessageQueue::respond`] (without the item).
    pub(crate) unsafe fn flush_response_batch(this: *mut Self) {
        // SAFETY: by the caller; `_completed_fifo` is a receiving-side field and `_completed`
        // is shared by `&`.
        unsafe {
            let fifo = &mut (*this)._completed_fifo;
            if !fifo.is_empty() {
                let completed = &(*this)._completed;
                let pushed = completed.base.push_range(fifo);
                if pushed == 0 {
                    return;
                }
                completed.maybe_wakeup();
                fifo.drain(..pushed);
            }
        }
    }

    /// # Safety
    /// As for `SmpMessageQueue::flush_response_batch`.
    pub unsafe fn has_unflushed_responses(this: *mut Self) -> bool {
        // SAFETY: by the caller; a receiving-side field.
        unsafe { !(*this)._completed_fifo.is_empty() }
    }

    /// # Safety
    /// As for [`SmpMessageQueue::flush_response_batch`].
    pub(crate) unsafe fn pure_poll_rx(this: *mut Self) -> bool {
        // Reads the ring's indices directly.
        // empty() is not const, so need const_cast.
        // SAFETY: by the caller; `_pending` is shared by `&`.
        unsafe { !(*this)._pending.base.empty() }
    }

    #[inline]
    fn process_queue<const PREFETCH_CNT: usize, F: FnMut(WorkItemPtr)>(
        q: &LfQueue,
        mut process: F,
    ) -> usize {
        // copy batch to local memory in order to minimize
        // time in which cross-cpu data is accessed
        let mut items: [std::mem::MaybeUninit<WorkItemPtr>; SmpMessageQueue::QUEUE_LENGTH + 16] =
            [const { std::mem::MaybeUninit::uninit() }; SmpMessageQueue::QUEUE_LENGTH + 16];
        const { assert!(PREFETCH_CNT <= 16) };
        let Some(mut wi) = q.base.pop() else {
            return 0;
        };
        // start prefetching first item before popping the rest to overlap memory
        // access with potential cache miss the second pop may cause
        crate::engine::core::prefetch::prefetch::<2>(wi.as_ptr() as *const u8);
        // SAFETY: `MaybeUninit<T>` has the layout of `T`, and pop_into only writes the slots
        // it reports.
        let nr = q.base.pop_into(unsafe {
            std::slice::from_raw_parts_mut(
                items.as_mut_ptr() as *mut WorkItemPtr,
                SmpMessageQueue::QUEUE_LENGTH,
            )
        });
        let fill = if nr != 0 {
            // SAFETY: `nr > 0` items were written by pop_into.
            unsafe { items.get_unchecked(nr - 1).assume_init() }
        } else {
            wi
        };
        // SAFETY: nr <= QUEUE_LENGTH, so [nr, nr + PREFETCH_CNT) is within the 16 spare slots.
        for slot in unsafe { items.get_unchecked_mut(nr..nr + PREFETCH_CNT) } {
            slot.write(fill);
        }
        let mut i = 0usize;
        loop {
            crate::engine::core::prefetch::prefetch_n::<2, _>(
                // SAFETY: slots [0, nr + PREFETCH_CNT) are initialized and `i <= nr`.
                unsafe { items.get_unchecked(i..i + PREFETCH_CNT) }
                    .iter()
                    // SAFETY: as above.
                    .map(|slot| unsafe { slot.assume_init() }.as_ptr() as *const u8),
            );
            process(wi);
            // SAFETY: `i <= nr`, and slot `nr` was filled above.
            wi = unsafe { items.get_unchecked(i).assume_init() };
            i += 1;
            if i > nr {
                break;
            }
        }

        nr + 1
    }

    /// Runs the completions returned by shard `t`, on the sending side; returns how many.
    ///
    /// # Safety
    /// As for [`SmpMessageQueue::stop`].
    pub(crate) unsafe fn process_completions(this: *mut Self, t: ShardId) -> usize {
        // SAFETY: by the caller; `_completed` is shared by `&`.
        let nr = Self::process_queue::<{ SmpMessageQueue::PREFETCH_CNT * 2 }, _>(
            unsafe { &(*this)._completed },
            |wi| {
                // SAFETY: completed items are owned by the sender again; they were leaked by
                // submit_item, and no smp service group is created or destroyed meanwhile.
                unsafe {
                    wi.complete();
                    let ssg_id = crate::engine::core::smp::internal::smp_service_group_id(wi.ssg());
                    crate::engine::core::smp::get_smp_service_groups_semaphore(ssg_id, t).signal(1);
                    wi.destroy();
                }
            },
        );
        // SAFETY: by the caller; `tx_stats` is a sending-side field.
        let tx = unsafe { &mut (*this).tx_stats.0 };
        tx._current_queue_length -= nr;
        tx._compl += nr;
        tx._last_cmpl_batch = nr;

        nr
    }

    /// Publishes the batched requests to the receiving shard (waking it if it sleeps).
    ///
    /// # Safety
    /// As for [`SmpMessageQueue::stop`].
    pub(crate) unsafe fn flush_request_batch(this: *mut Self) {
        // SAFETY: by the caller.
        unsafe {
            if !Self::tx_a(this).pending_fifo.is_empty() {
                Self::move_pending(this);
            }
        }
    }

    /// Runs the requests received from the sending shard, on the receiving side; returns how
    /// many.
    ///
    /// # Safety
    /// As for [`SmpMessageQueue::flush_response_batch`].
    pub(crate) unsafe fn process_incoming(this: *mut Self) -> usize {
        // SAFETY: by the caller; `_pending` is shared by `&`, and its items were received here.
        let nr = Self::process_queue::<{ SmpMessageQueue::PREFETCH_CNT }, _>(
            unsafe { &(*this)._pending },
            |wi| {
                // SAFETY: as above.
                unsafe { crate::engine::core::smp::work_item_process(wi) };
            },
        );
        // SAFETY: by the caller; `rx_stats` is a receiving-side field.
        let rx = unsafe { &mut (*this).rx_stats.0 };
        rx._received += nr;
        rx._last_rcv_batch = nr;
        nr
    }

    /// Starts the queue on the sending side and registers its metrics for shard `cpuid`.
    ///
    /// # Errors
    ///
    /// Returns the error of registering the metrics.
    ///
    /// # Safety
    /// As for [`SmpMessageQueue::stop`]; the queue stays in place until it is stopped (the
    /// metrics read it through its address).
    pub(crate) unsafe fn start(this: *mut Self, cpuid: u32) -> Result<(), ExceptionPtr> {
        // SAFETY: by the caller; `_tx` is a sending-side field, constructed before use.
        unsafe { (*this)._tx = Some(TxSide { pending_fifo: std::collections::VecDeque::new() }) };
        use crate::engine::core::metrics as sm;
        let instance = format!("{}-{}", crate::engine::core::shard_id::this_shard_id(), cpuid);
        let this_addr = this as usize;
        // The metrics read the statistics of both sides by raw place, unsynchronized; the
        // values are plain counters written by one shard each.
        macro_rules! q {
            () => {
                (this_addr as *const SmpMessageQueue)
            };
        }
        // SAFETY (every closure below): the metrics are owned by the queue and cleared in
        // stop(), so the queue is live whenever one is read.
        let metrics = vec![
            // queue_length     value:GAUGE:0:U
            // Absolute value of num packets in last tx batch.
            sm::make_queue_length(
                "send_batch_queue_length",
                // SAFETY: as above, the queue is live whenever the metric is read; a plain read of a
                // counter written by one shard.
                move || unsafe { (*q!()).tx_stats.0._last_snt_batch } as u64,
                sm::description("Current send batch queue length"),
                vec![sm::shard_label().call(&instance)],
            )
            .enabled(sm::METRIC_DISABLED),
            sm::make_queue_length(
                "receive_batch_queue_length",
                // SAFETY: as above, the queue is live whenever the metric is read. Not sound as it
                // stands: `rx_stats` is written by the receiving shard (`process_incoming`) while
                // this metric runs on the sending shard, so this unsynchronized read races with
                // that write (undefined behaviour in the memory model, an aligned word load in
                // practice); the metric is disabled by default. An atomic counter would fix it.
                move || unsafe { (*q!()).rx_stats.0._last_rcv_batch } as u64,
                sm::description("Current receive batch queue length"),
                vec![sm::shard_label().call(&instance)],
            )
            .enabled(sm::METRIC_DISABLED),
            sm::make_queue_length(
                "complete_batch_queue_length",
                // SAFETY: as above, the queue is live whenever the metric is read; a plain read of a
                // counter written by one shard.
                move || unsafe { (*q!()).tx_stats.0._last_cmpl_batch } as u64,
                sm::description("Current complete batch queue length"),
                vec![sm::shard_label().call(&instance)],
            )
            .enabled(sm::METRIC_DISABLED),
            sm::make_queue_length(
                "send_queue_length",
                // SAFETY: as above, the queue is live whenever the metric is read; a plain read of a
                // counter written by one shard.
                move || unsafe { (*q!()).tx_stats.0._current_queue_length } as u64,
                sm::description("Current send queue length"),
                vec![sm::shard_label().call(&instance)],
            )
            .enabled(sm::METRIC_DISABLED),
            // total_operations value:DERIVE:0:U
            sm::make_counter(
                "total_received_messages",
                // SAFETY: as above, the queue is live whenever the metric is read. Not sound as it
                // stands: `rx_stats` is written by the receiving shard (`process_incoming`) while
                // this metric runs on the sending shard, so this unsynchronized read races with
                // that write (undefined behaviour in the memory model, an aligned word load in
                // practice); the metric is disabled by default. An atomic counter would fix it.
                move || unsafe { (*q!()).rx_stats.0._received } as u64,
                sm::description("Total number of received messages"),
                vec![sm::shard_label().call(&instance)],
            )
            .enabled(sm::METRIC_DISABLED),
            // total_operations value:DERIVE:0:U
            sm::make_counter(
                "total_sent_messages",
                // SAFETY: as above, the queue is live whenever the metric is read; a plain read of a
                // counter written by one shard.
                move || unsafe { (*q!()).tx_stats.0._sent } as u64,
                sm::description("Total number of sent messages"),
                vec![sm::shard_label().call(&instance)],
            )
            .enabled(sm::METRIC_DISABLED),
            // total_operations value:DERIVE:0:U
            sm::make_counter(
                "total_completed_messages",
                // SAFETY: as above, the queue is live whenever the metric is read; a plain read of a
                // counter written by one shard.
                move || unsafe { (*q!()).tx_stats.0._compl } as u64,
                sm::description("Total number of messages completed"),
                vec![sm::shard_label().call(&instance)],
            )
            .enabled(sm::METRIC_DISABLED),
        ];
        // SAFETY: by the caller; `_metrics` is a sending-side field.
        unsafe { (*this)._metrics.add_group("smp", metrics) }?;
        Ok(())
    }
}

impl LfQueue {
    /// Wakes the consuming reactor if it sleeps.
    pub(crate) fn maybe_wakeup(&self) {
        // Called after lf_queue_base::push().
        //
        // This is read-after-write, which wants memory_order_seq_cst,
        // but we insert that barrier using systemwide_memory_barrier()
        // because seq_cst is so expensive.
        //
        // However, we do need a compiler barrier:
        std::sync::atomic::compiler_fence(Ordering::SeqCst);
        // SAFETY: the remote reactor outlives the queues between shards.
        unsafe { Reactor::wakeup_from(self.remote.remote) };
    }
}

impl Drop for LfQueue {
    fn drop(&mut self) {
        self.base.consume_all(|ptr: WorkItemPtr| {
            // SAFETY: items left in the queue are owned by it.
            unsafe { ptr.destroy() };
        });
    }
}

// ---------------------------------------------------------------------------
// Eventfds.
// ---------------------------------------------------------------------------

use crate::engine::core::internal::pollable_fd::{ReadableEventfd, WriteableEventfd};

impl WriteableEventfd {
    /// A reactor-driven read side for this eventfd.
    ///
    /// # Errors
    ///
    /// Returns the system error of duplicating the descriptor.
    pub fn read_side(&self) -> Result<ReadableEventfd, ExceptionPtr> {
        Ok(ReadableEventfd::from_file_desc(self._fd.dup()?))
    }

    /// Creates a close-on-exec eventfd with counter `initial`.
    pub(crate) fn try_create_eventfd(initial: usize) -> Result<FileDesc, ExceptionPtr> {
        engine_assert!(initial as i32 as usize == initial);
        FileDesc::eventfd(initial as u32, EFD_CLOEXEC)
    }

    /// Adds `count` to the eventfd counter: a write(2) on the descriptor, which
    /// needs no exclusive access (the syscall queue signals it from a shared reference).
    pub fn signal(&self, count: usize) {
        let c = count as u64;
        // SAFETY: writing 8 bytes from a local to our eventfd.
        let r = unsafe { write(self._fd.get(), &c as *const u64 as *const std::ffi::c_void, 8) };
        engine_assert!(r == 8);
    }
}

impl ReadableEventfd {
    /// A write side for this eventfd.
    ///
    /// # Errors
    ///
    /// Returns the system error of duplicating the descriptor.
    pub fn write_side(&mut self) -> Result<WriteableEventfd, ExceptionPtr> {
        Ok(WriteableEventfd::from_file_desc(self._fd.get_file_desc().dup()?))
    }

    /// Creates a non-blocking, close-on-exec eventfd with counter `initial`.
    pub(crate) fn try_create_eventfd(initial: usize) -> Result<FileDesc, ExceptionPtr> {
        engine_assert!(initial as i32 as usize == initial);
        FileDesc::eventfd(initial as u32, EFD_CLOEXEC | EFD_NONBLOCK)
    }

    /// Resolves with the counter once the eventfd is readable (resetting it).
    pub fn wait(&mut self) -> Future<usize> {
        // The descriptor is read when the wait completes; the eventfd outlives the wait, so
        // it is still open then.
        let fd = self._fd.get_fd();
        // SAFETY: on this shard's reactor thread; the state is live while `self` is, and is
        // passed by its root pointer.
        unsafe { Reactor::readable(self._fd.state_ptr()) }.then(move |()| {
            let mut count: u64 = 0;
            // SAFETY: reads 8 bytes into a local.
            let r = unsafe { read(fd, &mut count as *mut u64 as *mut std::ffi::c_void, 8) };
            engine_assert!(r == 8);
            make_ready_future(count as usize)
        })
    }
}

// ---------------------------------------------------------------------------
// schedule
// ---------------------------------------------------------------------------

#[inline]
/// Queues `t` on the current shard, at the back of its scheduling group's queue.
pub fn schedule(t: TaskPtr) {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).add_task(t) };
}

/// Like [`schedule`], but first rejects (as an internal error) a task of the at-exit
/// scheduling group.
pub fn schedule_checked(t: TaskPtr) {
    // SAFETY: `t` is a live task.
    if unsafe { (*t.as_ptr()).group() }.is_at_exit() {
        // trying to schedule a task in at_destroy. Not allowed
        crate::engine::core::on_internal_error::on_internal_error(
            engine_logger(),
            "Cannot schedule tasks in at_destroy queue. Use reactor::at_destroy.",
        );
    }
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).add_task(t) };
}

#[inline]
/// Queues `t` on the current shard, at the front of its scheduling group's queue.
pub fn schedule_urgent(t: TaskPtr) {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).add_urgent_task(t) };
}

fn kernel_supports_aio_fsync() -> bool {
    crate::engine::core::internal::uname::kernel_uname().whitelisted(&["4.18"])
}

fn wakeup_granularity() -> (std::path::PathBuf, u64) {
    let try_read = |path: &str| -> u64 {
        crate::engine::util::read_first_line::read_first_line_as::<u64>(std::path::Path::new(path))
            .unwrap_or(0)
    };

    let legacy_path = "/proc/sys/kernel/sched_wakeup_granularity_ns";
    let val = try_read(legacy_path);
    if val != 0 {
        return (std::path::PathBuf::from(legacy_path), val);
    }

    // This will in practice almost always fail because debug fs requires root
    // perms to read so we are out of luck
    let debug_fs_path = "/sys/kernel/debug/sched/wakeup_granularity_ns";
    let val = try_read(debug_fs_path);
    if val != 0 {
        return (std::path::PathBuf::from(debug_fs_path), val);
    }

    (std::path::PathBuf::new(), 0)
}

use crate::engine::util::program_options as po;

fn create_network_stacks_option() -> Result<po::SelectionValue<NetworkStackFactory>, ExceptionPtr> {
    let mut candidates: Vec<po::Candidate<NetworkStackFactory>> = Vec::new();
    let mut net_stack_names: Vec<String> = Vec::new();

    let mut default_stack = String::new();
    for reg_func in [
        crate::engine::net::native_stack::register_native_stack
            as fn() -> crate::engine::net::api::NetworkStackEntry,
        crate::engine::net::posix_stack::register_posix_stack,
    ] {
        let s = reg_func();
        let name = s.name.as_str().to_string();
        if s.is_default {
            default_stack = name.clone();
        }
        let opts: std::pin::Pin<Box<dyn po::AsOptionGroup>> = Box::into_pin(s.opts);
        candidates.push(po::Candidate {
            name: name.clone(),
            value: po::ValueHandle::from_box(Box::new(NetworkStackFactory::new(s.factory))),
            opts: Some(opts),
        });
        net_stack_names.push(name);
    }

    po::SelectionValue::new_with_default(
        "network-stack",
        candidates,
        &default_stack,
        format!("select network stack (valid values: {})", net_stack_names.join(", ")),
    )
}

fn backend_selector_candidates() -> Result<Vec<po::Candidate<ReactorBackendSelector>>, ExceptionPtr>
{
    let mut candidates = Vec::new();

    for be in ReactorBackendSelector::available()? {
        let name = be.name().clone();
        candidates.push(po::Candidate {
            name,
            value: po::ValueHandle::from_box(Box::new(be)),
            opts: None,
        });
    }
    Ok(candidates)
}

impl ReactorOptions {
    /// The reactor options with their default values, not yet linked to a group (see
    /// [`ReactorOptions::link`]).
    ///
    /// Probing the network stacks or the reactor backends may fail;
    /// [`ReactorOptions::try_new`] returns that error, while `new()` reports it and aborts.
    pub fn new() -> Self {
        match Self::try_new() {
            Ok(opts) => opts,
            Err(e) => {
                engine_logger().error(
                    crate::engine::util::log::FormatInfo::new("reactor options: {}"),
                    format_args!(
                        "reactor options: {}",
                        crate::engine::util::log::DisplayExceptionPtr(Some(&e))
                    ),
                );
                std::process::abort();
            }
        }
    }

    /// The reactor options with their default values.
    ///
    /// # Errors
    ///
    /// Returns the error of probing the network stacks or the reactor backends.
    pub fn try_new() -> Result<Self, ExceptionPtr> {
        let available = ReactorBackendSelector::available()?;
        let available_names: Vec<String> = available.iter().map(|b| b.name().clone()).collect();
        let default_backend = ReactorBackendSelector::default_backend()?;
        Ok(ReactorOptions {
            group: po::OptionGroup::new("Core options"),
            network_stack: create_network_stacks_option()?,
            poll_mode: po::Switch::new("poll-mode", "poll continuously (100% cpu use)"),
            idle_poll_time_us: po::Value::new(
                "idle-poll-time-us",
                Some(Reactor::calculate_poll_time().as_micros() as u32),
                "idle polling time in microseconds (reduce for overprovisioned environments or laptops)",
            ),
            poll_aio: po::Value::new(
                "poll-aio",
                Some(true),
                "busy-poll for disk I/O (reduces latency and increases throughput)",
            ),
            task_quota_ms: po::Value::new(
                "task-quota-ms",
                Some(0.5),
                "Max time (ms) between polls",
            ),
            io_latency_goal_ms: po::Value::new(
                "io-latency-goal-ms",
                None,
                "Max time (ms) io operations must take (1.5 * task-quota-ms if not set)",
            ),
            io_flow_ratio_threshold: po::Value::new(
                "io-flow-rate-threshold",
                Some(1.1),
                "Dispatch rate to completion rate threshold",
            ),
            io_completion_notify_ms: po::Value::new(
                "io-completion-notify-ms",
                None,
                "Threshold in milliseconds over which IO request completion is reported to logs",
            ),
            max_task_backlog: po::Value::new(
                "max-task-backlog",
                Some(1000),
                "Maximum number of task backlog to allow; above this we ignore I/O",
            ),
            blocked_reactor_notify_ms: po::Value::new(
                "blocked-reactor-notify-ms",
                Some(25),
                "threshold in miliseconds over which the reactor is considered blocked if no progress is made",
            ),
            blocked_reactor_reports_per_minute: po::Value::new(
                "blocked-reactor-reports-per-minute",
                Some(5),
                "Maximum number of backtraces reported by stall detector per minute",
            ),
            blocked_reactor_report_format_oneline: po::Value::new(
                "blocked-reactor-report-format-oneline",
                Some(true),
                "Print a simplified backtrace on a single line",
            ),
            relaxed_dma: po::Switch::new(
                "relaxed-dma",
                "allow using buffered I/O if DMA is not available (reduces performance)",
            ),
            linux_aio_nowait: po::Value::new(
                "linux-aio-nowait",
                Some(aio_nowait_supported()),
                "use the Linux NOWAIT AIO feature, which reduces reactor stalls due to aio (autodetected)",
            ),
            unsafe_bypass_fsync: po::Value::new(
                "unsafe-bypass-fsync",
                Some(false),
                "Bypass fsync(), may result in data loss. Use for testing on consumer drives",
            ),
            kernel_page_cache: po::Value::new(
                "kernel-page-cache",
                Some(false),
                "Use the kernel page cache. This disables DMA (O_DIRECT). Useful for short-lived functional tests with a small data set.",
            ),
            overprovisioned: po::Switch::new(
                "overprovisioned",
                "run in an overprovisioned environment (such as docker or a laptop); equivalent to --idle-poll-time-us 0 --thread-affinity 0 --poll-aio 0",
            ),
            abort_on_engine_bad_alloc: po::Switch::new(
                "abort-on-engine-bad-alloc",
                "abort when engine allocator cannot allocate memory",
            ),
            force_aio_syscalls: po::Value::new(
                "force-aio-syscalls",
                Some(false),
                "Force io_getevents(2) to issue a system call, instead of bypassing the kernel when possible. This makes strace output more useful, but slows down the application",
            ),
            dump_memory_diagnostics_on_alloc_failure_kind: po::Value::new(
                "dump-memory-diagnostics-on-alloc-failure-kind",
                Some(memory::AllocFailureKind::Critical),
                "Dump diagnostics of the engine allocator state on allocation failure. Accepted values: none, critical (default), all. When set to critical, only allocations marked as critical will trigger diagnostics dump. The diagnostics will be written to the engine_memory logger, with error level. Note that if the engine_memory logger is set to debug or trace level, the diagnostics will be logged irrespective of this setting.",
            ),
            reactor_backend: po::SelectionValue::new_with_default(
                "reactor-backend",
                backend_selector_candidates()?,
                default_backend.name(),
                format!("Internal reactor implementation ([{}])", available_names.join(", ")),
            )?,
            aio_fsync: po::Value::new(
                "aio-fsync",
                Some(kernel_supports_aio_fsync()),
                "Use Linux aio for fsync() calls. This reduces latency; requires Linux 4.18 or later.",
            ),
            max_networking_io_control_blocks: po::Value::new(
                "max-networking-io-control-blocks",
                Some(10000),
                "Maximum number of I/O control blocks (IOCBs) to allocate per shard. This bounds the number of sockets supported per shard. Requires tuning /proc/sys/fs/aio-max-nr. Only valid for the linux-aio reactor backend (see --reactor-backend).",
            ),
            reserve_io_control_blocks: po::Value::new(
                "reserve-io-control-blocks",
                Some(0),
                "Reserve this many IOCBs, so it is available to any side application that runs parallel to the engine appliation. Takes precedence over --max-networking-io-control-blocks. Only valid for the linux-aio reactor backend (see --reactor-backend).",
            ),
            #[cfg(engine_heapprof)]
            heapprof: po::Value::new(
                "heapprof",
                Some(0),
                "Enable engine heap profiling. Sample every ARG bytes. 0 means off",
            ),
            #[cfg(not(engine_heapprof))]
            heapprof: po::Value::new_unused("heapprof", po::Unused),
            no_handle_interrupt: po::Switch::new("no-handle-interrupt", "ignore SIGINT (for gdb)"),
            _argv0: String::new(),
            _auto_handle_sigint_sigterm: true,
        })
    }
}

impl Default for ReactorOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// The owner of this thread's reactor: destroys it and frees its cache-aligned storage.
pub struct ReactorHolder(*mut Reactor);

impl Drop for ReactorHolder {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the reactor was constructed in place in storage from posix_memalign, and
            // no reference to it is live.
            unsafe {
                Reactor::destroy(self.0);
                free(self.0 as *mut std::ffi::c_void);
            }
        }
    }
}

unsafe extern "C" {
    fn posix_memalign(memptr: *mut *mut std::ffi::c_void, alignment: usize, size: usize) -> i32;
    fn free(p: *mut std::ffi::c_void);
}

thread_local! {
    /// This thread's reactor owner.
    pub static REACTOR_HOLDER: RefCell<ReactorHolder> = const { RefCell::new(ReactorHolder(std::ptr::null_mut())) };
}

// The smp object is shared (`Arc<Smp>`) by the application and every reactor thread; the
// members written after it is shared are cells with one writer at a time (see `Smp` in
// smp.rs).
impl Smp {
    /// `_qs[t][from]`: the queue that carries the requests of shard `from` to shard `t`.
    ///
    /// # Safety
    /// The queue matrix is installed on this thread (`_qs`, see `set_qs_from_owner`), and both
    /// ids are below `smp::count`.
    #[inline(always)]
    pub(crate) unsafe fn queue(t: u32, from: u32) -> *mut SmpMessageQueue {
        // SAFETY: guaranteed by the caller.
        unsafe {
            (*crate::engine::core::smp::SMP_QS.with(|q| q.get()).add(t as usize)).add(from as usize)
        }
    }

    /// Whether the queue matrix is installed on this thread.
    #[inline]
    fn has_queues() -> bool {
        !crate::engine::core::smp::SMP_QS.with(|q| q.get()).is_null()
    }

    /// Installs the queue matrix built by shard 0 on this thread (after the queues-constructed
    /// barrier on the other threads).
    fn set_qs_from_owner(&self) {
        // SAFETY: `_qs_owner` is written only by allocate_qs_owner, on the main thread before
        // the barrier that orders this read, and by the destructor.
        let qs = unsafe { (*self._qs_owner.get()).as_ref() }
            .map_or(std::ptr::null_mut(), |o| o.as_ptr() as *mut *mut SmpMessageQueue);
        crate::engine::core::smp::SMP_QS.with(|q| q.set(qs));
    }

    /// Allocates the queue matrix: `count` rows of `count` queues, allocated with the
    /// alignment of [`SmpMessageQueue`] (it has fields with large alignment) and constructed
    /// in place with `make(i, j)`.
    ///
    /// Called by configure() on the main thread while the other reactor threads wait on
    /// `smp_queues_constructed`.
    fn allocate_qs_owner(&self, count: u32, mut make: impl FnMut(u32, u32) -> SmpMessageQueue) {
        let layout = std::alloc::Layout::array::<SmpMessageQueue>(count as usize)
            .unwrap_or_else(|_| std::process::abort());
        let mut rows: Vec<*mut SmpMessageQueue> = Vec::with_capacity(count as usize);
        for i in 0..count {
            // SAFETY: the layout has a non-zero size (`count >= 1` queues).
            let row = unsafe { std::alloc::alloc(layout) } as *mut SmpMessageQueue;
            if row.is_null() {
                // `operator new[]` throws bad_alloc; Rust aborts on allocation failure.
                std::alloc::handle_alloc_error(layout);
            }
            for j in 0..count {
                // SAFETY: slot `j` of the fresh row, in bounds and uninitialized.
                unsafe { row.add(j as usize).write(make(i, j)) };
            }
            rows.push(row);
        }
        // SAFETY: the only writer, on the main thread, while no other thread reads it (see
        // above).
        unsafe { *self._qs_owner.get() = Some(rows.into_boxed_slice()) };
    }

    /// Records the main thread's id.
    fn set_tmain(id: std::ffi::c_ulong) {
        crate::engine::core::smp::SMP_TMAIN.with(|t| t.set(Some(id)));
    }

    /// The main thread's id, once recorded.
    fn tmain() -> Option<std::ffi::c_ulong> {
        crate::engine::core::smp::SMP_TMAIN.with(|t| t.get())
    }

    /// Sets the number of shards.
    fn set_count(n: u32) {
        crate::engine::core::smp::SMP_COUNT.store(n, Ordering::Relaxed);
    }

    /// Starts the queues from this shard to the others: the queue matrix is installed on this
    /// thread.
    pub(crate) fn start_all_queues(&self) -> Result<(), ExceptionPtr> {
        let me = crate::engine::core::shard_id::this_shard_id();
        for c in 0..Smp::count() {
            if c != me {
                // SAFETY: the matrix is installed (callers run after set_qs_from_owner);
                // `_qs[c][me]` is this shard's sending side, which only this shard touches.
                unsafe { SmpMessageQueue::start(Smp::queue(c, me), c)? };
            }
        }
        // SAFETY: the alien instance outlives the smp object and is shared by `&`.
        unsafe { (*self._alien).queue(me).start() }
    }

    /// Joins the reactor threads: called on shard 0 once every event loop has ended.
    pub fn join_all(&self) {
        #[cfg(feature = "dpdk")]
        if self._using_dpdk.get() {
            unsafe extern "C" {
                fn rte_eal_mp_wait_lcore();
            }
            // SAFETY: DPDK's EAL is initialised when `_using_dpdk` is set.
            unsafe { rte_eal_mp_wait_lcore() };
            return;
        }
        // SAFETY: only shard 0 uses `_threads` at this point (the other threads have left
        // their event loop and do not touch it).
        let threads = unsafe { &mut *self._threads.get() };
        for t in threads.drain(..) {
            t.join();
        }
    }

    /// Pins the calling thread to `cpu_id` (unless DPDK does its own pinning).
    pub fn pin(&self, cpu_id: u32) {
        if self._using_dpdk.get() {
            // dpdk does its own pinning
            return;
        }
        crate::engine::core::posix::pin_this_thread(cpu_id);
    }

    /// Waits at the barrier of the threads whose event loop ended.
    pub fn arrive_at_event_loop_end(&self) {
        // SAFETY: set by configure() before the reactor threads start, then only read.
        if let Some(b) = unsafe { &*self._all_event_loops_done.get() } {
            b.wait();
        }
    }

    /// Allocates and constructs the reactor of shard `id` on this thread.
    pub fn allocate_reactor(
        self: &Arc<Self>,
        id: u32,
        rbs: ReactorBackendSelector,
        cfg: ReactorConfig,
    ) -> Result<(), ExceptionPtr> {
        REACTOR_HOLDER.with(|h| engine_assert!(h.borrow().0.is_null()));

        // we cannot just write "local_engin = new reactor" since reactor's constructor
        // uses local_engine
        let mut buf: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `buf` receives cache-line aligned storage for one reactor.
        let r =
            unsafe { posix_memalign(&mut buf, CACHE_LINE_SIZE, std::mem::size_of::<Reactor>()) };
        engine_assert!(r == 0);
        // SAFETY: the pointer refers to the thread-local shard id of this thread.
        unsafe { *crate::engine::core::shard_id::internal::this_shard_id_ptr() = id };
        let p = buf as *mut Reactor;
        if let Err(e) =
            // SAFETY: `p` is suitably aligned uninitialized storage; Reactor::construct_in_place
            // initializes it fully before returning Ok.
            unsafe {
                Reactor::construct_in_place(p, self.clone(), self._alien, id, rbs, cfg)
            }
        {
            // SAFETY: freeing the storage allocated above, which holds no reactor.
            unsafe { free(buf) };
            return Err(e);
        }
        // The engine is published once its constructor has finished, so engine_is_ready()
        // is false while it runs.
        set_local_engine(p);
        REACTOR_HOLDER.with(|h| h.borrow_mut().0 = p);
        Ok(())
    }

    /// Releases the shared state: called once every reactor thread has been joined.
    pub fn cleanup(&self) {
        // SAFETY: no reactor thread runs any more, so nothing else uses these members.
        unsafe {
            *self._threads.get() = Vec::new();
            (*self._thread_loops.get()).clear();
            *self._shard_to_numa_node_mapping.get() = Vec::new();
        }
        REACTOR_HOLDER.with(|h| {
            let old = std::mem::replace(&mut *h.borrow_mut(), ReactorHolder(std::ptr::null_mut()));
            drop(old);
        });
        set_local_engine(std::ptr::null_mut());
    }

    /// Stops this shard's sending queues (called by each shard as it stops).
    pub fn cleanup_cpu(&self) {
        let cpuid = crate::engine::core::shard_id::this_shard_id();

        if Smp::has_queues() {
            for i in 0..Smp::count() {
                // SAFETY: the matrix is installed on this thread; `_qs[i][cpuid]` is this
                // shard's sending side.
                unsafe { SmpMessageQueue::stop(Smp::queue(i, cpuid)) };
            }
        }
        // SAFETY: the alien instance outlives the smp object and is shared by `&`.
        let alien = unsafe { &*self._alien };
        if alien.has_queues() {
            alien.queue(cpuid).stop();
        }
    }

    /// Starts a reactor thread running `thread_loop`: called by configure()
    /// on the main thread; the reactor threads it has started do not use `_threads` or
    /// `_thread_loops`.
    pub(crate) fn create_thread(
        &self,
        thread_loop: Box<dyn FnOnce() + Send>,
    ) -> Result<(), ExceptionPtr> {
        if self._using_dpdk.get() {
            // The loop runs once.
            let once = std::cell::Cell::new(Some(thread_loop));
            // SAFETY: the only user of `_thread_loops` now (see above).
            unsafe { &mut *self._thread_loops.get() }.push(Box::new(move || {
                if let Some(f) = once.take() {
                    f();
                }
            }));
        } else {
            let t = PosixThread::new(thread_loop)?;
            // SAFETY: the only user of `_threads` now (see above).
            unsafe { &mut *self._threads.get() }.push(t);
        }
        Ok(())
    }
}

impl Drop for Smp {
    /// Destroys the queue matrix: every queue is destroyed in place and each row freed with
    /// the alignment of [`SmpMessageQueue`].
    fn drop(&mut self) {
        let Some(qs) = self._qs_owner.get_mut().take() else { return };
        let count = qs.len();
        let layout = std::alloc::Layout::array::<SmpMessageQueue>(count)
            .unwrap_or_else(|_| std::process::abort());
        for &row in qs.iter() {
            // SAFETY: each row was allocated by allocate_qs_owner with this layout and holds
            // `count` constructed queues, which nothing uses any more.
            unsafe {
                for j in 0..count {
                    std::ptr::drop_in_place(row.add(j));
                }
                std::alloc::dealloc(row as *mut u8, layout);
            }
        }
    }
}

/// State of the one-shot signal handlers: one `handled` flag and one function per signal
/// number, behind one spinlock.
struct OneshotSignalState {
    lock: crate::engine::util::spinlock::util::Spinlock,
    handled: [AtomicBool; 65],
    funcs: [AtomicUsize; 65],
}

static ONESHOT_SIGNALS: OneshotSignalState = OneshotSignalState {
    lock: crate::engine::util::spinlock::util::Spinlock::new(),
    handled: [const { AtomicBool::new(false) }; 65],
    funcs: [const { AtomicUsize::new(0) }; 65],
};

unsafe extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
    fn pthread_kill(thread: usize, sig: i32) -> i32;
}

const SIG_DFL: usize = 0;
const SIGSEGV: i32 = 11;
const SIGABRT: i32 = 6;
const SIGQUIT: i32 = 3;
const SIGILL: i32 = 4;
const SIGFPE: i32 = 8;
const SIGALRM: i32 = 14;
const SIGCONT: i32 = 18;
const SIGSTOP: i32 = 19;
const SIGTSTP: i32 = 20;
const SIGTTIN: i32 = 21;
const SIGTTOU: i32 = 22;

/// A one-shot signal handler function: `(siginfo, ucontext)`.
type OneshotFunc = fn(*mut SiginfoT, *mut std::ffi::c_void);

extern "C" fn oneshot_trampoline(sig: i32, info: *mut SiginfoT, p: *mut std::ffi::c_void) {
    let idx = sig as usize;
    let _g = ONESHOT_SIGNALS.lock.lock_guard();
    if !ONESHOT_SIGNALS.handled[idx].load(Ordering::Relaxed) {
        ONESHOT_SIGNALS.handled[idx].store(true, Ordering::Relaxed);
        // SAFETY: restoring the default disposition.
        unsafe { signal(sig, SIG_DFL) };
        let f = ONESHOT_SIGNALS.funcs[idx].load(Ordering::Relaxed);
        if f != 0 {
            // SAFETY: `funcs[idx]` holds a `OneshotFunc` stored by install_oneshot_signal_handler.
            let f: OneshotFunc = unsafe { std::mem::transmute::<usize, OneshotFunc>(f) };
            f(info, p);
        }
    }
}

// Installs handler for Signal which ensures that Func is invoked only once
// in the whole program and that after it is invoked the default handler is restored.
fn install_oneshot_signal_handler(signal_no: i32, func: OneshotFunc) -> Result<(), ExceptionPtr> {
    ONESHOT_SIGNALS.funcs[signal_no as usize].store(func as usize, Ordering::Relaxed);
    let mut sa = Sigaction {
        sa_sigaction: oneshot_trampoline as *const () as usize,
        ..Sigaction::default()
    };
    // SAFETY: valid signal set.
    unsafe { sigfillset(&mut sa.sa_mask) };
    sa.sa_flags = SA_SIGINFO | SA_RESTART;
    if signal_no == SIGSEGV {
        sa.sa_flags |= SA_ONSTACK;
    }
    // SAFETY: `sa` is a valid sigaction.
    let r = unsafe { sigaction(signal_no, &sa, std::ptr::null_mut()) };
    throw_system_error_on(r == -1, "sigaction")
}

fn reraise_signal(signo: i32) {
    // SAFETY: plain libc calls.
    unsafe {
        signal(signo, SIG_DFL);
        pthread_kill(pthread_self(), signo);
    }
}

/// `siginfo_t` fields for SIGSEGV (si_pid and si_addr share the union after si_code).
#[repr(C)]
#[cfg(not(engine_asan_enabled))]
struct SiginfoFault {
    si_signo: i32,
    si_errno: i32,
    si_code: i32,
    _pad0: i32,
    si_addr_or_pid: u64,
}

#[cfg(not(engine_asan_enabled))]
const SI_USER: i32 = 0;
#[cfg(not(engine_asan_enabled))]
const SEGV_MAPERR: i32 = 1;
#[cfg(not(engine_asan_enabled))]
const SEGV_ACCERR: i32 = 2;
#[cfg(not(engine_asan_enabled))]
const SEGV_BNDERR: i32 = 3;
#[cfg(target_arch = "x86_64")]
#[cfg(not(engine_asan_enabled))]
const UC_RIP_OFFSET: usize = 40 + 16 * 8; // offsetof(ucontext_t, uc_mcontext.gregs[REG_RIP])
#[cfg(target_arch = "aarch64")]
#[cfg(not(engine_asan_enabled))]
const UC_PC_OFFSET: usize = 176 + 8 * 32 + 8; // offsetof(ucontext_t, uc_mcontext.pc)

#[cfg(not(engine_asan_enabled))]
fn sigsegv_action(info: *mut SiginfoT, uc: *mut std::ffi::c_void) {
    use crate::engine::util::print_safe::{
        print_decimal_safe, print_safe, print_zero_padded_hex_safe,
    };
    // SAFETY: the kernel passes a valid siginfo_t to the handler.
    let fi = unsafe { &*(info as *const SiginfoFault) };
    print_safe(b"Segmentation fault: si_code: ");
    let code = fi.si_code;
    print_decimal_safe(fi.si_code as u32 as u64);

    if code == SI_USER {
        print_safe(b", si_pid: ");
        // print the pid in the case the signal was sent by someone else
        print_decimal_safe(fi.si_addr_or_pid as u32 as u64);
    } else if code == SEGV_MAPERR || code == SEGV_ACCERR || code == SEGV_BNDERR {
        // print the address of the data access
        print_safe(b", si_addr: ");
        print_zero_padded_hex_safe(fi.si_addr_or_pid);
    }

    let ip: u64 = if !uc.is_null() {
        #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: `uc` is the ucontext_t the kernel passed to the handler.
            unsafe { *((uc as *const u8).add(UC_RIP_OFFSET) as *const u64) }
        }
        #[cfg(target_arch = "aarch64")]
        {
            // SAFETY: as above.
            unsafe { *((uc as *const u8).add(UC_PC_OFFSET) as *const u64) }
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            0xBAD
        }
    } else {
        0xBAD2
    };
    print_safe(b", ip: ");
    print_zero_padded_hex_safe(ip);
    print_safe(b"\n");

    // Print the resolved IP, i.e., suitable for use
    // with addr2line and other tools which expect an
    // address without any added offset (from ASLR or
    // because it's a relocated shared object).
    print_safe(b"Segmentation fault: resolved ip: 0x");
    let f = crate::engine::util::backtrace::decorate(ip as usize);
    print_zero_padded_hex_safe(f.addr as u64);
    print_safe(b" in ");
    let so = f.so;
    print_safe(so.name.as_bytes());
    print_safe(b"[0x");
    print_zero_padded_hex_safe(so.begin as u64);
    print_safe(b"+0x");
    print_zero_padded_hex_safe((so.end - so.begin) as u64);
    print_safe(b"]\n");

    // print the backtrace in immediate mode, so if we crash
    // during the backtrace we get as much output as possible
    print_with_backtrace("Segmentation fault", false, true);
    reraise_signal(SIGSEGV);
}

fn sigabrt_action(_info: *mut SiginfoT, _uc: *mut std::ffi::c_void) {
    print_with_backtrace("Aborting", false, false);
    reraise_signal(SIGABRT);
}

// We don't need to handle SIGSEGV when asan is enabled.
// (With `engine_asan_enabled`, install_oneshot_signal_handler<SIGSEGV, sigsegv_action> does nothing.)

/// The I/O configuration of the disks, from the I/O options or the I/O properties.
pub struct DiskConfigParams {
    _max_queues: u32,
    _num_io_groups: u32,
    _disks: HashMap<u32, DiskParams>,
    _latency_goal: Duration,
    _stall_threshold: Duration,
    _flow_ratio_backpressure_threshold: f64,
}

impl DiskConfigParams {
    /// An empty configuration for up to `max_queues` I/O queues.
    pub fn new(max_queues: u32) -> Self {
        DiskConfigParams {
            _max_queues: max_queues,
            _num_io_groups: 0,
            _disks: HashMap::new(),
            _latency_goal: Duration::ZERO,
            _stall_threshold: Duration::ZERO,
            _flow_ratio_backpressure_threshold: 0.0,
        }
    }

    #[inline]
    /// `qty` divided among `nr_groups` I/O groups.
    pub fn per_io_group(&self, qty: u64, nr_groups: u32) -> u64 {
        (qty / nr_groups as u64).max(1)
    }

    /// Number of I/O groups.
    pub fn num_io_groups(&self) -> u32 {
        self._num_io_groups
    }

    /// The I/O latency goal.
    pub fn latency_goal(&self) -> Duration {
        self._latency_goal
    }

    /// The I/O stall threshold.
    pub fn stall_threshold(&self) -> Duration {
        self._stall_threshold
    }

    /// The latency goal in milliseconds from the options (1.5 times the task quota by
    /// default).
    pub fn latency_goal_opt(&self, opts: &ReactorOptions) -> f64 {
        if opts.io_latency_goal_ms.as_bool() {
            *opts.io_latency_goal_ms.get_value()
        } else {
            *opts.task_quota_ms.get_value() * 1.5
        }
    }

    /// Reads the disk configuration from the I/O options or the I/O properties file.
    ///
    /// # Errors
    ///
    /// Returns an error for an unreadable or invalid configuration.
    pub fn parse_config(
        &mut self,
        smp_opts: &crate::engine::core::smp_options::SmpOptions,
        reactor_opts: &ReactorOptions,
    ) -> Result<(), ExceptionPtr> {
        engine_logger().debug(
            crate::engine::util::log::FormatInfo::new("smp::count: {}"),
            format_args!("smp::count: {}", Smp::count()),
        );
        self._latency_goal = duration_from_user_secs(
            self.latency_goal_opt(reactor_opts) / 1000.0,
            "io-latency-goal-ms",
        )?;
        engine_logger().debug(
            crate::engine::util::log::FormatInfo::new("latency_goal: {}"),
            format_args!("latency_goal: {}", self.latency_goal().as_secs_f64()),
        );
        self._flow_ratio_backpressure_threshold = *reactor_opts.io_flow_ratio_threshold.get_value();
        engine_logger().debug(
            crate::engine::util::log::FormatInfo::new("flow-ratio threshold: {}"),
            format_args!("flow-ratio threshold: {}", self._flow_ratio_backpressure_threshold),
        );
        self._stall_threshold = if reactor_opts.io_completion_notify_ms.defaulted() {
            Duration::MAX
        } else {
            Duration::from_millis(u64::from(*reactor_opts.io_completion_notify_ms.get_value()))
        };

        if smp_opts.num_io_groups.as_bool() {
            self._num_io_groups = *smp_opts.num_io_groups.get_value();
            if self._num_io_groups == 0 {
                return Err(ExceptionPtr::new(crate::engine::core::future::RuntimeError::new(
                    "num-io-groups must be greater than zero".to_string(),
                )));
            }
        }
        if smp_opts.io_properties_file.as_bool() && smp_opts.io_properties.as_bool() {
            return Err(ExceptionPtr::new(crate::engine::core::future::RuntimeError::new(
                "Both io-properties and io-properties-file specified. Don't know which to trust!"
                    .to_string(),
            )));
        }

        let doc: Option<yaml::Node> = if smp_opts.io_properties_file.as_bool() {
            Some(yaml::load_file(smp_opts.io_properties_file.get_value())?)
        } else if smp_opts.io_properties.as_bool() {
            Some(yaml::load(smp_opts.io_properties.get_value())?)
        } else {
            None
        };

        if let Some(doc) = doc {
            let yaml::Node::Map(sections) = &doc else {
                return Err(ExceptionPtr::new(crate::engine::core::future::RuntimeError::new("Bogus io-properties (did you mix up --io-properties and --io-properties-file?)".to_string())));
            };
            for (sec_name, section) in sections {
                if sec_name != "disks" {
                    return Err(ExceptionPtr::new(crate::engine::core::future::RuntimeError::new(
                        format!(
                            "While parsing I/O options: section {} currently unsupported.",
                            sec_name
                        ),
                    )));
                }
                let mut disks: Vec<DiskParams> = Vec::new();
                for n in section.as_sequence()? {
                    let mut d = DiskParams::default();
                    decode_disk_params(n, &mut d)?;
                    disks.push(d);
                }
                let mut devices: std::collections::HashSet<u64> = std::collections::HashSet::new();
                // Queue ids start at 1 and are incremented once per disk.
                for (queue_id_gen, d) in (1_u32..).zip(disks.iter_mut()) {
                    for mp in d.mountpoints.clone() {
                        // SAFETY: `Stat` is plain data.
                        let mut buf: Stat = unsafe { std::mem::zeroed() };
                        let c = cstr(&mp);
                        // SAFETY: NUL-terminated path and valid buffer.
                        let ret = unsafe { stat(c.as_ptr(), &mut buf) };
                        if ret < 0 {
                            return Err(ExceptionPtr::new(
                                crate::engine::core::future::RuntimeError::new(format!(
                                    "Couldn't stat {}",
                                    mp
                                )),
                            ));
                        }

                        let st_dev =
                            if buf.st_mode & S_IFMT == S_IFBLK { buf.st_rdev } else { buf.st_dev };
                        d.devices.push(st_dev);
                        let inserted = devices.insert(st_dev);
                        if !inserted {
                            return Err(ExceptionPtr::new(
                                crate::engine::core::future::RuntimeError::new(format!(
                                    "Mountpoint {}, device {} already configured",
                                    mp, st_dev
                                )),
                            ));
                        }
                    }

                    if self._disks.len() >= self._max_queues as usize {
                        return Err(ExceptionPtr::new(
                            crate::engine::core::future::RuntimeError::new(format!(
                                "Configured number of queues {} is larger than the maximum {}",
                                self._disks.len(),
                                self._max_queues
                            )),
                        ));
                    }

                    d.read_bytes_rate = (d.read_bytes_rate as f32 * d.rate_factor) as u64; // computed in f32
                    d.write_bytes_rate = (d.write_bytes_rate as f32 * d.rate_factor) as u64; // computed in f32
                    d.read_req_rate = (d.read_req_rate as f32 * d.rate_factor) as u64; // computed in f32
                    d.write_req_rate = (d.write_req_rate as f32 * d.rate_factor) as u64; // computed in f32

                    if d.read_bytes_rate == 0
                        || d.write_bytes_rate == 0
                        || d.read_req_rate == 0
                        || d.write_req_rate == 0
                    {
                        return Err(ExceptionPtr::new(
                            crate::engine::core::future::RuntimeError::new(
                                "R/W bytes and req rates must not be zero".to_string(),
                            ),
                        ));
                    }

                    let q = queue_id_gen;
                    engine_logger().debug(
                        crate::engine::util::log::FormatInfo::new(
                            "queue-id: {} mountpoints: {:?} devices: {:?}",
                        ),
                        format_args!(
                            "queue-id: {} mountpoints: {:?} devices: {:?}",
                            q, d.mountpoints, d.devices
                        ),
                    );
                    self._disks.insert(q, d.clone());
                }
            }
        }

        // Placeholder for unconfigured disks.
        let mut d = DiskParams::default();
        d.devices.push(0);
        self._disks.entry(0).or_insert(d);
        Ok(())
    }

    /// The configuration of I/O queue `q` divided among `nr_groups` groups.
    pub fn generate_config(
        &self,
        q: u32,
        nr_groups: u32,
    ) -> crate::engine::core::io_queue::IoQueueConfig {
        let p = &self._disks[&q];
        engine_logger().debug(
            crate::engine::util::log::FormatInfo::new("generate_config queue-id: {}"),
            format_args!("generate_config queue-id: {}", q),
        );
        let mut cfg = crate::engine::core::io_queue::IoQueueConfig { id: q, ..Default::default() };

        if p.read_bytes_rate != u64::MAX {
            cfg.blocks_count_rate = (IoQueue::READ_REQUEST_BASE_COUNT as u64
                * self.per_io_group(p.read_bytes_rate, nr_groups))
                >> IoQueue::BLOCK_SIZE_SHIFT;
            cfg.disk_blocks_write_to_read_multiplier = ((IoQueue::READ_REQUEST_BASE_COUNT as u64
                * p.read_bytes_rate)
                / p.write_bytes_rate) as u32;
        }
        if p.read_req_rate != u64::MAX {
            cfg.req_count_rate = IoQueue::READ_REQUEST_BASE_COUNT as u64
                * self.per_io_group(p.read_req_rate, nr_groups);
            cfg.disk_req_write_to_read_multiplier = ((IoQueue::READ_REQUEST_BASE_COUNT as u64
                * p.read_req_rate)
                / p.write_req_rate) as u32;
        }
        if p.read_saturation_length != u64::MAX {
            cfg.disk_read_saturation_length = p.read_saturation_length;
        }
        if p.write_saturation_length != u64::MAX {
            cfg.disk_write_saturation_length = p.write_saturation_length;
        }
        cfg.mountpoint = p.mountpoints.join(":");
        cfg.duplex = p.duplex;
        cfg.rate_limit_duration = self.latency_goal();
        cfg.flow_ratio_backpressure_threshold = self._flow_ratio_backpressure_threshold;
        // Block count limit should not be less than the minimal IO size on the device
        // On the other hand, even this is not good enough -- in the worst case the
        // scheduler will self-tune to allow for the single 64k request, while it would
        // be better to sacrifice some IO latency, but allow for larger concurrency
        cfg.block_count_limit_min = (64 << 10) >> IoQueue::BLOCK_SIZE_SHIFT;
        cfg.stall_threshold = self.stall_threshold();

        cfg
    }

    /// The ids of the configured I/O queues.
    pub fn queue_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self._disks.keys().copied()
    }

    /// The devices served by I/O queue `q`.
    pub fn queue_devices(&self, q: u32) -> &Vec<u64> {
        &self._disks[&q].devices
    }
}

impl Smp {
    /// Logs the linux-aio control block budget at `level`.
    pub(crate) fn log_aiocbs(
        level: crate::engine::util::log::LogLevel,
        storage: u32,
        preempt: u32,
        network: u32,
        reserve: u32,
    ) {
        // Each cell in the table should be
        // - as wide as the grand total,
        // - as wide as its containing column's header,
        // whichever is wider.
        let percpu_hdr = "per cpu".to_string();
        let allcpus_hdr = format!("all {} cpus", Smp::count());
        let percpu_total = storage + preempt + network;
        let allcpus_total = reserve + percpu_total * Smp::count();
        let num_width = format!("{}", allcpus_total).len();
        let pw = num_width.max(percpu_hdr.len());
        let aw = num_width.max(allcpus_hdr.len());
        let n = Smp::count();
        let l = engine_logger();

        l.log(
            level,
            crate::engine::util::log::FormatInfo::new("purpose  {:pw$}  {:aw$}"),
            format_args!("purpose  {:pw$}  {:aw$}", percpu_hdr, allcpus_hdr),
        );
        l.log(
            level,
            crate::engine::util::log::FormatInfo::new("-------  {:-<pw$}  {:-<aw$}"),
            format_args!("-------  {:-<pw$}  {:-<aw$}", "", ""),
        );
        l.log(
            level,
            crate::engine::util::log::FormatInfo::new("reserve  {:pw$}  {:aw$}"),
            format_args!("reserve  {:pw$}  {:aw$}", "", reserve),
        );
        l.log(
            level,
            crate::engine::util::log::FormatInfo::new("storage  {:pw$}  {:aw$}"),
            format_args!("storage  {:pw$}  {:aw$}", storage, storage * n),
        );
        l.log(
            level,
            crate::engine::util::log::FormatInfo::new("preempt  {:pw$}  {:aw$}"),
            format_args!("preempt  {:pw$}  {:aw$}", preempt, preempt * n),
        );
        l.log(
            level,
            crate::engine::util::log::FormatInfo::new("network  {:pw$}  {:aw$}"),
            format_args!("network  {:pw$}  {:aw$}", network, network * n),
        );
        l.log(
            level,
            crate::engine::util::log::FormatInfo::new("-------  {:-<pw$}  {:-<aw$}"),
            format_args!("-------  {:-<pw$}  {:-<aw$}", "", ""),
        );
        l.log(
            level,
            crate::engine::util::log::FormatInfo::new("total    {:pw$}  {:aw$}"),
            format_args!("total    {:pw$}  {:aw$}", percpu_total, allcpus_total),
        );
    }

    /// Reduces the networking control blocks so the total fits the system's limit minus
    /// `reserve_iocbs`; returns the adjusted count.
    ///
    /// # Errors
    ///
    /// Returns an error if not even the minimum fits.
    pub(crate) fn adjust_max_networking_aio_io_control_blocks(
        mut network_iocbs: u32,
        reserve_iocbs: u32,
    ) -> Result<u32, ExceptionPtr> {
        use crate::engine::util::log::LogLevel;
        const STORAGE_IOCBS: u32 = Reactor::MAX_AIO;
        const PREEMPT_IOCBS: u32 = 2;

        let aio_max_nr = crate::engine::util::read_first_line::read_first_line_as::<u32>(
            std::path::Path::new("/proc/sys/fs/aio-max-nr"),
        )?;
        let aio_nr = crate::engine::util::read_first_line::read_first_line_as::<u32>(
            std::path::Path::new("/proc/sys/fs/aio-nr"),
        )?;
        let available_aio = aio_max_nr.wrapping_sub(aio_nr);
        let requested_aio_network = network_iocbs * Smp::count();
        let requested_aio_other = reserve_iocbs + (STORAGE_IOCBS + PREEMPT_IOCBS) * Smp::count();
        let requested_aio = requested_aio_network + requested_aio_other;
        let l = engine_logger();

        l.debug(
            crate::engine::util::log::FormatInfo::new("Intended AIO control block usage:"),
            format_args!("Intended AIO control block usage:"),
        );
        l.debug(crate::engine::util::log::FormatInfo::new(""), format_args!(""));
        Self::log_aiocbs(
            LogLevel::Debug,
            STORAGE_IOCBS,
            PREEMPT_IOCBS,
            network_iocbs,
            reserve_iocbs,
        );
        l.debug(crate::engine::util::log::FormatInfo::new(""), format_args!(""));
        l.debug(
            crate::engine::util::log::FormatInfo::new(
                "Available AIO control blocks = aio-max-nr - aio-nr = {} - {} = {}",
            ),
            format_args!(
                "Available AIO control blocks = aio-max-nr - aio-nr = {} - {} = {}",
                aio_max_nr, aio_nr, available_aio
            ),
        );

        if available_aio < requested_aio {
            if available_aio >= requested_aio_other + Smp::count() {
                // at least one queue for each shard
                network_iocbs = (available_aio - requested_aio_other) / Smp::count();
                l.warn(crate::engine::util::log::FormatInfo::new("Your system does not have enough AIO capacity for optimal network performance; reducing `max-networking-io-control-blocks'."), format_args!("Your system does not have enough AIO capacity for optimal network performance; reducing `max-networking-io-control-blocks'."));
                l.warn(
                    crate::engine::util::log::FormatInfo::new("Resultant AIO control block usage:"),
                    format_args!("Resultant AIO control block usage:"),
                );
                l.warn(crate::engine::util::log::FormatInfo::new(""), format_args!(""));
                Self::log_aiocbs(
                    LogLevel::Warn,
                    STORAGE_IOCBS,
                    PREEMPT_IOCBS,
                    network_iocbs,
                    reserve_iocbs,
                );
                l.warn(crate::engine::util::log::FormatInfo::new(""), format_args!(""));
                l.warn(crate::engine::util::log::FormatInfo::new("For optimal network performance, set /proc/sys/fs/aio-max-nr to at least {}."), format_args!("For optimal network performance, set /proc/sys/fs/aio-max-nr to at least {}.", aio_nr + requested_aio));
            } else {
                return Err(ExceptionPtr::new(crate::engine::core::future::RuntimeError::new(
                    format!(
                        "Your system does not satisfy minimum AIO requirements. Set /proc/sys/fs/aio-max-nr to at least {} (minimum) or {} (recommended for networking performance).",
                        aio_nr + (requested_aio_other + Smp::count()),
                        aio_nr + requested_aio
                    ),
                )));
            }
        }

        Ok(network_iocbs)
    }
}

/// The shared block of an I/O queue: the reference counter and the queue in one
/// allocation. The queue is constructed in place (its timer callback,
/// its classes and its requests point back at it).
#[repr(C)]
struct IoQueueSharedCount {
    base: crate::engine::core::shared_ptr::SharedPtrCountBase,
    queue: IoQueue,
}

impl IoQueueSharedCount {
    unsafe fn destroy(b: *mut crate::engine::core::shared_ptr::SharedPtrCountBase) {
        // SAFETY: `b` is the first field of a boxed IoQueueSharedCount (repr(C)) with its
        // queue constructed; the last reference is gone.
        unsafe { drop(Box::from_raw(b as *mut Self)) };
    }
}

/// Converts a command line option given in seconds to a `Duration`; a negative, infinite or
/// NaN value is refused with the option's name instead of panicking.
fn duration_from_user_secs(secs: f64, option: &str) -> Result<Duration, ExceptionPtr> {
    Duration::try_from_secs_f64(secs).map_err(|e| {
        ExceptionPtr::new(crate::engine::core::future::InvalidArgument::new(format!(
            "--{}: {}",
            option, e
        )))
    })
}

/// The root pointer of a shared io queue (the address in its block, see `share_io_queue`).
#[inline]
fn io_queue_ptr(q: &SharedPtr<IoQueue>) -> *mut IoQueue {
    q.get().map_or(std::ptr::null_mut(), std::ptr::NonNull::as_ptr)
}

/// Creates the shared I/O queue of `group` feeding `sink`.
///
/// # Safety
/// As [`IoQueue::construct_in_place`] for `sink`.
unsafe fn share_io_queue(
    group: crate::engine::core::io_queue::IoGroupPtr,
    sink: *mut crate::engine::core::internal::io_sink::IoSink,
) -> Result<SharedPtr<IoQueue>, ExceptionPtr> {
    let c = Box::into_raw(Box::new(std::mem::MaybeUninit::<IoQueueSharedCount>::uninit()))
        as *mut IoQueueSharedCount;
    // SAFETY: `c` is a fresh, aligned block; its fields are written in place, and the queue's
    // address is the block's for its whole life (the root pointer the queue keeps).
    unsafe {
        std::ptr::addr_of_mut!((*c).base).write(
            crate::engine::core::shared_ptr::SharedPtrCountBase::new(IoQueueSharedCount::destroy),
        );
        let q = std::ptr::addr_of_mut!((*c).queue);
        if let Err(e) = IoQueue::construct_in_place(q, group, sink) {
            // the queue is not constructed: free the block as uninitialized memory
            drop(Box::from_raw(c as *mut std::mem::MaybeUninit<IoQueueSharedCount>));
            return Err(e);
        }
        Ok(SharedPtr::from_parts(c as *mut crate::engine::core::shared_ptr::SharedPtrCountBase, q))
    }
}

/// A pointer handed to the reactor threads that the SMP configuration starts. The locals it
/// points to outlive the threads' use of them because each thread waits on `inited` before
/// it stops using them, and shard 0 waits on it as well.
struct SendPtr<T>(*mut T);
// Not derived: a derive would require `T: Copy`.
impl<T> Clone for SendPtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for SendPtr<T> {}
// SAFETY: see the type's documentation; accesses are ordered by the barriers.
unsafe impl<T> Send for SendPtr<T> {}

impl<T> SendPtr<T> {
    /// The pointer; a method, so that a closure captures the whole `SendPtr` (a field
    /// access would capture only the raw pointer, which is not `Send`).
    #[inline(always)]
    fn get(self) -> *mut T {
        self.0
    }
}

const MCL_CURRENT: i32 = 1;
const MCL_FUTURE: i32 = 2;
const MCL_ONFAULT: i32 = 4;

unsafe extern "C" {
    fn mlockall(flags: i32) -> i32;
    fn strerror(errnum: i32) -> *const std::ffi::c_char;
    fn pthread_setname_np(thread: usize, name: *const std::ffi::c_char) -> i32;
}

impl Smp {
    /// Configures and starts the shards from the SMP and reactor options: parses the CPU,
    /// memory and I/O configuration, starts one reactor thread per shard and builds the
    /// reactor of shard 0 on the calling thread.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid configuration or a failure to start a shard.
    pub fn configure(
        self: &Arc<Self>,
        smp_opts: &crate::engine::core::smp_options::SmpOptions,
        reactor_opts: &ReactorOptions,
    ) -> Result<(), ExceptionPtr> {
        use crate::engine::core::resource;
        let use_transparent_hugepages = !reactor_opts.overprovisioned.as_bool();

        if *smp_opts.enable_glibc_exception_scaling_workaround.get_value() {
            crate::engine::core::exception_hacks::init_phdr_cache();
        }

        // Mask most, to prevent threads (esp. dpdk helper threads)
        // from servicing a signal.  Individual reactors will unmask signals
        // as they become prepared to handle them.
        //
        // We leave some signals unmasked since we don't handle them ourself.
        let mut sigs = SigsetT::default();
        // SAFETY: valid signal set.
        unsafe { sigfillset(&mut sigs) };
        for sig in [
            SIGHUP, SIGQUIT, SIGILL, SIGABRT, SIGFPE, SIGSEGV, SIGALRM, SIGCONT, SIGSTOP, SIGTSTP,
            SIGTTIN, SIGTTOU,
        ] {
            // SAFETY: as above.
            unsafe { sigdelset(&mut sigs, sig) };
        }
        if !reactor_opts._auto_handle_sigint_sigterm {
            // SAFETY: as above.
            unsafe {
                sigdelset(&mut sigs, SIGINT);
                sigdelset(&mut sigs, SIGTERM);
            }
        }
        // SAFETY: as above.
        unsafe { pthread_sigmask(SIG_BLOCK, &sigs, std::ptr::null_mut()) };

        #[cfg(not(engine_asan_enabled))]
        install_oneshot_signal_handler(SIGSEGV, sigsegv_action)?;
        install_oneshot_signal_handler(SIGABRT, sigabrt_action)?;

        #[cfg(feature = "dpdk")]
        let native_stack = reactor_opts
            .network_stack
            .get_selected_candidate_opts_as::<crate::engine::net::native_stack::NativeStackOptions>(
        );
        #[cfg(feature = "dpdk")]
        self._using_dpdk.set(native_stack.is_some_and(|n| n.dpdk_pmd.as_bool()));

        let mut thread_affinity = *smp_opts.thread_affinity.get_value();
        if reactor_opts.overprovisioned.as_bool() && smp_opts.thread_affinity.defaulted() {
            thread_affinity = false;
        }
        if !thread_affinity && self._using_dpdk.get() {
            println!("warning: --thread-affinity 0 ignored in dpdk mode");
        }
        let mut mbind = *smp_opts.mbind.get_value();
        if !thread_affinity {
            mbind = false;
        }

        let mut rc = resource::Configuration::default();

        Smp::set_tmain(crate::engine::core::smp::current_thread_id());
        let mut cpu_set: resource::Cpuset = crate::engine::core::posix::get_current_cpuset();

        if smp_opts.cpuset.as_bool() {
            let opts_cpuset: resource::Cpuset = smp_opts.cpuset.get_value().clone();
            // CPUs that are not available are those pinned by
            // --cpuset but not present in current task set
            let not_available_cpus: std::collections::BTreeSet<u32> =
                opts_cpuset.difference(&cpu_set).copied().collect();

            if !not_available_cpus.is_empty() {
                let mut not_available_cpus_list = String::new();
                for cpu_id in &not_available_cpus {
                    not_available_cpus_list.push_str(&format!(" {}", cpu_id));
                }
                engine_logger().error(
                    crate::engine::util::log::FormatInfo::new(
                        "Bad value for --cpuset:{} not allowed. Shutting down.",
                    ),
                    format_args!(
                        "Bad value for --cpuset:{} not allowed. Shutting down.",
                        not_available_cpus_list
                    ),
                );
                std::process::exit(1);
            }
            cpu_set = opts_cpuset;
        }

        if smp_opts.smp.as_bool() {
            Smp::set_count(*smp_opts.smp.get_value());
        } else {
            Smp::set_count(cpu_set.len() as u32);
        }
        let count = Smp::count();
        let mut reactors: Vec<*mut Reactor> = vec![std::ptr::null_mut(); count as usize];
        if smp_opts.memory.as_bool() {
            #[cfg(engine_default_allocator)]
            engine_logger().warn(
                crate::engine::util::log::FormatInfo::new(
                    "Engine compiled with default allocator, --memory option won't take effect",
                ),
                format_args!(
                    "Engine compiled with default allocator, --memory option won't take effect"
                ),
            );
            rc.total_memory = Some(crate::engine::util::conversions::parse_memory_size(
                smp_opts.memory.get_value(),
            )?);
            #[cfg(feature = "dpdk")]
            if smp_opts.hugepages.as_bool()
                && reactor_opts.network_stack.get_selected_candidate_name() == "native"
                && self._using_dpdk.get()
            {
                let dpdk_memory = crate::engine::core::dpdk_rte::eal::mem_size(count);

                if dpdk_memory >= rc.total_memory.unwrap_or(0) {
                    engine_logger().error(
                        crate::engine::util::log::FormatInfo::new(
                            "Can't run with the given amount of memory: {}. Consider giving more.",
                        ),
                        format_args!(
                            "Can't run with the given amount of memory: {}. Consider giving more.",
                            smp_opts.memory.get_value()
                        ),
                    );
                    std::process::exit(1);
                }

                //
                // Subtract the memory we are about to give to DPDK from the total
                // amount of memory we are allowed to use.
                //
                if let Some(t) = rc.total_memory.as_mut() {
                    *t -= dpdk_memory;
                }
            }
        }
        if smp_opts.reserve_memory.as_bool() {
            rc.reserve_memory = Some(crate::engine::util::conversions::parse_memory_size(
                smp_opts.reserve_memory.get_value(),
            )?);
        }
        rc.reserve_additional_memory_per_shard = smp_opts.reserve_additional_memory_per_shard;
        let hugepages_path: Option<String> = if smp_opts.hugepages.as_bool() {
            Some(smp_opts.hugepages.get_value().clone())
        } else {
            None
        };
        let mut mlock = false;
        if smp_opts.lock_memory.as_bool() {
            mlock = *smp_opts.lock_memory.get_value();
        }
        if mlock {
            // Linux will serialize faulting in anonymous memory, and also
            // serialize marking them as locked. This can take many minutes on
            // terabyte class machines, so fault them in the future to spread
            // out the cost. This isn't good since we'll see contention if
            // multiple shards fault in memory at once, but if that work can be
            // in parallel to regular reactor work on other shards.
            let extra_flags = MCL_ONFAULT; // Linux 4.4+
            // SAFETY: plain mlockall.
            let r = unsafe { mlockall(MCL_CURRENT | MCL_FUTURE | extra_flags) };
            if r != 0 {
                // Don't hard fail for now, it's hard to get the configuration right
                // SAFETY: strerror returns a NUL-terminated static string.
                let msg = unsafe { std::ffi::CStr::from_ptr(strerror(errno())) }.to_string_lossy();
                println!("warning: failed to mlockall: {}", msg);
            }
        }

        rc.cpus = count as usize;
        rc.cpu_set = cpu_set;

        let mut disk_config = DiskConfigParams::new(Reactor::MAX_QUEUES);
        disk_config.parse_config(smp_opts, reactor_opts)?;
        for id in disk_config.queue_ids() {
            rc.io_queues.push(id);
        }
        rc.num_io_groups = disk_config.num_io_groups();

        #[cfg(feature = "hwloc")]
        if *smp_opts.allow_cpus_in_remote_numa_nodes.get_value() {
            rc.assign_orphan_cpus = true;
        }

        let mut resources = resource::allocate(&mut rc)?;
        crate::engine::util::log::Logger::set_shard_field_width(
            (count as f64).log10().ceil() as u32
        );
        let allocations: Vec<resource::Cpu> = std::mem::take(&mut resources.cpus);
        if thread_affinity {
            self.pin(allocations[0].cpu_id);
        }
        let mut layout: Option<memory::internal::NumaLayout> = None;
        if smp_opts.memory_allocator == crate::engine::core::smp_options::MemoryAllocator::Engine {
            layout = Some(memory::configure(
                allocations[0].mem.clone(),
                mbind,
                use_transparent_hugepages,
                hugepages_path.clone(),
            )?);
        } else {
            // #2148 - if running engine allocator but options that contradict this, we still need to
            // init memory at least minimally, otherwise a bunch of stuff breaks.
            // Previously, we got away wth this by accident due to #2137.
            memory::configure_minimal();
        }

        {
            // SAFETY: no reactor thread runs yet.
            let m = unsafe { &mut *self._shard_to_numa_node_mapping.get() };
            m.clear();
            m.reserve(count as usize);
            for a in allocations.iter().take(count as usize) {
                m.push(if !a.mem.is_empty() { a.mem[0].nodeid } else { 0 });
            }
        }

        if reactor_opts.abort_on_engine_bad_alloc.as_bool() {
            memory::set_abort_on_allocation_failure(true);
        }

        if reactor_opts.dump_memory_diagnostics_on_alloc_failure_kind.as_bool() {
            memory::set_dump_memory_diagnostics_on_alloc_failure_kind(
                *reactor_opts.dump_memory_diagnostics_on_alloc_failure_kind.get_value(),
            );
        }

        let mut max_networking_aio_io_control_blocks =
            *reactor_opts.max_networking_io_control_blocks.get_value();
        // Prevent errors about insufficient AIO blocks, when they are not needed by the reactor backend.
        if reactor_opts.reactor_backend.get_selected_candidate().name() == "linux-aio" {
            max_networking_aio_io_control_blocks =
                Self::adjust_max_networking_aio_io_control_blocks(
                    max_networking_aio_io_control_blocks,
                    *reactor_opts.reserve_io_control_blocks.get_value(),
                )?;
        }

        let mut reactor_cfg = ReactorConfig {
            task_quota: SchedDuration::from_std(duration_from_user_secs(
                *reactor_opts.task_quota_ms.get_value() / 1000.0,
                "task-quota-ms",
            )?),
            max_poll_time: if reactor_opts.poll_mode.as_bool() {
                SchedDuration::max()
            } else if reactor_opts.overprovisioned.as_bool()
                && reactor_opts.idle_poll_time_us.defaulted()
            {
                SchedDuration::default()
            } else {
                SchedDuration::from_std(Duration::from_micros(u64::from(
                    *reactor_opts.idle_poll_time_us.get_value(),
                )))
            },
            handle_sigint: !reactor_opts.no_handle_interrupt.as_bool(),
            auto_handle_sigint_sigterm: reactor_opts._auto_handle_sigint_sigterm,
            max_networking_aio_io_control_blocks,
            force_io_getevents_syscall: *reactor_opts.force_aio_syscalls.get_value(),
            kernel_page_cache: *reactor_opts.kernel_page_cache.get_value(),
            have_aio_fsync: *reactor_opts.aio_fsync.get_value(),
            max_task_backlog: *reactor_opts.max_task_backlog.get_value(),
            strict_o_direct: !reactor_opts.relaxed_dma.as_bool(),
            bypass_fsync: *reactor_opts.unsafe_bypass_fsync.get_value(),
            no_poll_aio: !*reactor_opts.poll_aio.get_value()
                || (reactor_opts.poll_aio.defaulted() && reactor_opts.overprovisioned.as_bool()),
        };

        // Disable hot polling if the scheduler wakeup granularity is too high: the
        // direct-I/O thread would be starved otherwise.
        if !reactor_cfg.no_poll_aio || reactor_cfg.max_poll_time != SchedDuration::default() {
            let (wakeup_file, granularity) = wakeup_granularity();
            // 15M is chosen as it's what tuned sets. Though you probably already
            // see an adverse effect earlier.
            if granularity >= 15_000_000 {
                reactor_cfg.no_poll_aio = true;
                reactor_cfg.max_poll_time = SchedDuration::default();
                engine_logger().warn(crate::engine::util::log::FormatInfo::new("Setting --poll-aio=0 and --idle-poll-time-us=0 due to too high sched_wakeup_granularity of {} in {}"), format_args!(
                    "Setting --poll-aio=0 and --idle-poll-time-us=0 due to too high sched_wakeup_granularity of {} in {}",
                    granularity, wakeup_file.display()));
            }
        }

        AIO_NOWAIT_OVERRIDE.store(
            if *reactor_opts.linux_aio_nowait.get_value() { 1 } else { 2 },
            Ordering::Relaxed,
        );
        let mtx = std::sync::Mutex::new(());

        #[cfg(engine_heapprof)]
        let heapprof_sampling_rate: usize = {
            let r = *reactor_opts.heapprof.get_value() as usize;
            if r != 0 {
                memory::set_heap_profiling_sampling_rate(r);
            }
            r
        };
        #[cfg(not(engine_heapprof))]
        let heapprof_sampling_rate: usize = 0;

        #[cfg(feature = "dpdk")]
        if self._using_dpdk.get() {
            let mut cpus = crate::engine::core::dpdk_rte::eal::Cpuset::default();
            for a in &allocations {
                cpus.set(a.cpu_id as usize, true);
            }
            crate::engine::core::dpdk_rte::eal::init(
                cpus,
                &reactor_opts._argv0,
                hugepages_path.clone(),
                native_stack.is_some_and(|n| n.dpdk_pmd.as_bool()),
            )?;
        }

        // Better to put it into the smp class, but at smp construction time
        // correct smp::count is not known.
        let reactors_registered = std::sync::Barrier::new(count as usize);
        let smp_queues_constructed = std::sync::Barrier::new(count as usize);
        // We use shared_ptr since this thread can exit while other threads are still unlocking
        let inited = Arc::new(std::sync::Barrier::new(count as usize));

        let mut ioq_topology = std::mem::take(&mut resources.ioq_topology);

        // ATTN: The ioq_topology value is referenced by below lambdas which are
        // then copied to other shard's threads, so each shard has a copy of the
        // ioq_topology on stack, but (!) still references and uses the value
        // from shard-0. This access is race-free because
        //  1. The .shard_to_group is not modified
        //  2. The .queues is pre-resize()-d in advance, so the vector itself
        //     doesn't change; existing slots are accessed by owning shards only
        //     without interference
        //  3. The .groups manipulations are guarded by the .lock lock (but it's
        //     also pre-resize()-d in advance)

        let ioq_p = SendPtr(
            &mut ioq_topology as *mut resource::UnorderedMap<u32, resource::IoQueueTopology>,
        );
        let disk_p = SendPtr(&mut disk_config as *mut DiskConfigParams);
        let alloc_io_queues = move |shard: ShardId| -> Result<(), ExceptionPtr> {
            // SAFETY: see SendPtr; the topology and the disk config outlive every shard's setup,
            // and every thread only reads them (the topology's cells, see `IoQueueTopology`).
            let (ioq_topology, disk_config) = unsafe { (&*ioq_p.get(), &*disk_p.get()) };
            for (q, io_info) in ioq_topology.iter() {
                let num_io_groups = {
                    let _g = io_info.lock.lock_guard();
                    // SAFETY: `groups` is touched only with the lock held.
                    unsafe { (*io_info.groups.get()).len() as u32 }
                };
                // Runs on the shard's thread before its reactor runs; short accesses.
                let r = engine();
                // SAFETY: as above.
                let cur = unsafe { (*r)._num_io_groups };
                if cur == 0 {
                    // SAFETY: as above.
                    unsafe { (*r)._num_io_groups = num_io_groups };
                } else if cur != num_io_groups {
                    return Err(ExceptionPtr::new(crate::engine::core::future::LogicError::new(
                        format!("Number of IO-groups mismatch, {} != {}", cur, num_io_groups),
                    )));
                }

                let group_idx = io_info.shard_to_group[shard as usize];
                let group: Arc<IoGroup>;

                {
                    let _g = io_info.lock.lock_guard();
                    // SAFETY: as above; the lock makes this the only reference to the groups.
                    let groups = unsafe { &mut *io_info.groups.get() };
                    let nr_groups = groups.len() as u32;
                    let iog = &mut groups[group_idx as usize];
                    if iog.is_none() {
                        let qcfg = disk_config.generate_config(*q, nr_groups);
                        *iog = Some(Arc::new(IoGroup::new(
                            qcfg,
                            io_info.shards_in_group[group_idx as usize],
                        )?));
                        engine_logger().debug(
                            crate::engine::util::log::FormatInfo::new(
                                "allocate {} IO group with {} queues, queue-id {}",
                            ),
                            format_args!(
                                "allocate {} IO group with {} queues, queue-id {}",
                                group_idx, io_info.shards_in_group[group_idx as usize], q
                            ),
                        );
                    }
                    group = iog.as_ref().map(Arc::clone).ok_or_else(|| {
                        ExceptionPtr::new(crate::engine::core::future::LogicError::new(
                            "io group".to_string(),
                        ))
                    })?;
                }

                // SAFETY: the sink is a field of this shard's reactor, constructed in place,
                // which the queue refers to for its life.
                let sink = unsafe { std::ptr::addr_of_mut!((*engine())._io_sink) };
                // SAFETY: the sink is the reactor's, which outlives its io queues.
                let queue = unsafe { share_io_queue(group, sink) }?;
                // SAFETY: each shard writes only its own slot.
                unsafe { *io_info.queues[shard as usize].get() = Some(queue) };
                engine_logger().debug(
                    crate::engine::util::log::FormatInfo::new(
                        "attached {} queue to {} IO group, queue-id {}",
                    ),
                    format_args!(
                        "attached {} queue to {} IO group, queue-id {}",
                        shard, group_idx, q
                    ),
                );
            }
            Ok(())
        };

        let assign_io_queues = move |shard: ShardId| {
            // SAFETY: as above.
            let (ioq_topology, disk_config) = unsafe { (&*ioq_p.get(), &*disk_p.get()) };
            for (q, io_info) in ioq_topology.iter() {
                // SAFETY: each shard takes only its own slot.
                let queue = unsafe { (*io_info.queues[shard as usize].get()).take() };
                engine_assert!(queue.is_some());
                let Some(queue) = queue else { continue };
                for &dev in disk_config.queue_devices(*q) {
                    // SAFETY: on the shard's thread before its reactor runs; a short access.
                    unsafe { (*engine())._io_queues.entry(dev).or_insert_with(|| queue.clone()) };
                }
            }
        };

        // SAFETY: no reactor thread runs yet.
        unsafe {
            *self._all_event_loops_done.get() = Some(std::sync::Barrier::new(count as usize))
        };

        let backend_selector = reactor_opts.reactor_backend.get_selected_candidate().clone();
        engine_logger().info(
            crate::engine::util::log::FormatInfo::new("Reactor backend: {}"),
            format_args!("Reactor backend: {}", backend_selector),
        );

        let smp_tmain = Smp::tmain();
        let reactors_registered_p =
            SendPtr(&reactors_registered as *const std::sync::Barrier as *mut std::sync::Barrier);
        let smp_queues_constructed_p = SendPtr(
            &smp_queues_constructed as *const std::sync::Barrier as *mut std::sync::Barrier,
        );
        let smp_opts_p = SendPtr(
            smp_opts as *const crate::engine::core::smp_options::SmpOptions
                as *mut crate::engine::core::smp_options::SmpOptions,
        );
        let reactor_opts_p = SendPtr(reactor_opts as *const ReactorOptions as *mut ReactorOptions);
        let reactors_p = SendPtr(reactors.as_mut_ptr());
        let mtx_p = SendPtr(&mtx as *const std::sync::Mutex<()> as *mut std::sync::Mutex<()>);
        let layout_p = SendPtr(&mut layout as *mut Option<memory::internal::NumaLayout>);
        for i in 1..count {
            let allocation = allocations[i as usize].clone();
            let inited = inited.clone();
            let hugepages_path = hugepages_path.clone();
            let backend_selector = backend_selector.clone();
            let reactor_cfg = reactor_cfg.clone();
            let this = self.clone();
            self.create_thread(Box::new(move || {
                // SAFETY: see SendPtr; every pointee outlives this thread's use of it.
                let (reactors_registered, smp_queues_constructed, smp_opts, reactor_opts) = unsafe {
                    (
                        &*reactors_registered_p.get(),
                        &*smp_queues_constructed_p.get(),
                        &*smp_opts_p.get(),
                        &*reactor_opts_p.get(),
                    )
                };
                let run = || -> Result<(), ExceptionPtr> {
                    // initialize thread_locals that are equal across all reacto threads of this smp instance
                    if let Some(id) = smp_tmain {
                        Smp::set_tmain(id);
                    }
                    let thread_name =
                        std::ffi::CString::new(format!("reactor-{}", i)).unwrap_or_default();
                    // SAFETY: NUL-terminated name.
                    unsafe { pthread_setname_np(pthread_self(), thread_name.as_ptr()) };
                    if thread_affinity {
                        this.pin(allocation.cpu_id);
                    }
                    if smp_opts.memory_allocator
                        == crate::engine::core::smp_options::MemoryAllocator::Engine
                    {
                        let another_layout = memory::configure(
                            allocation.mem.clone(),
                            mbind,
                            use_transparent_hugepages,
                            hugepages_path.clone(),
                        )?;
                        // SAFETY: as above; the mutex orders the merges.
                        let guard = unsafe { (*mtx_p.get()).lock() };
                        // SAFETY: the guard above is held, so no other thread touches the layout until it is
                        // dropped below.
                        let layout = unsafe { &mut *layout_p.get() };
                        if let Some(l) = layout.take() {
                            *layout = Some(memory::internal::merge(l, another_layout));
                        }
                        drop(guard);
                    } else {
                        // See comment above (shard 0)
                        memory::configure_minimal();
                    }
                    if heapprof_sampling_rate != 0 {
                        memory::set_heap_profiling_sampling_rate(heapprof_sampling_rate);
                    }
                    let mut mask = SigsetT::default();
                    // SAFETY: valid signal set.
                    unsafe {
                        sigfillset(&mut mask);
                        sigdelset(&mut mask, SIGSEGV);
                    }
                    // SAFETY: as above.
                    let r = unsafe { pthread_sigmask(SIG_BLOCK, &mask, std::ptr::null_mut()) };
                    crate::engine::core::posix::throw_pthread_error(r)?;
                    crate::engine::core::smp::init_default_smp_service_group(i);
                    LowresClock::update();
                    this.allocate_reactor(i, backend_selector.clone(), reactor_cfg.clone())?;
                    // SAFETY: `reactors` has `count` slots; slot `i` belongs to this thread.
                    unsafe { *reactors_p.get().add(i as usize) = engine() };
                    alloc_io_queues(i)?;
                    reactors_registered.wait();
                    smp_queues_constructed.wait();
                    // _qs_owner is only initialized here
                    this.set_qs_from_owner();
                    this.start_all_queues()?;
                    assign_io_queues(i);
                    inited.wait();
                    // SAFETY: this thread's reactor was just constructed; configure() runs no
                    // task, and run() takes the pointer, as the loop re-enters the reactor.
                    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
                    unsafe { (*engine()).configure(reactor_opts)? };
                    // SAFETY: `engine()` is this thread's live reactor, and no reference to it is live:
                    // `do_run` takes the pointer, as the loop re-enters the reactor.
                    unsafe { Reactor::do_run(engine())? };
                    Ok(())
                };
                if let Err(e) = run() {
                    engine_logger().error(
                        crate::engine::util::log::FormatInfo::new("{}"),
                        format_args!("{}", crate::engine::util::log::DisplayExceptionPtr(Some(&e))),
                    );
                    // SAFETY: `_exit` terminates the process.
                    unsafe { _exit(1) };
                }
            }))?;
        }

        crate::engine::core::smp::init_default_smp_service_group(0);
        LowresClock::update();
        if let Err(e) = self.allocate_reactor(0, backend_selector.clone(), reactor_cfg.clone()) {
            engine_logger().error(
                crate::engine::util::log::FormatInfo::new("{}"),
                format_args!("{}", crate::engine::util::log::DisplayExceptionPtr(Some(&e))),
            );
            // SAFETY: `_exit` terminates the process.
            unsafe { _exit(1) };
        }

        reactors[0] = engine();
        alloc_io_queues(0)?;

        #[cfg(feature = "dpdk")]
        if self._using_dpdk.get() {
            // SAFETY: the EAL was initialized above; the reactor threads do not use
            // `_thread_loops`, which nothing changes until `cleanup()`, after every reactor
            // thread has been joined.
            unsafe {
                crate::engine::core::dpdk_rte::launch_thread_loops(&mut *self._thread_loops.get())
            };
        }

        reactors_registered.wait();
        // The queue matrix: count rows of count queues, each row allocated with the alignment of
        // smp_message_queue (it has members with hefty alignment requirements).
        self.allocate_qs_owner(count, |i, j| {
            SmpMessageQueue::new(reactors[j as usize], reactors[i as usize])
        });
        self.set_qs_from_owner();
        // SAFETY: the other reactor threads wait on `smp_queues_constructed` and do not touch
        // the alien instance until then.
        unsafe { (*self._alien).set_qs(alien::Instance::create_qs(&reactors)) };
        smp_queues_constructed.wait();
        self.start_all_queues()?;
        assign_io_queues(0);
        inited.wait();

        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
        unsafe { (*engine()).configure(reactor_opts) }?;

        if smp_opts.lock_memory.as_bool()
            && *smp_opts.lock_memory.get_value()
            && let Some(l) = layout.take()
            && !l.ranges.is_empty()
        {
            self.setup_prefaulter(&resources, l)?;
        }
        Ok(())
    }

    /// Polls this shard's inter-shard queues: runs received requests and returned
    /// completions; returns `true` if any work was done.
    pub fn poll_queues() -> bool {
        let mut got: usize = 0;
        let me = crate::engine::core::shard_id::this_shard_id();
        for i in 0..Smp::count() {
            if me != i {
                // SAFETY: the matrix is installed on every reactor thread before it polls;
                // `_qs[me][i]` is received by this shard and `_qs[i][me]` sent by it, and each
                // call touches only this shard's side of the queue.
                unsafe {
                    let rxq = Smp::queue(me, i);
                    SmpMessageQueue::flush_response_batch(rxq);
                    got += SmpMessageQueue::has_unflushed_responses(rxq) as usize;
                    got += SmpMessageQueue::process_incoming(rxq);
                    let txq = Smp::queue(i, me);
                    SmpMessageQueue::flush_request_batch(txq);
                    got += SmpMessageQueue::process_completions(txq, i);
                }
            }
        }
        got != 0
    }

    /// Whether this shard's inter-shard queues have work, without doing it.
    pub fn pure_poll_queues() -> bool {
        let me = crate::engine::core::shard_id::this_shard_id();
        for i in 0..Smp::count() {
            if me != i {
                // SAFETY: as in poll_queues().
                unsafe {
                    let rxq = Smp::queue(me, i);
                    SmpMessageQueue::flush_response_batch(rxq);
                    let txq = Smp::queue(i, me);
                    SmpMessageQueue::flush_request_batch(txq);
                    if SmpMessageQueue::pure_poll_rx(rxq)
                        || SmpMessageQueue::pure_poll_tx(txq)
                        || SmpMessageQueue::has_unflushed_responses(rxq)
                    {
                        return true;
                    }
                }
            }
        }
        false
    }
}

/// The `linux-aio-nowait` option as seen by [`aio_nowait_supported`]: 0 = not set by the
/// options (use the kernel probe), 1 = true, 2 = false.
static AIO_NOWAIT_OVERRIDE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

// (`__thread reactor* local_engine;` is local_engine_slot(), above.)

/// Logs `message` with the error `eptr` on the reactor's logger.
pub fn report_exception(message: &str, eptr: &ExceptionPtr) {
    engine_logger().error(
        crate::engine::util::log::FormatInfo::new("{}: {}"),
        format_args!("{}: {}", message, crate::engine::util::log::DisplayExceptionPtr(Some(eptr))),
    );
}

/// The state of a direct I/O probe of [`check_direct_io_support`].
struct DirectIoProbe {
    path: Sstring,
    flags: OpenFlags,
    cleanup: Box<dyn FnOnce() -> Future<()>>,
}

impl DirectIoProbe {
    fn parse(
        path: Sstring,
        type_: Option<DirectoryEntryType>,
    ) -> Result<DirectIoProbe, ExceptionPtr> {
        let Some(type_) = type_ else {
            return Err(ExceptionPtr::new(crate::engine::core::future::InvalidArgument::new(
                format!("Could not open file at {}. Make sure it exists", path),
            )));
        };

        if type_ == DirectoryEntryType::Directory {
            let fpath = Sstring::from(format!("{}/.o_direct_test", path));
            let f2 = fpath.clone();
            Ok(DirectIoProbe {
                path: fpath,
                flags: OpenFlags::WO | OpenFlags::CREATE | OpenFlags::TRUNCATE,
                cleanup: Box::new(move || crate::engine::core::api::remove_file(f2.as_str())),
            })
        } else if type_ == DirectoryEntryType::Regular || type_ == DirectoryEntryType::Link {
            Ok(DirectIoProbe {
                path,
                flags: OpenFlags::RO,
                cleanup: Box::new(|| make_ready_future(())),
            })
        } else {
            Err(ExceptionPtr::new(crate::engine::core::future::InvalidArgument::new(format!(
                "{} neither a directory nor file. Can't be opened with O_DIRECT",
                path
            ))))
        }
    }
}

/// Opening files and directories.
pub use crate::engine::core::file::{open_directory, open_file_dma, open_file_dma_with_options};

/// Checks that `path` supports direct I/O (`O_DIRECT`).
pub fn check_direct_io_support(path: &str) -> Future<()> {
    // Allocating memory for a sstring can throw, hence the futurize_invoke
    let path = Sstring::from(path);
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).file_type(path.as_str(), FollowSymlink::YES) }.then(move |type_: Option<DirectoryEntryType>| {
        let w = fut_try!(DirectIoProbe::parse(path, type_));
        let wpath = w.path.clone();
        let cleanup = w.cleanup;
        crate::engine::core::api::open_file_dma(w.path.as_str(), w.flags).then_wrapped(move |f: Future<File>| {
            match f.get() {
                Ok(mut fd) => cleanup().finally(move || fd.close()),
                Err(e) => {
                    if e.downcast_ref::<SystemError>().is_some_and(|s| s.code() == crate::engine::core::reactor_backend::EINVAL) {
                        report_exception(&format!("Could not open file at {}. Does your filesystem support O_DIRECT?", wpath), &e);
                    }
                    make_exception_future(e)
                }
            }
        })
    })
}

/// Listens on `sa` with the current shard's network stack.
///
/// # Errors
///
/// Returns the stack's listen error.
pub fn listen(sa: SocketAddress) -> Result<ServerSocket, ExceptionPtr> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).listen(sa, ListenOptions::default()) }
}

/// Listens on `sa` with `opts` with the current shard's network stack.
///
/// # Errors
///
/// Returns the stack's listen error.
pub fn listen_with(sa: SocketAddress, opts: ListenOptions) -> Result<ServerSocket, ExceptionPtr> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).listen(sa, opts) }
}

/// Connects to `sa` over TCP with the current shard's network stack.
pub fn connect(sa: SocketAddress) -> Future<ConnectedSocket> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).connect(sa) }
}

/// Connects to `sa` from `local` with transport `proto`.
pub fn connect_from(
    sa: SocketAddress,
    local: SocketAddress,
    proto: Transport,
) -> Future<ConnectedSocket> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).connect_from(sa, local, proto) }
}

/// A socket of the current shard's network stack.
pub fn make_socket() -> crate::engine::net::api::Socket {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).net() }.socket()
}

/// The IPv4 address family.
pub const AF_INET: u16 = 2;

/// An unbound UDP channel.
///
/// # Errors
///
/// Returns the stack's error.
pub fn make_udp_channel() -> Result<crate::engine::net::api::UdpChannel, ExceptionPtr> {
    make_unbound_datagram_channel(AF_INET)
}

/// A UDP channel bound to `local`.
///
/// # Errors
///
/// Returns the stack's error.
pub fn make_udp_channel_bound(
    local: &SocketAddress,
) -> Result<crate::engine::net::api::UdpChannel, ExceptionPtr> {
    make_bound_datagram_channel(local)
}

/// An unbound datagram channel of `family`.
///
/// # Errors
///
/// Returns the stack's error.
pub fn make_unbound_datagram_channel(
    family: u16,
) -> Result<crate::engine::net::api::DatagramChannel, ExceptionPtr> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).net() }.make_unbound_datagram_channel(family)
}

/// A datagram channel bound to `local`.
///
/// # Errors
///
/// Returns the stack's error.
pub fn make_bound_datagram_channel(
    local: &SocketAddress,
) -> Result<crate::engine::net::api::DatagramChannel, ExceptionPtr> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).net() }.make_bound_datagram_channel(local)
}

impl Reactor {
    /// Queues `t` to run before the other tasks of its group.
    pub fn add_high_priority_task(&mut self, t: TaskPtr) {
        self.add_urgent_task(t);
        // break .then() chains
        self.request_preemption();
    }
}

/// Sets this shard's idle CPU handler (see [`IdleCpuHandler`]).
pub fn set_idle_cpu_handler(handler: IdleCpuHandler) {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).set_idle_cpu_handler(handler) };
}

/// Experimental APIs.
pub mod experimental {
    use super::*;

    /// Creates a pipe; resolves with its read and write ends.
    pub fn make_pipe() -> Future<(FileDesc, FileDesc)> {
        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
        unsafe { (*engine()).make_pipe() }
    }

    /// Spawns the process `pathname` with `params`.
    pub fn spawn_process(
        pathname: &std::path::Path,
        params: crate::engine::util::process::experimental::SpawnParameters,
    ) -> Future<crate::engine::util::process::experimental::Process> {
        crate::engine::util::process::experimental::Process::spawn_with(pathname, params)
    }

    /// Spawns the process `pathname` with default parameters.
    pub fn spawn_process_default(
        pathname: &std::path::Path,
    ) -> Future<crate::engine::util::process::experimental::Process> {
        crate::engine::util::process::experimental::Process::spawn(pathname)
    }
}

fn virtualized() -> bool {
    std::path::Path::new("/sys/hypervisor/type").exists()
}

impl Reactor {
    /// The idle polling time: 2 ms when virtualized (wake-ups are slow), 200 us otherwise.
    pub(crate) fn calculate_poll_time() -> Duration {
        // In a non-virtualized environment, select a poll time
        // that is competitive with halt/unhalt.
        //
        // In a virutalized environment, IPIs are slow and dominate
        // sleep/wake (mprotect/tgkill), so increase poll time to reduce
        // so we don't sleep in a request/reply workload
        if virtualized() { Duration::from_micros(2000) } else { Duration::from_micros(200) }
    }
}

/// Yields to the reactor: the returned future resolves after the other pending tasks ran.
pub fn yield_() -> Future<()> {
    let _section =
        crate::engine::util::critical_alloc_section::memory::ScopedCriticalAllocSection::new();
    let tsk = make_task(current_scheduling_group(), || {});
    // SAFETY: a fresh task from make_task, queued right after.
    unsafe {
        let fut = LambdaTask::get_future(tsk);
        schedule(LambdaTask::as_task_ptr(tsk));
        fut
    }
}

/// Like [`yield_`], but makes the reactor poll for I/O before resolving.
pub fn check_for_io_immediately() -> Future<()> {
    let _section =
        crate::engine::util::critical_alloc_section::memory::ScopedCriticalAllocSection::new();
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).force_poll() };
    let tsk = make_task(default_scheduling_group(), || {});
    // SAFETY: a fresh task from make_task, queued right after.
    unsafe {
        let fut = LambdaTask::get_future(tsk);
        schedule(LambdaTask::as_task_ptr(tsk));
        fut
    }
}

/// Same as [`check_for_io_immediately`].
pub fn later() -> Future<()> {
    check_for_io_immediately()
}

impl Reactor {
    /// Total time the reactor was idle.
    pub fn total_idle_time(&self) -> SchedDuration {
        self._total_idle
    }

    /// Total time the reactor was busy.
    pub fn total_busy_time(&self) -> SchedDuration {
        // Busy time is exported as a counter, so it must never decrease. While the reactor
        // is idle it holds at its value from the start of the idle period. Ending the period
        // adds at most the time elapsed since then to `_total_idle`, so the value can only go
        // up from there.
        let end = self._idle_start.unwrap_or_else(Self::now);
        end - self._start_time - self._total_idle
    }

    /// Records that the reactor was idle at `t`, starting an idle period if none is open.
    fn mark_idle(&mut self, t: SchedTimePoint) {
        self._idle_end = t;
        if self._idle_start.is_none() {
            self._idle_start = Some(t);
        }
    }

    /// Adds the open idle period up to `_idle_end` to `_total_idle`, leaving it open from
    /// there. Does nothing when not idle.
    fn update_idle(&mut self) {
        if let Some(start) = self._idle_start {
            self._total_idle += self._idle_end - start;
            self._idle_start = Some(self._idle_end);
        }
    }

    /// Adds the open idle period up to `_idle_end` to `_total_idle` and closes it. Does
    /// nothing when not idle.
    fn end_idle(&mut self) {
        self.update_idle();
        self._idle_start = None;
    }

    /// Total time the reactor thread was awake.
    pub fn total_awake_time(&self) -> SchedDuration {
        Self::now() - self._start_time - self._total_sleep
    }

    /// Total CPU time consumed by the reactor thread.
    pub fn total_cpu_time(&self) -> SchedDuration {
        SchedDuration::from_nanos(
            crate::engine::core::thread_cputime_clock::ThreadCputimeClock::now()
                .time_since_epoch()
                .0,
        )
    }

    /// Total steal time (awake time not spent on the CPU), kept monotonic.
    pub fn total_steal_time(&mut self) -> SchedDuration {
        // Steal time: this mimics the concept some Hypervisors have about Steal time.
        // That is the time in which a VM has something to run, but is not running because some other
        // process (another VM or the hypervisor itself) is in control.
        //
        // For us, we notice that during the time in which we were not sleeping (either running or busy
        // polling while idle), we should be accumulating thread runtime. If we are not, that's because
        // someone stole it from us.
        //
        // Because this is totally in userspace we can miss some events. For instance, if the engine
        // process is ready to run but the kernel hasn't scheduled us yet, that would be technically
        // steal time but we have no ways to account it.
        //
        // Furthermore, not all steal is from other processes: time used by the syscall thread and any
        // alien threads will show up as steal as well as any time spent in a system call that
        // unexpectedly blocked (since CPU time won't tick up when that occurs).
        //
        // But what we have here should be good enough and at least has a well defined meaning.
        //
        // Because we calculate sleep time with timestamps around polling methods that may sleep, like
        // io_getevents, we systematically over-count sleep time, since there is CPU usage within the
        // period timed as sleep, before and after an actual sleep occurs (and no sleep may occur at all,
        // e.g., if there are events immediately available). Over-counting sleep means we under-count the
        // wall-clock awake time, and so if there is no "true" steal, we will generally have a small
        // *negative* steal time, because we under-count awake wall clock time while thread CPU time does
        // not have a corresponding error.
        //
        // Becuase we claim "steal" is a counter, we must ensure that it never deceases, because PromQL
        // functions which use counters will produce non-sensical results if they do. Therefore we clamp
        // the output such that it never decreases.
        //
        // Finally, we don't just clamp difference of awake and CPU time since proces start at 0, but
        // take the last value we returned from this function and then calculate the incremental steal
        // time since that measurement, clamped to 0. This means that as soon as steal time becomes
        // positive, it will be reflected in the measurement, rather than needing to "consume" all the
        // accumulated negative steal time before positive steal times start showing up.

        let true_steal = self.total_awake_time() - self.total_cpu_time();
        let mono_steal = self._last_mono_steal
            + (true_steal - self._last_true_steal).max(SchedDuration::default());

        self._last_true_steal = true_steal;
        self._last_mono_steal = mono_steal;

        mono_steal
    }
}

static S_USED_SCHEDULING_GROUP_IDS_BITMAP: AtomicU64 = AtomicU64::new(3); // 0=main, 1=atexit
static S_NEXT_SCHEDULING_GROUP_SPECIFIC_KEY: AtomicU64 = AtomicU64::new(0);

fn allocate_scheduling_group_id() -> i32 {
    const _: () =
        assert!(max_scheduling_groups() <= u64::BITS, "more scheduling groups than available bits");
    let mut b = S_USED_SCHEDULING_GROUP_IDS_BITMAP.load(Ordering::Relaxed);
    loop {
        if b.count_ones() == max_scheduling_groups() {
            return -1;
        }
        let i = (!b).trailing_zeros();
        let nb = b | (1u64 << i);
        match S_USED_SCHEDULING_GROUP_IDS_BITMAP.compare_exchange_weak(
            b,
            nb,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return i as i32,
            Err(cur) => b = cur,
        }
    }
}

fn allocate_scheduling_group_specific_key() -> u64 {
    S_NEXT_SCHEDULING_GROUP_SPECIFIC_KEY.fetch_add(1, Ordering::Relaxed)
}

fn deallocate_scheduling_group_id(id: u32) {
    S_USED_SCHEDULING_GROUP_IDS_BITMAP.fetch_and(!(1u64 << id), Ordering::Relaxed);
}

use crate::engine::core::scheduling_specific::internal::SpecificVal;
use crate::engine::core::shared_ptr::LwSharedPtr;

unsafe extern "C" {
    fn aligned_alloc(alignment: usize, size: usize) -> *mut std::ffi::c_void;
}

fn allocate_scheduling_group_specific_data(
    _sg: SchedulingGroup,
    _key_id: u64,
    cfg: &LwSharedPtr<SchedulingGroupKeyConfig>,
) -> Result<SpecificVal, ExceptionPtr> {
    // SAFETY: aligned_alloc has no preconditions; the result is checked below.
    let valp = unsafe { aligned_alloc(cfg.alignment, cfg.allocation_size) };
    if valp.is_null() {
        return Err(ExceptionPtr::new(crate::engine::core::future::RuntimeError::new(
            "memory allocation failed",
        )));
    }
    // SAFETY: `valp` is fresh storage of `allocation_size` bytes aligned for the key's type,
    // released by `free`.
    unsafe { SpecificVal::with(valp, SpecificVal::free, &Some(cfg.clone())) }
}

fn store_specific_val(
    this_sg: &mut crate::engine::core::scheduling_specific::internal::PerSchedulingGroup,
    key_id: u64,
    v: SpecificVal,
) {
    let need = (key_id + 1) as usize;
    if this_sg.specific_vals.len() < need {
        this_sg.specific_vals.resize_with(need, SpecificVal::new);
    }
    this_sg.specific_vals[key_id as usize] = v;
}

impl Reactor {
    fn rename_scheduling_group_specific_data(&mut self, sg: SchedulingGroup) -> Future<()> {
        let this = self as *mut Reactor;
        let func = move || {
            crate::engine::core::with_scheduling_group::with_scheduling_group(sg, move || {
                // SAFETY: the reactor outlives its scheduling-group operations.
                unsafe { (*this).get_sg_data(&sg).rename() };
                make_ready_future(())
            })
        };
        // SAFETY: the mutex is a field of the reactor, which never moves and outlives its scheduling-group operations.
        unsafe {
            crate::engine::core::shared_mutex::with_shared(
                &mut self._scheduling_group_keys_mutex,
                func,
            )
        }
    }

    fn init_scheduling_group(
        &mut self,
        sg: SchedulingGroup,
        name: Sstring,
        shortname: Sstring,
        shares: f32,
    ) -> Future<()> {
        let this = self as *mut Reactor;
        let func = move || {
            // SAFETY: the reactor outlives its scheduling-group operations.
            let me = unsafe { &mut *this };
            me.get_sg_data(&sg).queue_is_initialized = true;
            me._task_queues[sg.id as usize] =
                Some(task_queue_node(crate::engine::core::reactor::fut_try!(TaskQueue::new(
                    sg.id, name, shortname, shares
                ))));

            crate::engine::core::with_scheduling_group::with_scheduling_group(sg, move || {
                // SAFETY: as above.
                let me = unsafe { &mut *this };
                let configs: Vec<(u64, LwSharedPtr<SchedulingGroupKeyConfig>)> = me
                    ._scheduling_group_specific_data
                    .scheduling_group_key_configs
                    .iter()
                    .map(|(k, c)| (*k, c.clone()))
                    .collect();
                for (key_id, cfg) in configs {
                    let v = fut_try!(allocate_scheduling_group_specific_data(sg, key_id, &cfg));
                    store_specific_val(me.get_sg_data(&sg), key_id, v);
                }
                make_ready_future(())
            })
        };
        // SAFETY: the mutex is a field of the reactor, which never moves and outlives its scheduling-group operations.
        unsafe {
            crate::engine::core::shared_mutex::with_shared(
                &mut self._scheduling_group_keys_mutex,
                func,
            )
        }
    }

    fn init_new_scheduling_group_key(
        &mut self,
        key: SchedulingGroupKey,
        cfg: SchedulingGroupKeyConfig,
    ) -> Future<()> {
        let this = self as *mut Reactor;
        let func = move || {
            // SAFETY: the reactor outlives its scheduling-group operations.
            let me = unsafe { &mut *this };
            let key_id = crate::engine::core::scheduling::internal::scheduling_group_key_id(key);
            let cfgp = crate::engine::core::shared_ptr::make_lw_shared(cfg);
            me._scheduling_group_specific_data
                .scheduling_group_key_configs
                .insert(key_id, cfgp.clone());
            crate::engine::core::loop_::parallel_for_each(
                0..me._task_queues.len(),
                move |idx: usize| {
                    // SAFETY: as above.
                    let me = unsafe { &mut *this };
                    let Some(tq) = me._task_queues[idx].as_ref().map(|n| n.as_ptr()) else {
                        return make_ready_future(());
                    };
                    // SAFETY: the queue is live while its node is; one field read.
                    let sg = SchedulingGroup::from_id(unsafe { (*tq)._id } as u32);
                    if std::ptr::eq(tq, me._at_destroy_tasks) {
                        // fake the group by assuming it here
                        let curr = current_scheduling_group();
                        let _cleanup = crate::engine::util::defer::defer(move || {
                            set_current_scheduling_group(curr)
                        });
                        set_current_scheduling_group(sg);
                        let v =
                            fut_try!(allocate_scheduling_group_specific_data(sg, key_id, &cfgp));
                        store_specific_val(me.get_sg_data(&sg), key_id, v);
                    } else {
                        let cfgp = cfgp.clone();
                        return crate::engine::core::with_scheduling_group::with_scheduling_group(
                            sg,
                            move || {
                                // SAFETY: as above.
                                let me = unsafe { &mut *this };
                                let v = fut_try!(allocate_scheduling_group_specific_data(
                                    sg, key_id, &cfgp
                                ));
                                store_specific_val(me.get_sg_data(&sg), key_id, v);
                                make_ready_future(())
                            },
                        );
                    }
                    make_ready_future(())
                },
            )
        };
        // SAFETY: the mutex is a field of the reactor, which never moves and outlives its scheduling-group operations.
        unsafe {
            crate::engine::core::shared_mutex::with_lock(
                &mut self._scheduling_group_keys_mutex,
                func,
            )
        }
    }

    fn destroy_scheduling_group(&mut self, sg: SchedulingGroup) -> Future<()> {
        if sg.id >= max_scheduling_groups() {
            crate::engine::core::on_internal_error::on_fatal_internal_error(
                engine_logger(),
                &format!("Invalid scheduling_group {}", sg.id),
            );
        }
        let this = self as *mut Reactor;
        crate::engine::core::with_scheduling_group::with_scheduling_group(sg, move || {
            // SAFETY: the reactor outlives its scheduling-group operations.
            unsafe { (*this).get_sg_data(&sg).specific_vals.clear() };
            make_ready_future(())
        })
        .then(move |()| {
            // SAFETY: as above.
            let me = unsafe { &mut *this };
            me.get_sg_data(&sg).queue_is_initialized = false;
            me._task_queues[sg.id as usize] = None;
            // The group's I/O classes go with it.
            for queue in me._io_queues.values_mut() {
                // SAFETY: the queue is engaged and reached by this reference only during the call.
                unsafe { queue.get_mut_unchecked() }.destroy_priority_class(
                    crate::engine::core::io_queue::internal::PriorityClass::from(sg),
                );
            }
            make_ready_future(())
        })
    }

    /// Returns the [`Smp`] instance that owns this reactor.
    pub fn smp(&self) -> &Smp {
        &self._smp
    }
}

/// The error for an unknown scheduling group.
pub fn no_such_scheduling_group(sg: SchedulingGroup) -> ExceptionPtr {
    ExceptionPtr::new(crate::engine::core::future::InvalidArgument::new(format!(
        "The scheduling group does not exist ({})",
        crate::engine::core::scheduling::internal::scheduling_group_index(sg)
    )))
}

impl SchedulingGroup {
    /// The group's name: a copy, since the queue that holds the name
    /// can be renamed or destroyed while a reference would still be held.
    pub fn name(&self) -> Sstring {
        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
        let tq = unsafe { (*engine())._task_queues[self.id as usize].as_ref().map(|n| n.as_ptr()) };
        match tq {
            // SAFETY: the queue is live while its node is; one field read.
            Some(tq) => unsafe { (*tq)._name.clone() },
            None => crate::engine::core::on_internal_error::on_fatal_internal_error(
                engine_logger(),
                "scheduling group without task queue",
            ),
        }
    }

    /// The group's short name: a copy (of a few bytes, kept inline by
    /// `Sstring`), for the reason given in [`name`](Self::name).
    pub fn short_name(&self) -> Sstring {
        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
        match unsafe { (*engine())._task_queues[self.id as usize].as_ref().map(|n| n.as_ptr()) } {
            // SAFETY: the queue is live while its node is; one field read.
            Some(task_queue) => unsafe { (*task_queue)._shortname.clone() },
            // we might want to print logging messages before task_queues are ready.
            // pad this string so its length is task_queue->shortname_size
            None => Sstring::from("n/a "),
        }
    }

    /// The group's shares.
    pub fn get_shares(&self) -> f32 {
        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
        // The queue is live while its node is; one field read.
        unsafe {
            (*engine())._task_queues[self.id as usize]
                .as_ref()
                .map_or(0.0, |tq| (*tq.as_ptr())._shares)
        }
    }

    /// Sets the group's shares.
    pub fn set_shares(&self, shares: f32) {
        if let Some(tq) =
            // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
            unsafe {
                (*engine())._task_queues[self.id as usize].as_ref().map(|n| n.as_ptr())
            }
        {
            // SAFETY: the queue is live while its node is; set_shares runs no task, so the
            // `&mut` overlaps no other reference.
            unsafe { (*tq).set_shares(shares) };
        }
        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
        unsafe {
            (*engine()).update_shares_for_queues(
                crate::engine::core::io_queue::internal::PriorityClass::from(*self),
                shares as u32,
            )
        };
    }

    /// Sets the group's I/O bandwidth on every shard.
    pub fn update_io_bandwidth(&self, bandwidth: u64) -> Future<()> {
        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
        unsafe {
            (*engine()).update_bandwidth_for_queues(
                crate::engine::core::io_queue::internal::PriorityClass::from(*self),
                bandwidth,
            )
        }
    }
}

/// Creates a scheduling group on every shard with `name`, `shortname` and `shares`.
pub fn create_scheduling_group(
    name: Sstring,
    shortname: Sstring,
    shares: f32,
) -> Future<SchedulingGroup> {
    let aid = allocate_scheduling_group_id();
    if aid < 0 {
        return make_exception_future(ExceptionPtr::new(
            crate::engine::core::future::RuntimeError::new(format!(
                "Scheduling group limit exceeded while creating {}",
                name
            )),
        ));
    }
    let id = aid as u32;
    engine_assert!(id < max_scheduling_groups());
    let sg = SchedulingGroup::from_id(id);
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    Smp::invoke_on_all(move || unsafe {
        (*engine()).init_scheduling_group(sg, name.clone(), shortname.clone(), shares)
    })
    .then(move |()| make_ready_future(sg))
}

/// Creates a scheduling group with `name` and `shares`.
pub fn create_scheduling_group_with_shares(name: Sstring, shares: f32) -> Future<SchedulingGroup> {
    create_scheduling_group(name, Sstring::new(), shares)
}

/// Creates a scheduling group key on every shard.
pub fn scheduling_group_key_create(cfg: SchedulingGroupKeyConfig) -> Future<SchedulingGroupKey> {
    let key = SchedulingGroupKey::from_id(allocate_scheduling_group_specific_key());
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    Smp::invoke_on_all(move || unsafe {
        (*engine()).init_new_scheduling_group_key(key, cfg.clone())
    })
    .then(move |()| make_ready_future(key))
}

/// Destroys the scheduling group `sg` on every shard.
pub fn destroy_scheduling_group(sg: SchedulingGroup) -> Future<()> {
    if sg == default_scheduling_group() {
        return make_exception_future(
            crate::engine::util::backtrace::make_backtraced_exception_ptr(
                crate::engine::core::future::RuntimeError(String::from(
                    "Attempt to destroy the default scheduling group",
                )),
            ),
        );
    }
    if sg == current_scheduling_group() {
        return make_exception_future(
            crate::engine::util::backtrace::make_backtraced_exception_ptr(
                crate::engine::core::future::RuntimeError(String::from(
                    "Attempt to destroy the current scheduling group",
                )),
            ),
        );
    }
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    Smp::invoke_on_all(move || unsafe { (*engine()).destroy_scheduling_group(sg) }).then(
        move |()| {
            deallocate_scheduling_group_id(sg.id);
            make_ready_future(())
        },
    )
}

/// Renames the scheduling group `sg` on every shard.
pub fn rename_scheduling_group(sg: SchedulingGroup, new_name: Sstring) -> Future<()> {
    rename_scheduling_group_with_shortname(sg, new_name, Sstring::new())
}

/// Renames the scheduling group `sg` and sets its short name on every shard.
pub fn rename_scheduling_group_with_shortname(
    sg: SchedulingGroup,
    new_name: Sstring,
    new_shortname: Sstring,
) -> Future<()> {
    if sg == default_scheduling_group() {
        return make_exception_future(
            crate::engine::util::backtrace::make_backtraced_exception_ptr(
                crate::engine::core::future::RuntimeError(String::from(
                    "Attempt to rename the default scheduling group",
                )),
            ),
        );
    }
    Smp::invoke_on_all(move || {
        if let Some(tq) =
            // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
            unsafe { (*engine())._task_queues[sg.id as usize].as_ref().map(|n| n.as_ptr()) }
        {
            // SAFETY: the queue is live while its node is; rename runs no task, so the `&mut`
            // overlaps no other reference.
            crate::engine::core::reactor::fut_try!(unsafe {
                (*tq).rename(new_name.clone(), new_shortname.clone())
            });
        }
        // The stages of the group are named after it: they follow the rename.
        // SAFETY: the manager of this thread is live; the reference ends with the call.
        unsafe {
            (*crate::engine::core::execution_stage::internal::ExecutionStageManager::get())
                .update_scheduling_group_name(sg)
        };
        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
        unsafe {
            (*engine()).rename_queues(
                crate::engine::core::io_queue::internal::PriorityClass::from(sg),
                new_name.clone(),
            )
        };
        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
        unsafe { (*engine()).rename_scheduling_group_specific_data(sg) }
    })
}

/// Runs a future to completion without anyone waiting for it.
pub use internal_misc::run_in_background;

/// Reactor services used by other modules.
pub mod internal_misc {
    use super::*;

    /// Adds the output stream `os` to the batched flush list.
    ///
    /// # Safety
    /// As [`BatchFlushList::push_back`](crate::engine::core::iostream::BatchFlushList::push_back).
    pub unsafe fn add_to_flush_poller(
        os: *mut crate::engine::core::iostream::OutputStreamState<u8>,
    ) {
        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only
        // (see `engine()`); the list's contract is forwarded.
        unsafe { (*engine())._flush_batching.push_back(os) };
    }

    /// A time value (`struct timeval`).
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct Timeval {
        /// Seconds.
        pub tv_sec: i64,
        /// Microseconds.
        pub tv_usec: i64,
    }

    /// Resource usage (`struct rusage`).
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct Rusage {
        /// User CPU time.
        pub ru_utime: Timeval,
        /// System CPU time.
        pub ru_stime: Timeval,
        /// Maximum resident set size.
        pub ru_maxrss: i64,
        /// Shared memory size.
        pub ru_ixrss: i64,
        /// Unshared data size.
        pub ru_idrss: i64,
        /// Unshared stack size.
        pub ru_isrss: i64,
        /// Minor page faults.
        pub ru_minflt: i64,
        /// Major page faults.
        pub ru_majflt: i64,
        /// Swaps.
        pub ru_nswap: i64,
        /// Block input operations.
        pub ru_inblock: i64,
        /// Block output operations.
        pub ru_oublock: i64,
        /// Messages sent.
        pub ru_msgsnd: i64,
        /// Messages received.
        pub ru_msgrcv: i64,
        /// Signals received.
        pub ru_nsignals: i64,
        /// Voluntary context switches.
        pub ru_nvcsw: i64,
        /// Involuntary context switches.
        pub ru_nivcsw: i64,
    }

    const RUSAGE_THREAD: i32 = 1;

    unsafe extern "C" {
        fn getrusage(who: i32, usage: *mut Rusage) -> i32;
    }

    #[inline]
    /// Converts a `timeval` to a duration.
    pub fn timeval_to_duration(tv: Timeval) -> SchedDuration {
        SchedDuration::from_std(
            Duration::from_secs(tv.tv_sec as u64) + Duration::from_micros(tv.tv_usec as u64),
        )
    }

    /// A poller that measures reactor stalls (for [`report_reactor_stalls`]).
    pub struct ReactorStallSampler {
        _run_start: SchedTimePoint,
        _run_start_rusage: Rusage,
        _kernel_stalls: u64,
        _nonsleep_cpu_time: SchedDuration,
        _nonsleep_wall_time: SchedDuration,
    }

    /// Stall statistics.
    pub use crate::engine::core::stall_sampler::internal::StallReport;

    impl Default for ReactorStallSampler {
        fn default() -> Self {
            Self::new()
        }
    }

    impl ReactorStallSampler {
        fn get_rusage() -> Rusage {
            let mut ru = Rusage::default();
            // SAFETY: `ru` is a valid rusage.
            unsafe { getrusage(RUSAGE_THREAD, &mut ru) };
            ru
        }
        fn cpu_time(ru: &Rusage) -> SchedDuration {
            timeval_to_duration(ru.ru_stime) + timeval_to_duration(ru.ru_utime)
        }
        fn mark_run_start(&mut self) {
            self._run_start = Reactor::now();
            self._run_start_rusage = Self::get_rusage();
        }
        fn mark_run_end(&mut self) {
            let start_nvcsw = self._run_start_rusage.ru_nvcsw;
            let start_cpu_time = Self::cpu_time(&self._run_start_rusage);
            let start_time = self._run_start;
            self._run_start = Reactor::now();
            self._run_start_rusage = Self::get_rusage();
            self._kernel_stalls += (self._run_start_rusage.ru_nvcsw - start_nvcsw) as u64;
            self._nonsleep_cpu_time += Self::cpu_time(&self._run_start_rusage) - start_cpu_time;
            self._nonsleep_wall_time += self._run_start - start_time;
        }
        /// A sampler with fresh counters.
        pub fn new() -> Self {
            let mut s = ReactorStallSampler {
                _run_start: SchedTimePoint::default(),
                _run_start_rusage: Rusage::default(),
                _kernel_stalls: 0,
                _nonsleep_cpu_time: SchedDuration::default(),
                _nonsleep_wall_time: SchedDuration::default(),
            };
            s.mark_run_start();
            s
        }
        /// The statistics so far; marks the run end and start.
        pub fn report(&mut self) -> StallReport {
            // mark_run_end() with an immediate mark_run_start() is logically a no-op,
            // but each one of them has an effect, so they can't be marked const
            self.mark_run_end();
            let r = StallReport {
                kernel_stalls: self._kernel_stalls,
                run_wall_time: self._nonsleep_wall_time.to_std(),
                stall_time: (self._nonsleep_wall_time - self._nonsleep_cpu_time).to_std(),
            };
            self.mark_run_start();
            r
        }
    }

    impl Pollfn for ReactorStallSampler {
        fn poll(&mut self) -> bool {
            false
        }
        fn pure_poll(&mut self) -> bool {
            false
        }
        fn try_enter_interrupt_mode(&mut self) -> bool {
            // try_enter_interrupt_mode marks the end of a reactor run that should be context-switch free
            self.mark_run_end();
            true
        }
        fn exit_interrupt_mode(&mut self) {
            // start a reactor run that should be context switch free
            self.mark_run_start();
        }
    }

    /// Runs `uut` to completion and reports any reactor stalls it generated.
    pub fn report_reactor_stalls(
        uut: NoncopyableFunction<fn() -> Future<()>>,
    ) -> Future<StallReport> {
        let mut uut = uut;
        let poller = Poller::new(ReactorStallSampler::new());
        let p_reporter = poller.pollfn_ptr().cast::<ReactorStallSampler>();
        uut.call().then(move |()| {
            let _poller = poller;
            // SAFETY: the sampler is owned by `_poller`, alive in this scope.
            make_ready_future(unsafe { (*p_reporter).report() })
        })
    }

    /// Formats the report as `N stalls, X ms stall time, Y ms run time`.
    impl std::fmt::Display for StallReport {
        fn fmt(&self, os: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let to_ms = |d: Duration| -> f32 { d.as_secs_f32() * 1000.0 };
            write!(
                os,
                "{} stalls, {} ms stall time, {} ms run time",
                self.kernel_stalls,
                to_ms(self.stall_time),
                to_ms(self.run_wall_time)
            )
        }
    }

    /// Runs `f` to completion without anyone waiting for it.
    pub fn run_in_background(f: Future<()>) {
        // SAFETY: on a reactor thread; the reference lasts for the call, which only queues
        // the future behind the background gate.
        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
        unsafe { (*engine()).run_in_background(f) };
    }

    /// Logs an exception thrown by a timer callback.
    pub fn log_timer_callback_exception(ex: ExceptionPtr) {
        engine_logger().error(
            crate::engine::util::log::FormatInfo::new("Timer callback failed: {}"),
            format_args!(
                "Timer callback failed: {}",
                crate::engine::util::log::DisplayExceptionPtr(Some(&ex))
            ),
        );
    }
}

fn scheduling_group_count_impl() -> usize {
    let b = S_USED_SCHEDULING_GROUP_IDS_BITMAP.load(Ordering::Relaxed);
    b.count_ones() as usize
}

#[cfg(engine_task_backtrace)]
impl crate::engine::core::task::TaskBase {
    /// Records the creation backtrace of the task.
    pub fn make_backtrace(&mut self) {
        let _dbt = memory::DisableBacktraceTemporarily::new();
        self.bt = crate::engine::core::shared_ptr::make_lw_shared(
            crate::engine::util::backtrace::current_backtrace_tasklocal(),
        );
    }
}
