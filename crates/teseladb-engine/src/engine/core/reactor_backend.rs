//! The reactor's I/O backends.
//!
//! A backend is how a shard's reactor talks to the kernel: it submits disk and network I/O,
//! reaps completions, waits for events when the reactor sleeps, and drives the task-quota
//! timer that sets the preemption flag ([`need_preempt`]). Three backends implement the same
//! contract and are chosen at start-up by [`ReactorBackendSelector`]:
//!
//! * `linux-aio` ([`ReactorBackendAio`]): linux-aio for disk I/O and for polling descriptors,
//!   with a separate preempting context whose completion ring doubles as the preemption
//!   monitor;
//! * `epoll` ([`ReactorBackendEpoll`]): epoll for descriptors, timerfds for timers and a timer
//!   thread for the task quota, with linux-aio for disk I/O;
//! * `io_uring` (the `uring` module): one io_uring ring for all I/O.
//!
//! Everything here runs on the reactor thread except the signal-handler hooks of
//! [`ReactorBackend`], which only touch atomics. Objects handed to the kernel start with a
//! [`KernelCompletion`] header and are identified by a thin pointer. Backends are reached
//! through [`BackendEntries`], a table of functions taking the backend's root pointer, because
//! completions may re-enter the backend.
use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering, compiler_fence};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::engine::core::future::{
    ExceptionPtr, Future, LogicError, Promise, RuntimeError, SystemError, do_until,
    make_exception_future, make_ready_future,
};
use crate::engine::core::internal::buffer_allocator::BufferAllocator;
use crate::engine::core::internal::io_desc::{KernelCompletion, KernelCompletionImpl};
use crate::engine::core::internal::io_request::{IoRequest, Operation};
use crate::engine::core::internal::pollable_fd::{
    PollableFd, PollableFdState, PollableFdStatePtr, Speculation,
};
use crate::engine::core::internal::uname::kernel_uname;
use crate::engine::core::linux_aio::internal::linux_abi::{AioContextT, IoEvent, Iocb};
use crate::engine::core::linux_aio::internal::{
    self as aio, get_iocb, get_user_data, get_user_data_iocb, io_destroy, io_getevents,
    io_pgetevents, io_submit, make_fdsync_iocb, make_poll_iocb, make_read_iocb, make_readv_iocb,
    make_write_iocb, make_writev_iocb, set_eventfd_notification, set_nowait, set_user_data,
    setup_aio_context,
};
use crate::engine::core::posix::{self, FileDesc, Itimerspec, Timespec, make_sigset_mask};
use crate::engine::core::preempt::internal::PreemptionMonitor;
use crate::engine::core::preempt::need_preempt;
use crate::engine::core::reactor::internal_preempt::set_need_preempt_var;
use crate::engine::core::reactor::{
    IoCompletion, IoCompletionImpl, Reactor, SigsetT, engine_logger, hrtimer_signal,
};
use crate::engine::core::syscall_result::{SyscallResult, wrap_syscall};
use crate::engine::core::temporary_buffer::TemporaryBuffer;
use crate::engine::net::api::SocketAddress;
use crate::engine::net::node_ptr::NodePtr;
use crate::engine::net::packet::Packet;
use crate::engine::util::internal::iovec_utils::iovec_len;
use crate::engine::util::log::{DisplayExceptionPtr, FormatInfo};
use crate::engine::util::read_first_line::read_first_line_as;

// Linux constants and system calls used by the backends.

/// `poll(2)` event: data to read.
pub const POLLIN: i32 = 0x001;
/// `poll(2)` event: urgent data to read.
pub const POLLPRI: i32 = 0x002;
/// `poll(2)` event: writing is possible.
pub const POLLOUT: i32 = 0x004;
/// `poll(2)` event: error condition.
pub const POLLERR: i32 = 0x008;
/// `poll(2)` event: hang up.
pub const POLLHUP: i32 = 0x010;
/// `poll(2)` event: the peer closed its writing side.
pub const POLLRDHUP: i32 = 0x2000;

/// epoll event: data to read.
pub const EPOLLIN: u32 = 0x001;
/// epoll event: writing is possible.
pub const EPOLLOUT: u32 = 0x004;
/// epoll event: error condition.
pub const EPOLLERR: u32 = 0x008;
/// epoll event: hang up.
pub const EPOLLHUP: u32 = 0x010;
/// epoll event: the peer closed its writing side.
pub const EPOLLRDHUP: u32 = 0x2000;
/// `epoll_ctl` operation: add a descriptor.
pub const EPOLL_CTL_ADD: i32 = 1;
/// `epoll_ctl` operation: remove a descriptor.
pub const EPOLL_CTL_DEL: i32 = 2;
/// `epoll_ctl` operation: change a descriptor's events.
pub const EPOLL_CTL_MOD: i32 = 3;
/// `epoll_create1` flag: close on exec.
pub const EPOLL_CLOEXEC: i32 = 0o2000000;

/// errno: interrupted system call.
pub const EINTR: i32 = 4;
/// errno: bad file descriptor.
pub const EBADF: i32 = 9;
/// errno: try again.
pub const EAGAIN: i32 = 11;
/// errno: the operation would block (same as `EAGAIN`).
pub const EWOULDBLOCK: i32 = EAGAIN;
/// errno: invalid argument.
pub const EINVAL: i32 = 22;
/// errno: operation not supported.
pub const ENOTSUP: i32 = 95;
/// errno: operation in progress.
pub const EINPROGRESS: i32 = 115;
/// errno: operation canceled.
pub const ECANCELED: i32 = 125;
/// errno: timer expired.
pub const ETIME: i32 = 62;
/// errno: function not implemented.
pub const ENOSYS: i32 = 38;
/// errno: device or resource busy.
pub const EBUSY: i32 = 16;

/// The monotonic clock id.
pub const CLOCK_MONOTONIC: i32 = 1;
/// `timerfd_create` flag: close on exec.
pub const TFD_CLOEXEC: i32 = 0o2000000;
/// `timerfd_create` flag: non-blocking.
pub const TFD_NONBLOCK: i32 = 0o4000;
/// `timerfd_settime` flag: absolute expiration time.
pub const TFD_TIMER_ABSTIME: i32 = 1;
/// File status flag: non-blocking.
pub const O_NONBLOCK: i32 = 0o4000;
/// `fcntl` command: get the file status flags.
pub const F_GETFL: i32 = 3;
/// `fcntl` command: set the file status flags.
pub const F_SETFL: i32 = 4;
/// `pthread_sigmask` operation: block signals.
pub const SIG_BLOCK: i32 = 0;
/// `pthread_sigmask` operation: unblock signals.
pub const SIG_UNBLOCK: i32 = 1;
/// `socket` type flag: non-blocking.
pub const SOCK_NONBLOCK: i32 = 0o4000;
/// `socket` type flag: close on exec.
pub const SOCK_CLOEXEC: i32 = 0o2000000;
/// `send` flag: no `SIGPIPE` on a broken connection.
pub const MSG_NOSIGNAL: i32 = 0x4000;
/// `send`/`recv` flag: do not block.
pub const MSG_DONTWAIT: i32 = 0x40;
/// Socket option level of the socket itself.
pub const SOL_SOCKET: i32 = 1;
/// Socket option: pending error.
pub const SO_ERROR: i32 = 4;
/// Resource limit: locked memory.
pub const RLIMIT_MEMLOCK: i32 = 8;

/// The epoll event layout of the running architecture.
pub use crate::engine::core::internal::linux_abi::EpollEvent;

/// A buffer descriptor for vectored I/O (`struct iovec`).
pub use crate::engine::util::internal::iovec_utils::Iovec;

/// A message header for `sendmsg`/`recvmsg` (`struct msghdr`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Msghdr {
    /// Optional address.
    pub msg_name: *mut c_void,
    /// Address size.
    pub msg_namelen: u32,
    /// The buffers.
    pub msg_iov: *mut Iovec,
    /// Number of buffers.
    pub msg_iovlen: usize,
    /// Ancillary data.
    pub msg_control: *mut c_void,
    /// Ancillary data size.
    pub msg_controllen: usize,
    /// Flags of the received message.
    pub msg_flags: i32,
}

impl Default for Msghdr {
    fn default() -> Self {
        Msghdr {
            msg_name: std::ptr::null_mut(),
            msg_namelen: 0,
            msg_iov: std::ptr::null_mut(),
            msg_iovlen: 0,
            msg_control: std::ptr::null_mut(),
            msg_controllen: 0,
            msg_flags: 0,
        }
    }
}

/// Signal information (`siginfo_t`, 128 bytes on Linux); only the common header is named.
#[repr(C)]
pub struct SiginfoT {
    /// Signal number.
    pub si_signo: i32,
    /// errno value associated with the signal.
    pub si_errno: i32,
    /// Signal code.
    pub si_code: i32,
    /// The rest of the structure.
    pub _pad: [i32; 29],
}

/// A resource limit (`struct rlimit`).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Rlimit {
    /// Soft limit.
    pub rlim_cur: u64,
    /// Hard limit.
    pub rlim_max: u64,
}

unsafe extern "C" {
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    fn pthread_sigmask(how: i32, set: *const SigsetT, oldset: *mut SigsetT) -> i32;
    fn epoll_ctl(epfd: i32, op: i32, fd: i32, event: *mut EpollEvent) -> i32;
    fn epoll_pwait(
        epfd: i32,
        events: *mut EpollEvent,
        maxevents: i32,
        timeout: i32,
        sigmask: *const SigsetT,
    ) -> i32;
    fn __errno_location() -> *mut i32;
    fn getrlimit(resource: i32, rlim: *mut Rlimit) -> i32;
    fn pthread_self() -> usize;
    fn syscall(num: i64, ...) -> i64;
    fn _exit(status: i32) -> !;
}

/// The calling thread's `errno`.
#[inline]
pub(crate) fn errno() -> i32 {
    // SAFETY: `__errno_location` returns the thread-local errno of the calling thread.
    unsafe { *__errno_location() }
}

/// Sets the calling thread's `errno`.
#[inline]
pub(crate) fn set_errno(v: i32) {
    // SAFETY: `__errno_location` returns the thread-local errno of the calling thread.
    unsafe { *__errno_location() = v }
}

/// Aborts the process with an assertion message if the condition is false; the optional
/// second argument is a message appended to the report. Active in every build.
macro_rules! engine_assert {
    ($e:expr) => {
        if !($e) {
            crate::engine::util::assert::internal::assert_fail(
                stringify!($e),
                file!(),
                line!() as i32,
                "",
            );
        }
    };
    // With a message.
    ($e:expr, $msg:literal) => {
        if !($e) {
            crate::engine::util::assert::internal::assert_fail(
                concat!(stringify!($e), " && \"", $msg, "\""),
                file!(),
                line!() as i32,
                "",
            );
        }
    };
}
pub(crate) use engine_assert;

/// Handles an error that escapes the reactor's main loop from a backend method called through
/// a poller that has no error path (`kernel_submit_work`): logs it, prints the backtrace and
/// exits the process with status 1.
#[cold]
fn exception_in_reactor_main_loop(e: ExceptionPtr) -> ! {
    engine_logger().error(FormatInfo::new("{}"), format_args!("{}", DisplayExceptionPtr(Some(&e))));
    crate::engine::core::reactor::print_with_backtrace(
        "exception running reactor main loop",
        false,
        false,
    );
    // SAFETY: `_exit` terminates the process.
    unsafe { _exit(1) }
}

/// Handles an error raised by a backend method on a path that cannot fail (enabling the
/// timer, the preemption paths): aborts the process.
#[cold]
fn exception_in_noexcept(_e: ExceptionPtr) -> ! {
    std::process::abort()
}

// Backend types.

/// A linux-aio context with a batch of control blocks waiting to be submitted.
// FIXME: merge it with the storage context below. At this point the main thing to do is
// unify the iocb list.
pub struct AioGeneralContext {
    /// The kernel context.
    pub io_context: AioContextT,
    /// The batch of control blocks to submit.
    pub iocbs: Box<[*mut Iocb]>,
    /// Number of queued control blocks.
    pub last: usize,
    /// Capacity of the batch.
    pub end: usize,
}

/// Maximum number of disk requests in flight in one storage context.
pub const AIO_STORAGE_MAX_AIO: usize = 1024;

/// The control blocks of an [`IocbPool`], cache-line aligned.
#[repr(C, align(64))]
pub struct AllIocbs(pub [Iocb; AIO_STORAGE_MAX_AIO]);

/// A fixed pool of control blocks for disk I/O.
pub struct IocbPool {
    /// The control blocks.
    pub(crate) _all_iocbs: AllIocbs,
    /// The free control blocks, used as a stack; its capacity is fixed at
    /// [`AIO_STORAGE_MAX_AIO`].
    pub(crate) _free_iocbs: Vec<*mut Iocb>,
}

/// A list of control blocks to resubmit, of fixed capacity [`AIO_STORAGE_MAX_AIO`].
pub type PendingAioRetryT = Vec<*mut Iocb>;

/// The linux-aio context for disk I/O, shared by the linux-aio and epoll backends.
pub struct AioStorageContext {
    /// The context's address with the backend's root provenance, written by `init`; the retry
    /// loop keeps it.
    _this: *mut AioStorageContext,
    /// The reactor that owns the backend.
    pub(crate) _r: *mut Reactor,
    /// The kernel context.
    pub(crate) _io_context: AioContextT,
    /// Control blocks prepared for the next `io_submit`.
    pub(crate) _submission_queue: Vec<*mut Iocb>,
    /// The control block pool.
    pub(crate) _iocb_pool: IocbPool,
    /// Control blocks rejected with `EAGAIN`, waiting for a retry.
    pub(crate) _pending_aio_retry: PendingAioRetryT,
    /// Control blocks being retried on the syscall thread.
    pub(crate) _aio_retries: PendingAioRetryT,
    /// The retry in progress (ready when none is).
    pub(crate) _pending_aio_retry_fut: Future<()>,
    /// Set by stop(): no retry loop starts any more, and requests rejected with EAGAIN
    /// complete at once instead of waiting for a retry.
    pub(crate) _stopping: bool,
    /// Buffer for reaped completion events.
    pub(crate) _ev_buffer: [IoEvent; AIO_STORAGE_MAX_AIO],
}

impl AioStorageContext {
    /// Maximum number of disk requests in flight.
    pub const MAX_AIO: usize = AIO_STORAGE_MAX_AIO;

    fn need_to_retry(&self) -> bool {
        !self._pending_aio_retry.is_empty() || !self._aio_retries.is_empty()
    }

    fn retry_in_progress(&self) -> bool {
        !self._pending_aio_retry_fut.available()
    }

    /// The owning reactor, as a raw pointer: the reactor owns the backend and is re-entered
    /// from the completions the backend runs.
    #[inline]
    fn r(&self) -> *mut Reactor {
        self._r
    }
}

/// A control block with a flag telling whether it is currently submitted to a context.
pub struct CompletionWithIocb {
    /// Whether the control block is submitted.
    pub(crate) _in_context: bool,
    /// The control block.
    pub(crate) _iocb: Iocb,
}

/// A kernel completion that reads a descriptor: the completion header, then a pointer to the
/// descriptor it reads.
#[repr(C)]
pub struct FdKernelCompletion {
    /// The completion header.
    pub(crate) base: KernelCompletion,
    /// The descriptor.
    pub(crate) _fd: *mut FileDesc,
}

impl FdKernelCompletion {
    /// A completion of the type `T` reading `fd`.
    ///
    /// # Safety
    /// `fd` points to the descriptor, which outlives the completion, or is null until
    /// [`FdKernelCompletion::rebind`] sets it before the completion is first queued.
    #[inline]
    pub(crate) unsafe fn new<T: KernelCompletionImpl>(fd: *mut FileDesc) -> Self {
        FdKernelCompletion { base: KernelCompletion::new::<T>(), _fd: fd }
    }
    /// Points the completion at its descriptor once the descriptor's owner reached its
    /// final address.
    ///
    /// # Safety
    /// As [`FdKernelCompletion::new`].
    #[inline]
    pub(crate) unsafe fn rebind(&mut self, fd: *mut FileDesc) {
        self._fd = fd;
    }
    /// The descriptor.
    pub fn fd(&mut self) -> &mut FileDesc {
        // SAFETY: the constructor's and rebind's contract: `_fd` is set and live.
        unsafe { &mut *self._fd }
    }
}

#[repr(C)]
/// The completion of the high resolution timer's timerfd poll.
pub struct HrtimerAioCompletion {
    /// Header and descriptor.
    pub(crate) fd_kc: FdKernelCompletion,
    /// The poll control block.
    pub(crate) cwi: CompletionWithIocb,
    /// The reactor, whose expired timers the completion runs.
    pub(crate) _r: *mut Reactor,
}

#[repr(C)]
/// The completion of the task-quota timer's timerfd poll.
pub struct TaskQuotaAioCompletion {
    /// Header and descriptor.
    pub(crate) fd_kc: FdKernelCompletion,
    /// The poll control block.
    pub(crate) cwi: CompletionWithIocb,
}

#[repr(C)]
/// The completion of the inter-shard wake-up eventfd poll.
pub struct SmpWakeupAioCompletion {
    /// Header and descriptor.
    pub(crate) fd_kc: FdKernelCompletion,
    /// The poll control block.
    pub(crate) cwi: CompletionWithIocb,
}

/// The linux-aio context of the preempting events: the task-quota timer and the high
/// resolution timer. Its completion ring is the reactor's preemption monitor, so a completion
/// queued by the kernel makes [`need_preempt`] return `true`.
pub struct PreemptIoContext {
    /// The reactor.
    pub(crate) _r: *mut Reactor,
    /// The kernel context.
    pub(crate) _context: AioGeneralContext,

    /// The task-quota timer completion.
    pub(crate) _task_quota_aio_completion: TaskQuotaAioCompletion,
    /// The high resolution timer completion.
    pub(crate) _hrtimer_aio_completion: HrtimerAioCompletion,
}

impl PreemptIoContext {
    /// Submits the queued control blocks; returns how many.
    pub fn flush(&mut self) -> usize {
        self._context.flush()
    }
}

/// The backend operations that may run kernel completions (or hand the kernel pointers into
/// the backend, or are reached from a completion), called with the backend's root pointer
/// (the address of the `NodePtr` the reactor owns it by, see `ReactorBackendSelector::create`)
/// as their first argument: a raw-pointer receiver for a trait object. The reactor reads the
/// table once, so each call is one indirect call.
///
/// The completions include the high resolution timer's, which runs the expired timers'
/// callbacks at once; those come back into the backend through this
/// table. With io_uring, every operation that takes a submission entry can run completions
/// (`get_sqe` reaps them when the ring is full), so the I/O entries are here too, and take
/// the descriptor state by its root pointer (`PollableFdState::root`), not a reference.
///
/// # Safety (each entry)
/// The pointer is the root pointer of the live backend that gave the table; a descriptor
/// state is the root pointer of a live state made by that backend; no reference to either
/// is live during the call.
pub struct BackendEntries {
    /// Completes the finished asynchronous work without blocking; returns `true` if any.
    pub reap_kernel_completions: unsafe fn(*mut ()) -> bool,
    /// Submits newly produced work without blocking; returns `true` if any.
    pub kernel_submit_work: unsafe fn(*mut ()) -> bool,
    /// Waits (blocking, with the given signal mask) for events and processes them; called
    /// when the reactor goes to sleep.
    pub wait_and_process_events: unsafe fn(*mut (), Option<&SigsetT>),
    /// Clears the preemption request after the reactor polled.
    pub reset_preemption_monitor: unsafe fn(*mut ()),
    /// Makes [`need_preempt`] return `true` now.
    pub request_preemption: unsafe fn(*mut ()),
    // Methods that allow polling on file descriptors. This will only work on
    // the epoll backend. Other backends will probably abort if
    // they are called (which is fine if no file descriptors are waited on):
    /// Resolves when the descriptor is readable.
    pub readable: unsafe fn(*mut (), *mut PollableFdState) -> Future<()>,
    /// Resolves when the descriptor is writeable.
    pub writeable: unsafe fn(*mut (), *mut PollableFdState) -> Future<()>,
    /// Resolves when the descriptor is readable or writeable.
    pub readable_or_writeable: unsafe fn(*mut (), *mut PollableFdState) -> Future<()>,
    /// Resolves when the peer shut down its side.
    pub poll_rdhup: unsafe fn(*mut (), *mut PollableFdState) -> Future<()>,
    /// Unregisters and frees a state made by this backend.
    pub forget: unsafe fn(*mut (), *mut PollableFdState),
    /// Accepts a connection.
    pub accept: unsafe fn(*mut (), *mut PollableFdState) -> Future<(PollableFd, SocketAddress)>,
    /// Connects a socket.
    pub connect: unsafe fn(*mut (), *mut PollableFdState, &mut SocketAddress) -> Future<()>,
    /// Shuts down part of a connection.
    pub shutdown: unsafe fn(*mut (), *mut PollableFdState, i32) -> Result<(), ExceptionPtr>,
    /// Reads into a buffer; the buffer is valid for writes of the length until the future
    /// resolves.
    pub read: unsafe fn(*mut (), *mut PollableFdState, *mut c_void, usize) -> Future<usize>,
    /// Receives into the buffers of an iovec array.
    pub recvmsg: unsafe fn(*mut (), *mut PollableFdState, &Vec<Iovec>) -> Future<usize>,
    /// Reads into a buffer from an allocator, which outlives the read.
    pub read_some: unsafe fn(
        *mut (),
        *mut PollableFdState,
        *mut dyn BufferAllocator,
    ) -> Future<TemporaryBuffer<u8>>,
    /// Sends a packet, which outlives the write.
    pub sendmsg: unsafe fn(*mut (), *mut PollableFdState, &mut Packet) -> Future<usize>,
    /// Sends a buffer; the buffer is valid for reads of the length until the future resolves.
    pub send: unsafe fn(*mut (), *mut PollableFdState, *const c_void, usize) -> Future<usize>,
    /// Receives into a buffer from an allocator, which outlives the read.
    pub recv_some: unsafe fn(
        *mut (),
        *mut PollableFdState,
        *mut dyn BufferAllocator,
    ) -> Future<TemporaryBuffer<u8>>,
    /// Starts the task-quota timer.
    pub start_tick: unsafe fn(*mut ()),
    /// Stops the task-quota timer.
    pub stop_tick: unsafe fn(*mut ()),
    /// Arms the high resolution timer.
    pub arm_highres_timer: unsafe fn(*mut (), &Itimerspec),
    /// Wraps a descriptor in a state made by this backend.
    pub make_pollable_fd_state: unsafe fn(*mut (), FileDesc, Speculation) -> PollableFdStatePtr,
}

/// The [`BackendEntries`] of a backend type whose inherent `unsafe fn(this: *mut Self, ...)`
/// functions have the entries' names.
macro_rules! backend_entries {
    ($ty:ty) => {{
        static ENTRIES: BackendEntries = BackendEntries {
            // SAFETY: each entry is called only through its `unsafe fn` pointer, whose
            // contract (see `BackendEntries`) makes `p` the root pointer of a live `$ty` and
            // each descriptor state the root pointer of a live state it made, with no
            // reference to either live during the call; the entry forwards `p.cast()` and
            // the other arguments unchanged to the `$ty` function of the same name, whose
            // contract is that same one. The same holds for every entry below.
            reap_kernel_completions: |p| unsafe { <$ty>::reap_kernel_completions(p.cast()) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            kernel_submit_work: |p| unsafe { <$ty>::kernel_submit_work(p.cast()) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            wait_and_process_events: |p, m| unsafe { <$ty>::wait_and_process_events(p.cast(), m) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            reset_preemption_monitor: |p| unsafe { <$ty>::reset_preemption_monitor(p.cast()) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            request_preemption: |p| unsafe { <$ty>::request_preemption(p.cast()) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            readable: |p, fd| unsafe { <$ty>::readable(p.cast(), fd) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            writeable: |p, fd| unsafe { <$ty>::writeable(p.cast(), fd) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            readable_or_writeable: |p, fd| unsafe { <$ty>::readable_or_writeable(p.cast(), fd) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            poll_rdhup: |p, fd| unsafe { <$ty>::poll_rdhup(p.cast(), fd) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            forget: |p, fd| unsafe { <$ty>::forget(p.cast(), fd) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            accept: |p, fd| unsafe { <$ty>::accept(p.cast(), fd) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            connect: |p, fd, sa| unsafe { <$ty>::connect(p.cast(), fd, sa) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            shutdown: |p, fd, how| unsafe { <$ty>::shutdown(p.cast(), fd, how) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            read: |p, fd, b, n| unsafe { <$ty>::read(p.cast(), fd, b, n) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            recvmsg: |p, fd, iov| unsafe { <$ty>::recvmsg(p.cast(), fd, iov) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            read_some: |p, fd, ba| unsafe { <$ty>::read_some(p.cast(), fd, ba) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            sendmsg: |p, fd, pk| unsafe { <$ty>::sendmsg(p.cast(), fd, pk) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            send: |p, fd, b, n| unsafe { <$ty>::send(p.cast(), fd, b, n) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            recv_some: |p, fd, ba| unsafe { <$ty>::recv_some(p.cast(), fd, ba) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            start_tick: |p| unsafe { <$ty>::start_tick(p.cast()) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            stop_tick: |p| unsafe { <$ty>::stop_tick(p.cast()) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            arm_highres_timer: |p, its| unsafe { <$ty>::arm_highres_timer(p.cast(), its) },
            // SAFETY: as for the first entry: the table's contract is the callee's.
            make_pollable_fd_state: |p, fd, sp| unsafe {
                <$ty>::make_pollable_fd_state(p.cast(), fd, sp)
            },
        };
        &ENTRIES
    }};
}

/// The interface a reactor backend implements: how a shard waits for basic events. There is
/// an implementation based on epoll and file descriptors ([`ReactorBackendEpoll`]), one based
/// on linux-aio ([`ReactorBackendAio`]), and one based on io_uring.
///
/// Communication with the kernel goes through [`BackendEntries`]: `reap_kernel_completions`
/// completes any previous asynchronous work that is ready to consume, `kernel_submit_work`
/// submits new events that were produced (both never block), and `wait_and_process_events`
/// may block and is called when the reactor is about to sleep. Those run kernel completions,
/// among them the high resolution timer's, which runs the expired timers' callbacks at once;
/// those may come back into the backend. So they, the I/O methods (which with io_uring may run
/// completions too) and the methods the timers' callbacks reach take the backend as its root
/// pointer and keep no reference to it across a completion: they are the entries of the
/// table. The trait keeps the methods that only read the backend.
pub trait ReactorBackend {
    /// The entries that take the backend by its root pointer.
    fn entries(&self) -> &'static BackendEntries;
    /// Whether the kernel can wake the reactor while it sleeps (so it may sleep in
    /// `wait_and_process_events`).
    fn kernel_events_can_sleep(&self) -> bool;

    /// Whether disk I/O must be done with blocking calls on the syscall thread.
    fn do_blocking_io(&self) -> bool {
        false
    }
    /// Called from the signal handler: implementations only touch atomics.
    fn signal_received(&self, signo: i32, siginfo: *mut SiginfoT, ignore: *mut c_void);
    /// Called from the signal handler: implementations only touch atomics.
    fn start_handling_signal(&self);
}

/// A backend using file descriptors and epoll, suitable for running on Linux. It can wait on
/// multiple file descriptors, and converts other events (such as timers, signals and
/// inter-thread notifications) into file descriptors using timerfd, signalfd and eventfd.
pub struct ReactorBackendEpoll {
    /// The reactor that owns the backend.
    pub(crate) _r: *mut Reactor,
    /// Set by the timer thread when the high resolution timer expired.
    pub(crate) _highres_timer_pending: AtomicBool,
    /// The thread that drives the task-quota timer.
    pub(crate) _task_quota_timer_thread: Option<JoinHandle<()>>,
    /// The armed high resolution timer deadline.
    pub(crate) _steady_clock_timer_deadline: Itimerspec,
    // These two timers are used for high resolution timer<>s, one for
    // the reactor thread (when sleeping) and one for the timer thread
    // (when awake). We can't use one timer because of races between the
    // timer thread and reactor thread.
    //
    // Only one of the two is active at any time.
    /// The high resolution timer used while the reactor sleeps.
    pub(crate) _steady_clock_timer_reactor_thread: FileDesc,
    /// The high resolution timer used by the timer thread while the reactor runs.
    pub(crate) _steady_clock_timer_timer_thread: FileDesc,
    /// The epoll instance.
    pub(crate) _epollfd: FileDesc,
    /// The disk I/O context.
    pub(crate) _storage_context: AioStorageContext,
    /// Whether epoll must be checked on the next poll.
    pub(crate) _need_epoll_events: bool,
}

/// A backend using linux-aio for disk I/O, descriptor polling and timers.
pub struct ReactorBackendAio {
    /// The reactor that owns the backend.
    pub(crate) _r: *mut Reactor,
    /// The high resolution timer's timerfd.
    pub(crate) _hrtimer_timerfd: FileDesc,
    /// The disk I/O context.
    pub(crate) _storage_context: AioStorageContext,
    // We use two aio contexts, one for preempting events (the timer tick and
    // signals), the other for non-preempting events (fd poll).
    /// Used for the timer tick and the high resolution timer.
    pub(crate) _preempting_io: PreemptIoContext,
    /// Used for descriptor polls.
    // FIXME: unify with the disk aio context.
    pub(crate) _polling_io: AioGeneralContext,
    /// The high resolution timer's completion in the polling context (used while sleeping).
    pub(crate) _hrtimer_poll_completion: HrtimerAioCompletion,
    /// The inter-shard wake-up completion.
    pub(crate) _smp_wakeup_aio_completion: SmpWakeupAioCompletion,
}

#[derive(Clone, Debug)]
/// The name of a reactor backend (`linux-aio`, `epoll` or `io_uring`), as selected by the
/// `reactor-backend` option; [`create`](Self::create) builds the backend.
pub struct ReactorBackendSelector {
    _name: String,
}

impl ReactorBackendSelector {
    fn new(name: String) -> Self {
        ReactorBackendSelector { _name: name }
    }
    /// The backend's name.
    pub fn name(&self) -> &String {
        &self._name
    }
}

impl std::fmt::Display for ReactorBackendSelector {
    fn fmt(&self, os: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(os, "{}", self._name)
    }
}

// Backend implementations.

/// A kernel completion that resolves a promise when a descriptor poll completes.
#[repr(C)]
pub struct PollableFdStateCompletion {
    base: KernelCompletion,
    _pr: Promise<()>,
}

impl Default for PollableFdStateCompletion {
    #[inline]
    fn default() -> Self {
        PollableFdStateCompletion { base: KernelCompletion::new::<Self>(), _pr: Promise::new() }
    }
}

// SAFETY: `#[repr(C)]` with the header built for `Self` at offset 0.
unsafe impl KernelCompletionImpl for PollableFdStateCompletion {
    unsafe fn complete_with(this: *mut Self, _res: isize) {
        // SAFETY: the trait's contract: `this` is live and no reference to it is live;
        // `me` is not used after the object is destroyed.
        let me = unsafe { &mut *this };
        me._pr.set_value(());
        // The promise is left with no future and no task once it resolves its waiter, so the
        // next poll of the same fd calls `get_future()` on it again; this promise keeps the
        // node it resolved, so the resolved one is replaced by a fresh promise here (its
        // future already holds the value).
        me._pr = Promise::new();
    }
}

impl PollableFdStateCompletion {
    /// The future resolved by the next completion.
    pub fn get_future(&mut self) -> Future<()> {
        self._pr.get_future()
    }
}

/// Prepares the linux-aio control block `iocb` for `req`, with `desc` as its completion.
///
/// Logs an error and aborts the process on an operation this function cannot prepare (only
/// reads, writes and `fdatasync` are supported).
pub fn prepare_iocb(req: &IoRequest, desc: *mut IoCompletion, iocb: &mut Iocb) {
    match req.opcode() {
        Operation::Fdatasync => {
            *iocb = make_fdsync_iocb(req.as_fdatasync().fd);
        }
        Operation::Write => {
            let op = req.as_write();
            *iocb = make_write_iocb(op.fd, op.pos, op.addr.cast(), op.size);
            set_nowait(iocb, op.nowait_works);
        }
        Operation::Writev => {
            let op = req.as_writev();
            *iocb = make_writev_iocb(op.fd, op.pos, op.iovec, op.iov_len);
            set_nowait(iocb, op.nowait_works);
        }
        Operation::Read => {
            let op = req.as_read();
            *iocb = make_read_iocb(op.fd, op.pos, op.addr.cast(), op.size);
            set_nowait(iocb, op.nowait_works);
        }
        Operation::Readv => {
            let op = req.as_readv();
            *iocb = make_readv_iocb(op.fd, op.pos, op.iovec, op.iov_len);
            set_nowait(iocb, op.nowait_works);
        }
        _ => {
            engine_logger().error(
                FormatInfo::new("Invalid operation for iocb: {}"),
                format_args!("Invalid operation for iocb: {}", req.opname()),
            );
            std::process::abort();
        }
    }
    set_user_data(iocb, IoCompletion::as_kernel_completion(desc));
}

/// Appends to a vector reserved with a fixed capacity that never grows, writing the slot
/// without the capacity check and growth path of `Vec::push`.
///
/// # Safety
/// `v.len() < v.capacity()`.
#[inline(always)]
unsafe fn static_vector_push<T>(v: &mut Vec<T>, x: T) {
    debug_assert!(v.len() < v.capacity());
    // SAFETY: the contract above: the slot at `len` is inside the allocation and unused.
    unsafe {
        v.as_mut_ptr().add(v.len()).write(x);
        v.set_len(v.len() + 1);
    }
}

impl Default for IocbPool {
    fn default() -> Self {
        Self::new()
    }
}

impl IocbPool {
    /// An empty pool, before it has its final address: the free list is filled by
    /// [`IocbPool::init`].
    pub fn new() -> Self {
        IocbPool {
            // SAFETY: `Iocb` is a plain C struct for which all-zero bytes is a valid value.
            _all_iocbs: unsafe { std::mem::zeroed() },
            _free_iocbs: Vec::with_capacity(AIO_STORAGE_MAX_AIO),
        }
    }

    /// Fills the free list, once the pool will no longer move.
    ///
    /// # Safety
    /// `this` is the live pool at its final address, derived from the backend's root pointer:
    /// the free list (and the kernel) keep pointers derived from it.
    pub unsafe fn init(this: *mut Self) {
        for i in 0..AIO_STORAGE_MAX_AIO {
            // SAFETY: the contract above; `i` is in bounds, and no reference is formed to
            // the array.
            unsafe {
                let io = std::ptr::addr_of_mut!((*this)._all_iocbs.0[i]);
                (*this)._free_iocbs.push(io);
            }
        }
    }

    /// Takes a free control block (the kernel keeps the pointer).
    ///
    /// # Safety
    /// The pool has capacity ([`has_capacity`](Self::has_capacity)).
    #[inline]
    pub unsafe fn get_one(&mut self) -> *mut Iocb {
        debug_assert!(self.has_capacity());
        // SAFETY: the contract above; every pointer in the free list points into
        // `_all_iocbs` with the pool's provenance (see `init`).
        unsafe { self._free_iocbs.pop().unwrap_unchecked() }
    }

    /// Returns a control block to the pool.
    ///
    /// # Safety
    /// `io` is an iocb of this pool taken by `get_one` and not yet returned, so the free list
    /// has room.
    #[inline]
    pub unsafe fn put_one(&mut self, io: *mut Iocb) {
        // SAFETY: the contract above.
        unsafe { static_vector_push(&mut self._free_iocbs, io) };
    }

    #[inline]
    /// Number of control blocks in use.
    pub fn outstanding(&self) -> u32 {
        (AIO_STORAGE_MAX_AIO - self._free_iocbs.len()) as u32
    }

    #[inline]
    /// Whether a control block is free.
    pub fn has_capacity(&self) -> bool {
        !self._free_iocbs.is_empty()
    }
}

impl AioStorageContext {
    /// Creates the context, up to the point where the object needs its final address;
    /// [`AioStorageContext::init`] completes it.
    ///
    /// # Errors
    ///
    /// Returns the error of setting up the linux-aio context.
    ///
    /// # Safety
    ///
    /// `r` is the reactor that owns the backend holding the context, which outlives it.
    pub unsafe fn new(r: *mut Reactor) -> Result<Self, ExceptionPtr> {
        const _: () = assert!(
            AIO_STORAGE_MAX_AIO >= (Reactor::MAX_QUEUES * Reactor::MAX_QUEUES) as usize,
            "Mismatch between maximum allowed io and what the IO queues can produce"
        );
        let mut this = AioStorageContext {
            _this: std::ptr::null_mut(),
            _r: r,
            _io_context: 0,
            _submission_queue: Vec::with_capacity(AIO_STORAGE_MAX_AIO),
            _iocb_pool: IocbPool::new(),
            _pending_aio_retry: Vec::with_capacity(AIO_STORAGE_MAX_AIO),
            _aio_retries: Vec::with_capacity(AIO_STORAGE_MAX_AIO),
            _pending_aio_retry_fut: make_ready_future(()),
            _stopping: false,
            // SAFETY: `IoEvent` is a plain C struct for which all-zero bytes is a valid value.
            _ev_buffer: unsafe { std::mem::zeroed() },
        };
        setup_aio_context(AIO_STORAGE_MAX_AIO as u32, &mut this._io_context)?;
        Ok(this)
    }

    /// Completes construction once the context is at its final address (inside the
    /// heap-allocated backend): fills the iocb pool and registers `stop()` at exit.
    ///
    /// # Safety
    /// `p` is the live context at its final address, derived from the backend's root
    /// pointer: the pool and the exit task keep it.
    pub unsafe fn init(p: *mut Self) {
        // SAFETY: the contract above; the context lives in the backend, which outlives the
        // exit tasks of the reactor.
        unsafe {
            (*p)._this = p;
            IocbPool::init(std::ptr::addr_of_mut!((*p)._iocb_pool));
            (*(*p)._r)
                .do_at_exit(crate::engine::util::noncopyable_function::NoncopyableFunction::<
                fn() -> Future<()>,
            >::new(move || Self::stop(p)));
        }
    }

    /// # Safety
    /// As [`AioStorageContext::init`]; the continuations keep `this`.
    pub unsafe fn stop(this: *mut Self) -> Future<()> {
        // SAFETY: the contract above; short accesses to the live context.
        let fut = unsafe {
            // Set _stopping first so that any reap_completions() and schedule_retry() calls
            // that race with the drain below see the right state: a second retry loop
            // started now would race with this drain on the same control blocks.
            (*this)._stopping = true;
            // Fail the requests still in the I/O sink (they have no control block yet).
            let sink = std::ptr::addr_of_mut!((*(*this).r())._io_sink);
            (*sink).drain(|_req: &IoRequest, desc: *mut IoCompletion| -> bool {
                // The completion lives until it is completed (inside the unsafe block above).
                KernelCompletion::complete_with(
                    IoCompletion::as_kernel_completion(desc),
                    -(ECANCELED as isize),
                );
                true
            });
            std::mem::replace(&mut (*this)._pending_aio_retry_fut, make_ready_future(()))
        };
        fut.finally(move || {
            // SAFETY: the storage context outlives the exit tasks of the reactor.
            do_until(
                move || unsafe { (*this)._iocb_pool.outstanding() == 0 },
                move || {
                    // SAFETY: as above, `this` is the live context, which outlives the exit
                    // tasks; this shard's thread is the only user, and no reference to the
                    // context is held across these calls.
                    unsafe {
                        (*this).reap_completions(false);
                        (*this).reap_pending_retries();
                    }
                    make_ready_future(())
                },
            )
        })
    }

    /// Completes the control blocks waiting for a retry with `-ECANCELED` and returns them
    /// to the pool. Called while stopping, once no retry loop runs.
    fn reap_pending_retries(&mut self) {
        let pending = std::mem::take(&mut self._pending_aio_retry);
        // _aio_retries is empty in the normal call path: this runs after the retry loop's
        // future has resolved, and that loop only ends when both lists are empty. The drain
        // is defensive, so that no control block leaks.
        let retries = std::mem::take(&mut self._aio_retries);
        for iocb in pending.iter().chain(retries.iter()).copied() {
            // SAFETY: `iocb` is one of ours, taken from the pool and prepared with a
            // completion as user data; it is in no other list.
            let desc = unsafe { get_user_data_iocb(&*iocb) };
            // SAFETY: as above.
            unsafe { self._iocb_pool.put_one(iocb) };
            // SAFETY: the completion lives until it is completed.
            unsafe { KernelCompletion::complete_with(desc, -(ECANCELED as isize)) };
        }
        // Keep the lists' storage (allocated once, with room for every control block).
        self._pending_aio_retry = pending;
        self._pending_aio_retry.clear();
        self._aio_retries = retries;
        self._aio_retries.clear();
    }

    // Returns: number of iocbs consumed (0 or 1)
    fn handle_aio_error(&mut self, iocb: *mut Iocb, ec: i32) -> Result<usize, ExceptionPtr> {
        match ec {
            EAGAIN => Ok(0),
            EBADF => {
                // SAFETY: `iocb` is one of ours, taken from the pool and prepared with a
                // completion as user data.
                let desc = unsafe { get_user_data_iocb(&*iocb) };
                // SAFETY: as above.
                unsafe { self._iocb_pool.put_one(iocb) };
                // SAFETY: the completion lives until it is completed.
                unsafe { KernelCompletion::complete_with(desc, -(EBADF as isize)) };
                // if EBADF, it means that the first request has a bad fd, so
                // we will only remove it from _pending_io and try again.
                Ok(1)
            }
            // happens when the filesystem does not implement aio read or write
            EINVAL | ENOTSUP => {
                engine_logger().error(FormatInfo::new("io_submit failed: this happens when accessing filesystem which does not supports asynchronous direct I/O"), format_args!(
                    "io_submit failed: this happens when accessing filesystem which does not supports asynchronous direct I/O"
                ));
                // SAFETY: as above.
                let desc = unsafe { get_user_data_iocb(&*iocb) };
                // SAFETY: as above.
                unsafe { self._iocb_pool.put_one(iocb) };
                // SAFETY: as above.
                unsafe { KernelCompletion::complete_with(desc, -(ENOTSUP as isize)) };
                Ok(1)
            }
            _ => {
                // SAFETY: `_r` is the reactor that owns this backend; a short access to a counter.
                unsafe { (*self.r())._io_stats.aio_errors += 1 };
                Err(ExceptionPtr::from(SystemError::new(ec, "io_submit")))
            }
        }
    }

    /// Submits the queued disk requests from the I/O sink to the kernel; requests rejected
    /// with `EAGAIN` are scheduled for retry. Returns `Ok(true)` if any work was done.
    ///
    /// # Errors
    ///
    /// Returns an `io_submit` error other than `EAGAIN` (and the transient ones it retries).
    pub fn submit_work(&mut self) -> Result<bool, ExceptionPtr> {
        let mut did_work = false;

        self._submission_queue.clear();
        let this = self as *mut AioStorageContext;
        let r = self.r();
        // SAFETY: `_r` is the reactor that owns this backend; the eventfd is read once, and
        // the sink is reached through its own field so that no reference to the whole
        // reactor is live while it drains.
        let (aio_eventfd, sink) = unsafe {
            ((*r)._aio_eventfd.as_ref().map(|e| e.get_fd()), std::ptr::addr_of_mut!((*r)._io_sink))
        };
        // Bound first so that its body stays outside the drain's unsafe block: its unsafe
        // operations carry their own blocks.
        let prepare = |req: &IoRequest, desc: *mut IoCompletion| -> bool {
            // SAFETY: `this` is valid during the drain; the sink does not alias the context.
            let me = unsafe { &mut *this };
            if !me._iocb_pool.has_capacity() {
                return false;
            }

            // SAFETY: the pool has capacity (checked above).
            let io = unsafe { me._iocb_pool.get_one() };
            // SAFETY: `io` points into the pool we own.
            prepare_iocb(req, desc, unsafe { &mut *io });

            if let Some(efd) = aio_eventfd {
                // SAFETY: as above.
                set_eventfd_notification(unsafe { &mut *io }, efd);
            }
            // SAFETY: the queue was cleared above and holds at most one entry per iocb of
            // the pool (`max_aio`), its capacity.
            unsafe { static_vector_push(&mut me._submission_queue, io) };
            true
        };
        // SAFETY: as above; draining runs no user code.
        let mut to_submit = unsafe { (*sink).drain(prepare) };

        // SAFETY: `_r` is the reactor that owns this backend, live while the backend is; it is reached for this access only.
        if unsafe { (*self.r())._cfg.kernel_page_cache } {
            // linux-aio is not asynchronous when the page cache is used,
            // so we don't want to call io_submit() from the reactor thread.
            //
            // Pretend that all aio failed with EAGAIN and submit them
            // via schedule_retry(), below.
            did_work = !self._submission_queue.is_empty();
            for i in 0..self._submission_queue.len() {
                // SAFETY: `i` is in bounds.
                let iocbp = unsafe { *self._submission_queue.get_unchecked(i) };
                // SAFETY: the queued iocbs belong to our pool.
                set_nowait(unsafe { &mut *iocbp }, false);
                // SAFETY: each iocb of the pool is in at most one retry list, so the list
                // (capacity `max_aio`) has room.
                unsafe { static_vector_push(&mut self._pending_aio_retry, iocbp) };
            }
            to_submit = 0;
        }

        let mut nr_consumed: usize;
        let mut iocbs = 0usize;
        let end = to_submit;
        while iocbs < end {
            let nr = end - iocbs;
            // SAFETY: the slice `[iocbs, end)` of the submission queue holds prepared iocbs.
            let r = unsafe {
                io_submit(
                    self._io_context,
                    nr as i64,
                    self._submission_queue.as_mut_ptr().add(iocbs),
                )
            };
            if r == -1 {
                // SAFETY: `iocbs < end <= len`.
                let first = unsafe { *self._submission_queue.get_unchecked(iocbs) };
                nr_consumed = self.handle_aio_error(first, errno())?;
            } else {
                nr_consumed = r as usize;
            }
            did_work = true;
            iocbs += nr_consumed;
        }

        if self.need_to_retry() {
            self.schedule_retry();
        }

        Ok(did_work)
    }

    /// Starts (if not running) the loop that resubmits the requests rejected with `EAGAIN`, on
    /// the syscall thread, until no retry is pending.
    pub fn schedule_retry(&mut self) {
        // Don't start a new retry loop if either one is already in flight, or the context is
        // stopping: stop() drains the lists itself, and a second loop would race with its
        // drain on the same control blocks.
        if self.retry_in_progress() || self._stopping {
            return;
        }
        // The loop outlives this call: it keeps the root pointer.
        let this = self._this;
        // loop until both _pending_aio_retry and _aio_retries are empty.
        // While retrying _aio_retries, new retries may be queued onto _pending_aio_retry.
        self._pending_aio_retry_fut = do_until(
            move || {
                // SAFETY: the storage context outlives the retry loop (stop() waits for it).
                let me = unsafe { &mut *this };
                if me._aio_retries.is_empty() {
                    if me._pending_aio_retry.is_empty() {
                        return true;
                    }
                    // _pending_aio_retry, holding a batch of new iocbs to retry,
                    // is swapped with the empty _aio_retries.
                    std::mem::swap(&mut me._aio_retries, &mut me._pending_aio_retry);
                }
                false
            },
            move || {
                // SAFETY: as above.
                let me = unsafe { &mut *this };
                let ctx = me._io_context;
                let ptr = me._aio_retries.as_mut_ptr() as usize;
                let len = me._aio_retries.len();
                // SAFETY: `_r` is the reactor that owns this backend, live while the backend is; it is reached for this access only.
                unsafe { (*me.r()).thread_pool() }
                    .submit::<SyscallResult<i32>, _>(move || {
                        // SAFETY: `_aio_retries` is not touched by the reactor while the retry is in flight.
                        let r = unsafe { io_submit(ctx, len as i64, ptr as *mut *mut Iocb) };
                        wrap_syscall::<i32>(r)
                    })
                    .then_wrapped(move |f: Future<SyscallResult<i32>>| {
                        // SAFETY: as above.
                        let me = unsafe { &mut *this };
                        // If submit failed, just log the error and exit the loop.
                        // The next call to submit_work will call schedule_retry again.
                        if f.failed() {
                            let ex = f.get_exception();
                            engine_logger().warn(
                                FormatInfo::new("aio_storage_context::schedule_retry failed: {}"),
                                format_args!(
                                    "aio_storage_context::schedule_retry failed: {}",
                                    DisplayExceptionPtr(Some(&ex))
                                ),
                            );
                            return;
                        }
                        // The future is ready and did not fail.
                        let result = match f.get() {
                            Ok(r) => r,
                            Err(ex) => {
                                engine_logger().warn(
                                    FormatInfo::new(
                                        "aio_storage_context::schedule_retry failed: {}",
                                    ),
                                    format_args!(
                                        "aio_storage_context::schedule_retry failed: {}",
                                        DisplayExceptionPtr(Some(&ex))
                                    ),
                                );
                                return;
                            }
                        };
                        let iocbs = me._aio_retries[0];
                        let nr_consumed: usize;
                        if result.result == -1 {
                            match me.handle_aio_error(iocbs, result.error) {
                                Ok(n) => nr_consumed = n,
                                Err(e) => {
                                    engine_logger().error(
                                        FormatInfo::new("aio retry failed: {}. Aborting."),
                                        format_args!(
                                            "aio retry failed: {}. Aborting.",
                                            DisplayExceptionPtr(Some(&e))
                                        ),
                                    );
                                    std::process::abort();
                                }
                            }
                        } else {
                            nr_consumed = result.result as usize;
                        }
                        me._aio_retries.drain(0..nr_consumed);
                    })
            },
        );
    }

    /// Reaps finished disk requests without blocking and completes them; when `allow_retry`,
    /// also starts the retry loop if retries are pending. Returns `true` if any completed.
    pub fn reap_completions(&mut self, mut allow_retry: bool) -> bool {
        // While stopping, complete EAGAIN control blocks at once instead of queueing them for a
        // retry: no retry loop starts any more.
        if self._stopping {
            allow_retry = false;
        }
        let timeout = Timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `_io_context` is the context set up in `new` and destroyed only on drop;
        // `_ev_buffer` holds AIO_STORAGE_MAX_AIO events and `timeout` is a live timespec.
        // `_r` is the reactor that owns this backend, live while the backend
        // is; it is reached for this access only.
        let mut n = unsafe {
            io_getevents(
                self._io_context,
                1,
                AIO_STORAGE_MAX_AIO as i64,
                self._ev_buffer.as_mut_ptr(),
                &timeout,
                (*self.r())._cfg.force_io_getevents_syscall,
            )
        };
        if n == -1 && errno() == EINTR {
            n = 0;
        }
        engine_assert!(n >= 0);
        for i in 0..n as usize {
            // SAFETY: the kernel wrote the first `n <= max_aio` events of the buffer.
            let ev = unsafe { *self._ev_buffer.get_unchecked(i) };
            let iocb = get_iocb(&ev);
            if ev.res == -(EAGAIN as i64) && allow_retry {
                // SAFETY: the completed iocb belongs to our pool.
                set_nowait(unsafe { &mut *iocb }, false);
                // SAFETY: each iocb of the pool is in at most one retry list (capacity
                // `max_aio`).
                unsafe { static_vector_push(&mut self._pending_aio_retry, iocb) };
                continue;
            }
            // SAFETY: the completed iocb was taken from our pool.
            unsafe { self._iocb_pool.put_one(iocb) };
            let desc = get_user_data(&ev);
            // SAFETY: the completion lives until it is completed.
            unsafe { KernelCompletion::complete_with(desc, ev.res as isize) };
        }
        n != 0
    }

    /// Whether the reactor may sleep: only when no disk request is in flight (disk completions
    /// cannot wake it) or when the reactor has an aio eventfd to wake it.
    pub fn can_sleep(&self) -> bool {
        // Because aio depends on polling, it cannot generate events to wake us up, Therefore, sleep
        // is only possible if there are no in-flight aios. If there are, we need to keep polling.
        //
        // Alternatively, if we enabled _aio_eventfd, we can always enter
        let executing = self._iocb_pool.outstanding();
        // SAFETY: `_r` is the reactor that owns this backend, live while the backend is; it is reached for this access only.
        executing == 0 || unsafe { (*self.r())._aio_eventfd.is_some() }
    }
}

impl Drop for AioStorageContext {
    fn drop(&mut self) {
        io_destroy(self._io_context);
    }
}

impl AioGeneralContext {
    /// A context for `nr` control blocks.
    ///
    /// # Errors
    ///
    /// Returns the error of setting up the linux-aio context.
    pub fn new(nr: usize) -> Result<Self, ExceptionPtr> {
        let mut this = AioGeneralContext {
            io_context: 0,
            iocbs: vec![std::ptr::null_mut(); nr].into_boxed_slice(),
            last: 0,
            end: nr,
        };
        setup_aio_context(nr as u32, &mut this.io_context)?;
        Ok(this)
    }

    /// Adds a control block to the batch.
    ///
    /// Aborts the process if the batch is full.
    pub fn queue(&mut self, iocb: *mut Iocb) {
        engine_assert!(self.last < self.end);
        self.iocbs[self.last] = iocb;
        self.last += 1;
    }

    /// Submits all queued control blocks and returns their count.
    pub fn flush(&mut self) -> usize {
        let mut begin = 0usize;
        let mut retry_until: Option<Instant> = None;
        while begin != self.last {
            // SAFETY: `[begin, last)` holds queued iocbs.
            let r = unsafe {
                io_submit(
                    self.io_context,
                    (self.last - begin) as i64,
                    self.iocbs.as_mut_ptr().add(begin),
                )
            };
            if r > 0 {
                begin += r as usize;
                continue;
            }
            // errno == EAGAIN is expected here. We don't explicitly assert that
            // since the assert below prevents an endless loop for any reason.
            match retry_until {
                None => {
                    // allow retrying for 1 second
                    retry_until = Some(Instant::now() + Duration::from_secs(1));
                }
                Some(t) => {
                    engine_assert!(Instant::now() < t);
                }
            }
        }
        let nr = self.last;
        self.last = 0;
        nr
    }
}

impl Drop for AioGeneralContext {
    fn drop(&mut self) {
        io_destroy(self.io_context);
    }
}

impl CompletionWithIocb {
    /// A poll control block for `events` on `fd`. The user data (the address of the
    /// containing completion) is stored by `maybe_queue` once that address is final.
    pub(crate) fn new(fd: i32, events: i32) -> Self {
        CompletionWithIocb { _in_context: false, _iocb: make_poll_iocb(fd, events) }
    }

    /// Marks the control block as no longer submitted.
    pub(crate) fn completed(&mut self) {
        self._in_context = false;
    }

    /// Queues the control block in `context` unless it is already submitted; `user_data` is
    /// the containing completion.
    ///
    /// # Safety
    /// `this` is the `cwi` field of the live completion `user_data`, both derived from the
    /// backend's root pointer (the kernel keeps them); `context` is live; no reference to
    /// either is live.
    pub unsafe fn maybe_queue(
        this: *mut Self,
        context: *mut AioGeneralContext,
        user_data: *mut KernelCompletion,
    ) {
        // SAFETY: the contract above; short raw places.
        unsafe {
            if !(*this)._in_context {
                (*this)._in_context = true;
                let iocb = std::ptr::addr_of_mut!((*this)._iocb);
                set_user_data(&mut *iocb, user_data);
                (*context).queue(iocb);
            }
        }
    }
}

impl HrtimerAioCompletion {
    /// A completion for the high resolution timer descriptor `fd` of reactor `r`; `raw_fd` is
    /// the descriptor's
    /// number, which the poll iocb needs before the descriptor reaches its final address.
    ///
    /// # Safety
    /// `r` is the reactor that owns the completion's backend; `fd` as in
    /// `FdKernelCompletion::new`, for the descriptor `raw_fd`.
    pub unsafe fn new(r: *mut Reactor, fd: *mut FileDesc, raw_fd: i32) -> Self {
        // SAFETY: guaranteed by the caller.
        let fd_kc = unsafe { FdKernelCompletion::new::<Self>(fd) };
        HrtimerAioCompletion { fd_kc, cwi: CompletionWithIocb::new(raw_fd, POLLIN), _r: r }
    }
    /// The descriptor of the timer.
    #[inline]
    pub fn fd(&mut self) -> &mut FileDesc {
        self.fd_kc.fd()
    }
    /// Queues the timer poll in `context` unless already queued.
    ///
    /// # Safety
    /// `this` is the live completion, derived from the backend's root pointer, and
    /// `context` a live context; no reference to either is live.
    #[inline]
    pub unsafe fn maybe_queue(this: *mut Self, context: *mut AioGeneralContext) {
        // SAFETY: the contract above; `#[repr(C)]` with the kernel completion at offset 0.
        unsafe {
            CompletionWithIocb::maybe_queue(
                std::ptr::addr_of_mut!((*this).cwi),
                context,
                this.cast::<KernelCompletion>(),
            )
        }
    }
    /// Points the completion at its descriptor after its owner reached its final address.
    ///
    /// # Safety
    /// As [`FdKernelCompletion::rebind`].
    #[inline]
    pub(crate) unsafe fn rebind(&mut self, fd: *mut FileDesc) {
        // SAFETY: guaranteed by the caller.
        unsafe { self.fd_kc.rebind(fd) };
    }
}

impl TaskQuotaAioCompletion {
    /// A completion for the task-quota timer descriptor `fd`.
    ///
    /// # Safety
    /// `fd` points to a live descriptor that outlives the completion.
    pub unsafe fn new(fd: *mut FileDesc) -> Self {
        // SAFETY: guaranteed by the caller.
        let (fd_kc, raw) = unsafe { (FdKernelCompletion::new::<Self>(fd), (*fd).get()) };
        TaskQuotaAioCompletion { fd_kc, cwi: CompletionWithIocb::new(raw, POLLIN) }
    }
    /// Queues the timer poll in `context` unless already queued.
    ///
    /// # Safety
    /// `this` is the live completion, derived from the backend's root pointer, and
    /// `context` a live context; no reference to either is live.
    #[inline]
    pub unsafe fn maybe_queue(this: *mut Self, context: *mut AioGeneralContext) {
        // SAFETY: the contract above; `#[repr(C)]` with the kernel completion at offset 0.
        unsafe {
            CompletionWithIocb::maybe_queue(
                std::ptr::addr_of_mut!((*this).cwi),
                context,
                this.cast::<KernelCompletion>(),
            )
        }
    }
}

impl SmpWakeupAioCompletion {
    /// A completion for the inter-shard wake-up eventfd `fd`.
    ///
    /// # Safety
    /// `fd` points to a live descriptor that outlives the completion.
    pub unsafe fn new(fd: *mut FileDesc) -> Self {
        // SAFETY: guaranteed by the caller.
        let (fd_kc, raw) = unsafe { (FdKernelCompletion::new::<Self>(fd), (*fd).get()) };
        SmpWakeupAioCompletion { fd_kc, cwi: CompletionWithIocb::new(raw, POLLIN) }
    }
    /// Queues the eventfd poll in `context` unless already queued.
    ///
    /// # Safety
    /// `this` is the live completion, derived from the backend's root pointer, and
    /// `context` a live context; no reference to either is live.
    #[inline]
    pub unsafe fn maybe_queue(this: *mut Self, context: *mut AioGeneralContext) {
        // SAFETY: the contract above; `#[repr(C)]` with the kernel completion at offset 0.
        unsafe {
            CompletionWithIocb::maybe_queue(
                std::ptr::addr_of_mut!((*this).cwi),
                context,
                this.cast::<KernelCompletion>(),
            )
        }
    }
}

// SAFETY: `#[repr(C)]` with `fd_kc` (whose header is built for `Self`) at offset 0.
unsafe impl KernelCompletionImpl for HrtimerAioCompletion {
    unsafe fn complete_with(this: *mut Self, _ret: isize) {
        // SAFETY: the trait's contract: `this` is live and no reference to it is live;
        // `me` is not used after the object is destroyed.
        let me = unsafe { &mut *this };
        let mut expirations: u64 = 0;
        // SAFETY: the buffer is valid for writes of the length passed.
        let _ = unsafe { me.fd_kc.fd().read(&mut expirations as *mut u64 as *mut c_void, 8) };
        let r = me._r;
        if expirations != 0 {
            // SAFETY: as in HrtimerCompletion::complete_with; the timers may re-queue this
            // completion (request_preemption), so `me` is not used past this point.
            unsafe { Reactor::service_highres_timer(r) };
        }
        // SAFETY: the completion is live (it is a member of the backend).
        unsafe { (*this).cwi.completed() };
    }
}

// SAFETY: as above.
unsafe impl KernelCompletionImpl for TaskQuotaAioCompletion {
    unsafe fn complete_with(this: *mut Self, _ret: isize) {
        // SAFETY: the trait's contract: `this` is live and no reference to it is live;
        // `me` is not used after the object is destroyed.
        let me = unsafe { &mut *this };
        let mut v: u64 = 0;
        // SAFETY: the buffer is valid for writes of the length passed.
        let _ = unsafe { me.fd_kc.fd().read(&mut v as *mut u64 as *mut c_void, 8) };
        me.cwi.completed();
    }
}

// SAFETY: as above.
unsafe impl KernelCompletionImpl for SmpWakeupAioCompletion {
    unsafe fn complete_with(this: *mut Self, _ret: isize) {
        // SAFETY: the trait's contract: `this` is live and no reference to it is live;
        // `me` is not used after the object is destroyed.
        let me = unsafe { &mut *this };
        let mut ignore: u64 = 0;
        // SAFETY: the buffer is valid for writes of the length passed.
        let _ = unsafe { me.fd_kc.fd().read(&mut ignore as *mut u64 as *mut c_void, 8) };
        me.cwi.completed();
    }
}

impl PreemptIoContext {
    /// The preempting context for reactor `r`, with the task-quota timer `task_quota` and the
    /// high resolution timer `hrtimer`:
    /// `hrtimer_fd` is the number of the `hrtimer` descriptor, which may still be null.
    ///
    /// # Safety
    /// `r` is the reactor that owns the backend; `task_quota` points to a live descriptor
    /// that outlives the context; `hrtimer` as in [`HrtimerAioCompletion::new`].
    pub unsafe fn new(
        r: *mut Reactor,
        task_quota: *mut FileDesc,
        hrtimer: *mut FileDesc,
        hrtimer_fd: i32,
    ) -> Result<Self, ExceptionPtr> {
        // SAFETY: guaranteed by the caller.
        unsafe {
            Ok(PreemptIoContext {
                _r: r,
                _context: AioGeneralContext::new(2)?,
                _task_quota_aio_completion: TaskQuotaAioCompletion::new(task_quota),
                _hrtimer_aio_completion: HrtimerAioCompletion::new(r, hrtimer, hrtimer_fd),
            })
        }
    }

    /// Points the high resolution timer completion at the timer descriptor once its owner
    /// reached its final address.
    ///
    /// # Safety
    /// As [`FdKernelCompletion::rebind`].
    pub(crate) unsafe fn rebind_hrtimer(&mut self, hrtimer: *mut FileDesc) {
        // SAFETY: guaranteed by the caller.
        unsafe { self._hrtimer_aio_completion.rebind(hrtimer) };
    }

    /// Makes the preemption check read this context's completion ring, so a timer tick or a
    /// signal queued by the kernel requests preemption.
    pub fn start_tick(&mut self) {
        // Preempt whenever an event (timer tick or signal) is available on the
        // _preempting_io ring
        set_need_preempt_var((self._context.io_context as usize + 8) as *const PreemptionMonitor);
        // `PreemptIoContext::request_preemption` writes to the reactor's own monitor, which is now ignored
    }

    /// Makes the preemption check read the reactor's own monitor again.
    pub fn stop_tick(&mut self) {
        // SAFETY: `_r` is the reactor that owns the backend, live while the context is.
        set_need_preempt_var(unsafe { &(*self._r)._preemption_monitor } as *const PreemptionMonitor);
    }

    // The three functions below queue the context's completions in the kernel or run them,
    // so they take the context through the backend's root pointer.

    /// # Safety
    /// `this` is the live context, derived from the backend's root pointer, with no reference
    /// to the backend live.
    pub unsafe fn request_preemption(this: *mut Self) {
        let mut expired = Itimerspec::default();
        expired.it_value.tv_nsec = 1;
        // SAFETY: see above; short raw places.
        unsafe {
            let hrt = std::ptr::addr_of_mut!((*this)._hrtimer_aio_completion);
            // will trigger immediately, triggering the preemption monitor
            if let Err(e) = (*hrt).fd().timerfd_settime(TFD_TIMER_ABSTIME, &expired) {
                exception_in_reactor_main_loop(e);
            }

            // This might have been called from poll_once. If that is the case, we cannot assume that timerfd is being
            // monitored.
            HrtimerAioCompletion::maybe_queue(hrt, std::ptr::addr_of_mut!((*this)._context));
            (*this)._context.flush();
        }

        // The kernel is not obliged to deliver the completion immediately, so wait for it
        while !need_preempt() {
            compiler_fence(Ordering::SeqCst);
        }
    }

    /// # Safety
    /// As [`PreemptIoContext::request_preemption`].
    pub unsafe fn reset_preemption_monitor(this: *mut Self) {
        // SAFETY: see above.
        unsafe {
            Self::service_preempting_io(this);
            let ctx = std::ptr::addr_of_mut!((*this)._context);
            HrtimerAioCompletion::maybe_queue(
                std::ptr::addr_of_mut!((*this)._hrtimer_aio_completion),
                ctx,
            );
            TaskQuotaAioCompletion::maybe_queue(
                std::ptr::addr_of_mut!((*this)._task_quota_aio_completion),
                ctx,
            );
            (*this).flush();
        }
    }

    /// # Safety
    /// As [`PreemptIoContext::request_preemption`].
    pub unsafe fn service_preempting_io(this: *mut Self) -> bool {
        // Left uninitialized: the kernel fills the first `r` events.
        let mut a = [MaybeUninit::<IoEvent>::uninit(); 2];
        // SAFETY: see above; a plain read of the context id.
        let ioc = unsafe { (*this)._context.io_context };
        // SAFETY: `ioc` is the context's own, set up in `AioGeneralContext::new` and destroyed
        // with it; `a` has the 2 slots asked for; no timeout.
        let r = unsafe {
            io_getevents(ioc, 0, 2, a.as_mut_ptr().cast::<IoEvent>(), std::ptr::null_mut(), false)
        };
        engine_assert!(r != -1);
        let did_work = r > 0;
        for slot in &a[..r.max(0) as usize] {
            // SAFETY: the kernel wrote the first `r` events.
            let ev = unsafe { slot.assume_init_ref() };
            let desc = get_user_data(ev);
            // SAFETY: the completion lives until it is completed.
            unsafe { KernelCompletion::complete_with(desc, ev.res as isize) };
        }
        did_work
    }
}

impl ReactorBackendAio {
    fn make_timerfd() -> Result<FileDesc, ExceptionPtr> {
        FileDesc::timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC | TFD_NONBLOCK)
    }

    /// Runs the completions of the polling context, among them the high resolution timer's.
    ///
    /// # Safety
    /// `this` is the backend's root pointer, with no reference to it live.
    unsafe fn await_events(
        this: *mut Self,
        timeout: i32,
        active_sigmask: Option<&SigsetT>,
    ) -> bool {
        // SAFETY: the contract above; plain reads.
        let (ioc, rp) = unsafe { ((*this)._polling_io.io_context, (*this)._r) };
        let mut ts = Timespec::default();
        let mut use_ts: bool = if timeout == 0 {
            true
        } else if timeout == -1 {
            false
        } else {
            ts = posix::to_timespec(Duration::from_millis(timeout as u64));
            true
        };
        const BATCH_SIZE: usize = 128;
        // Left uninitialized: the kernel fills the first `r` events.
        let mut batch = [MaybeUninit::<IoEvent>::uninit(); BATCH_SIZE];
        let mut did_work = false;
        let mut r: i32;
        loop {
            let may_sleep = !use_ts || (ts.tv_nsec + ts.tv_sec > 0);
            let before_getevents = if may_sleep { Some(Reactor::now()) } else { None };
            let tsp = if use_ts { &mut ts as *mut Timespec } else { std::ptr::null_mut() };
            // SAFETY: `ioc` is the polling context's, set up in `AioGeneralContext::new` and
            // destroyed with the backend; `batch` has the BATCH_SIZE slots asked for; `tsp` is
            // null or points to `ts`, live across the call.
            r = unsafe {
                io_pgetevents(
                    ioc,
                    1,
                    BATCH_SIZE as i64,
                    batch.as_mut_ptr().cast::<IoEvent>(),
                    tsp,
                    active_sigmask,
                )
            };
            if let Some(before) = before_getevents {
                // SAFETY: `_r` is the reactor that owns this backend; a short access to a counter.
                unsafe { (*rp)._total_sleep += Reactor::now() - before };
            }
            if r == -1 && errno() == EINTR {
                return true;
            }
            engine_assert!(r != -1);
            for slot in &batch[..r.max(0) as usize] {
                did_work = true;
                // SAFETY: the kernel wrote the first `r` events.
                let event = unsafe { slot.assume_init_ref() };
                let desc = get_user_data(event);
                // SAFETY: the completion lives until it is completed.
                unsafe { KernelCompletion::complete_with(desc, event.res as isize) };
            }
            // For the next iteration, don't use a timeout, since we may have waited already
            ts = Timespec::default();
            use_ts = true;
            if r as usize != BATCH_SIZE {
                break;
            }
        }
        did_work
    }

    /// The linux-aio backend for reactor `r`. The members hold references to each other, so
    /// they are wired once the backend is at its heap address.
    ///
    /// # Safety
    /// `r` is the reactor that will own the backend, constructed in place, which outlives it.
    pub unsafe fn new(r: *mut Reactor) -> Result<NodePtr<Self>, ExceptionPtr> {
        // SAFETY: guaranteed by the caller; the descriptors are fields of the reactor, which
        // does not move.
        let (task_quota, notify, max_blocks, tfd) = unsafe {
            (
                std::ptr::addr_of_mut!((*r)._task_quota_timer),
                std::ptr::addr_of_mut!((*r)._notify_eventfd),
                (*r)._cfg.max_networking_aio_io_control_blocks as usize,
                (*r)._task_quota_timer.get(),
            )
        };
        // `_hrtimer_timerfd` is created before the completions that refer to it; they get its
        // number now and its address once
        // the backend is at its heap address.
        let hrtimer_timerfd = Self::make_timerfd()?;
        let raw = hrtimer_timerfd.get();
        // SAFETY: the completions' descriptor pointers are null until rebound below, before
        // anything queues them; the others point into the live reactor.
        let node = unsafe {
            NodePtr::new(ReactorBackendAio {
                _r: r,
                _hrtimer_timerfd: hrtimer_timerfd,
                _storage_context: AioStorageContext::new(r)?,
                _preempting_io: PreemptIoContext::new(r, task_quota, std::ptr::null_mut(), raw)?,
                _polling_io: AioGeneralContext::new(max_blocks)?,
                _hrtimer_poll_completion: HrtimerAioCompletion::new(r, std::ptr::null_mut(), raw),
                _smp_wakeup_aio_completion: SmpWakeupAioCompletion::new(notify),
            })
        };
        // The backend's root pointer: the members' pointers to each other derive from it.
        let this = node.as_ptr();
        // SAFETY: `hrtimer` is the backend's own descriptor at its final address; no
        // reference to the backend is live.
        unsafe {
            let hrtimer = std::ptr::addr_of_mut!((*this)._hrtimer_timerfd);
            (*this)._preempting_io.rebind_hrtimer(hrtimer);
            (*this)._hrtimer_poll_completion.rebind(hrtimer);
            AioStorageContext::init(std::ptr::addr_of_mut!((*this)._storage_context));
        }
        // Protect against spurious wakeups - if we get notified that the timer has
        // expired when it really hasn't, we don't want to block in read(tfd, ...).
        // SAFETY: plain fcntl calls on a descriptor we own.
        unsafe { fcntl(tfd, F_SETFL, fcntl(tfd, F_GETFL) | O_NONBLOCK) };

        let mask = make_sigset_mask(hrtimer_signal());
        // SAFETY: `mask` is a valid signal set.
        let e = unsafe { pthread_sigmask(SIG_BLOCK, &mask, std::ptr::null_mut()) };
        engine_assert!(e == 0);
        Ok(node)
    }

    fn poll(&mut self, fd: &mut PollableFdState, events: i32) -> Future<()> {
        if events & fd.events_known != 0 {
            fd.events_known &= !events;
            return make_ready_future(());
        }

        fd.events_rw = events == (POLLIN | POLLOUT);

        // SAFETY: every descriptor state made by this backend is an `AioPollableFdState`
        // whose first field is the base state.
        let pfd = unsafe { fd.derived::<AioPollableFdState>() };
        let raw_fd = fd.fd.get();
        // SAFETY: `pfd` is the state's root pointer; the iocb and completion are its own
        // fields, so the kernel gets pointers with the provenance of the whole state.
        unsafe {
            let iocb = AioPollableFdState::get_iocb(pfd, events);
            let desc = AioPollableFdState::get_desc(pfd, events);
            *iocb = make_poll_iocb(raw_fd, events);
            *desc = PollableFdStateCompletion::default();
            set_user_data(&mut *iocb, desc.cast::<KernelCompletion>());
            self._polling_io.queue(iocb);
            AioPollableFdState::get_completion_future(pfd, events)
        }
    }
}

#[repr(C)]
/// The descriptor state of the linux-aio backend: the base state, then one poll control block
/// and completion for each of `POLLIN`, `POLLOUT` and `POLLRDHUP`.
pub struct AioPollableFdState {
    /// The base state.
    pub base: PollableFdState,
    _iocb_pollin: Iocb,
    _completion_pollin: PollableFdStateCompletion,

    _iocb_pollout: Iocb,
    _completion_pollout: PollableFdStateCompletion,

    _iocb_pollrdhup: Iocb,
    _completion_pollrdhup: PollableFdStateCompletion,
}

// The derived states are reached through the base's root pointer (`PollableFdState::derived`)
// and only their own fields are touched, so a live reference to the base does not overlap.

impl AioPollableFdState {
    /// The completion waiting for `events` (one of `POLLIN`, `POLLOUT`, `POLLRDHUP`).
    ///
    /// # Safety
    /// `this` is the root pointer of a live state of this type.
    pub unsafe fn get_desc(this: *mut Self, events: i32) -> *mut PollableFdStateCompletion {
        // SAFETY: see above; no reference is formed.
        unsafe {
            if events & POLLIN != 0 {
                return std::ptr::addr_of_mut!((*this)._completion_pollin);
            }
            if events & POLLOUT != 0 {
                return std::ptr::addr_of_mut!((*this)._completion_pollout);
            }
            std::ptr::addr_of_mut!((*this)._completion_pollrdhup)
        }
    }
    /// The control block polling for `events`.
    ///
    /// # Safety
    /// As [`AioPollableFdState::get_desc`].
    pub unsafe fn get_iocb(this: *mut Self, events: i32) -> *mut Iocb {
        // SAFETY: see above; no reference is formed.
        unsafe {
            if events & POLLIN != 0 {
                return std::ptr::addr_of_mut!((*this)._iocb_pollin);
            }
            if events & POLLOUT != 0 {
                return std::ptr::addr_of_mut!((*this)._iocb_pollout);
            }
            std::ptr::addr_of_mut!((*this)._iocb_pollrdhup)
        }
    }
    /// A state for `fd` with speculation `speculate`.
    pub fn new(fd: FileDesc, speculate: Speculation) -> Box<Self> {
        Box::new(AioPollableFdState {
            base: PollableFdState::new(fd, speculate),
            // SAFETY: `Iocb` is a plain C struct for which all-zero bytes is a valid value.
            _iocb_pollin: unsafe { std::mem::zeroed() },
            _completion_pollin: PollableFdStateCompletion::default(),
            // SAFETY: as above, all-zero bytes is a valid `Iocb`.
            _iocb_pollout: unsafe { std::mem::zeroed() },
            _completion_pollout: PollableFdStateCompletion::default(),
            // SAFETY: as above, all-zero bytes is a valid `Iocb`.
            _iocb_pollrdhup: unsafe { std::mem::zeroed() },
            _completion_pollrdhup: PollableFdStateCompletion::default(),
        })
    }
    /// A future for the next completion of the `events` poll.
    ///
    /// # Safety
    /// As [`AioPollableFdState::get_desc`].
    pub unsafe fn get_completion_future(this: *mut Self, events: i32) -> Future<()> {
        // SAFETY: see above; the completion is reborrowed for this call only.
        unsafe { (*Self::get_desc(this, events)).get_future() }
    }
}

// The entries of `BackendEntries`: `this` is the backend's root pointer, with no reference
// to it live (the table's contract).
impl ReactorBackendAio {
    unsafe fn reap_kernel_completions(this: *mut Self) -> bool {
        // SAFETY: see above. The storage completions only resolve promises, so the short
        // reborrow of the storage context is not re-entered.
        unsafe {
            let mut did_work = Self::await_events(this, 0, None);
            did_work |= (*this)._storage_context.reap_completions(true);
            did_work
        }
    }

    unsafe fn kernel_submit_work(this: *mut Self) -> bool {
        // SAFETY: see above; the completion and the contexts are reached through `this`.
        unsafe {
            let polling = std::ptr::addr_of_mut!((*this)._polling_io);
            HrtimerAioCompletion::maybe_queue(
                std::ptr::addr_of_mut!((*this)._hrtimer_poll_completion),
                polling,
            );
            let mut did_work = (*polling).flush() != 0;
            did_work |= (*this)
                ._storage_context
                .submit_work()
                .unwrap_or_else(|e| exception_in_reactor_main_loop(e));
            did_work
        }
    }

    unsafe fn wait_and_process_events(this: *mut Self, active_sigmask: Option<&SigsetT>) {
        // SAFETY: see above.
        unsafe {
            let preempting = std::ptr::addr_of_mut!((*this)._preempting_io);
            let mut timeout = -1;
            let did_work = PreemptIoContext::service_preempting_io(preempting);
            if did_work {
                timeout = 0;
            }

            let polling = std::ptr::addr_of_mut!((*this)._polling_io);
            HrtimerAioCompletion::maybe_queue(
                std::ptr::addr_of_mut!((*this)._hrtimer_poll_completion),
                polling,
            );
            SmpWakeupAioCompletion::maybe_queue(
                std::ptr::addr_of_mut!((*this)._smp_wakeup_aio_completion),
                polling,
            );
            (*polling).flush();
            Self::await_events(this, timeout, active_sigmask);
            PreemptIoContext::service_preempting_io(preempting); // clear task quota timer
        }
    }

    unsafe fn reset_preemption_monitor(this: *mut Self) {
        // SAFETY: see above.
        unsafe {
            PreemptIoContext::reset_preemption_monitor(std::ptr::addr_of_mut!(
                (*this)._preempting_io
            ))
        };
    }

    unsafe fn request_preemption(this: *mut Self) {
        // SAFETY: see above.
        unsafe {
            PreemptIoContext::request_preemption(std::ptr::addr_of_mut!((*this)._preempting_io))
        };
    }
}

impl ReactorBackend for ReactorBackendAio {
    fn entries(&self) -> &'static BackendEntries {
        backend_entries!(ReactorBackendAio)
    }

    fn kernel_events_can_sleep(&self) -> bool {
        self._storage_context.can_sleep()
    }

    fn signal_received(&self, signo: i32, siginfo: *mut SiginfoT, ignore: *mut c_void) {
        crate::engine::core::reactor::Signals::action(signo, siginfo, ignore);
    }

    fn start_handling_signal(&self) {
        // The aio backend only uses SIGHUP/SIGTERM/SIGINT. We don't need to handle them right away and our
        // implementation of request_preemption is not signal safe, so do nothing.
    }
}

// The I/O entries of `BackendEntries` (the table's contract). The polls only queue an iocb
// and the socket operations are the reactor's generic ones, which come back here through
// the table; none runs a completion, so the short reborrows below overlap nothing.
impl ReactorBackendAio {
    unsafe fn readable(this: *mut Self, fd: *mut PollableFdState) -> Future<()> {
        // SAFETY: see above; one call that only queues an iocb.
        unsafe { (*this).poll(&mut *fd, POLLIN) }
    }

    unsafe fn writeable(this: *mut Self, fd: *mut PollableFdState) -> Future<()> {
        // SAFETY: as in readable.
        unsafe { (*this).poll(&mut *fd, POLLOUT) }
    }

    unsafe fn readable_or_writeable(this: *mut Self, fd: *mut PollableFdState) -> Future<()> {
        // SAFETY: as in readable.
        unsafe { (*this).poll(&mut *fd, POLLIN | POLLOUT) }
    }

    unsafe fn poll_rdhup(this: *mut Self, fd: *mut PollableFdState) -> Future<()> {
        // SAFETY: as in readable.
        unsafe { (*this).poll(&mut *fd, POLLRDHUP) }
    }

    unsafe fn forget(_this: *mut Self, fd: *mut PollableFdState) {
        // SAFETY: the table's contract: the state was allocated by `make_pollable_fd_state` as
        // a boxed `AioPollableFdState`, and `fd` is its root pointer.
        drop(unsafe { Box::from_raw(fd.cast::<AioPollableFdState>()) });
        // ?
    }

    unsafe fn accept(
        _this: *mut Self,
        listenfd: *mut PollableFdState,
    ) -> Future<(PollableFd, SocketAddress)> {
        // SAFETY: forwarded (the table's contract).
        unsafe { Reactor::do_accept(listenfd) }
    }

    unsafe fn connect(
        _this: *mut Self,
        fd: *mut PollableFdState,
        sa: &mut SocketAddress,
    ) -> Future<()> {
        // SAFETY: forwarded.
        unsafe { Reactor::do_connect(fd, sa) }
    }

    unsafe fn shutdown(
        _this: *mut Self,
        fd: *mut PollableFdState,
        how: i32,
    ) -> Result<(), ExceptionPtr> {
        // SAFETY: the table's contract; a shared reborrow of the descriptor for one syscall.
        unsafe { (*fd).fd.shutdown(how) }
    }

    unsafe fn read(
        _this: *mut Self,
        fd: *mut PollableFdState,
        buffer: *mut c_void,
        len: usize,
    ) -> Future<usize> {
        // SAFETY: forwarded.
        unsafe { Reactor::do_read(fd, buffer, len) }
    }

    unsafe fn recvmsg(
        _this: *mut Self,
        fd: *mut PollableFdState,
        iov: &Vec<Iovec>,
    ) -> Future<usize> {
        // SAFETY: forwarded.
        unsafe { Reactor::do_recvmsg(fd, iov) }
    }

    unsafe fn read_some(
        _this: *mut Self,
        fd: *mut PollableFdState,
        ba: *mut dyn BufferAllocator,
    ) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: forwarded.
        unsafe { Reactor::do_read_some(fd, ba) }
    }

    unsafe fn send(
        _this: *mut Self,
        fd: *mut PollableFdState,
        buffer: *const c_void,
        len: usize,
    ) -> Future<usize> {
        // SAFETY: forwarded.
        unsafe { Reactor::do_send(fd, buffer, len) }
    }

    unsafe fn sendmsg(_this: *mut Self, fd: *mut PollableFdState, p: &mut Packet) -> Future<usize> {
        // SAFETY: forwarded.
        unsafe { Reactor::do_sendmsg(fd, p) }
    }

    unsafe fn recv_some(
        _this: *mut Self,
        fd: *mut PollableFdState,
        ba: *mut dyn BufferAllocator,
    ) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: forwarded.
        unsafe { Reactor::do_recv_some(fd, ba) }
    }

    unsafe fn start_tick(this: *mut Self) {
        // SAFETY: the table's contract; the context is reborrowed for one call.
        unsafe { (*std::ptr::addr_of_mut!((*this)._preempting_io)).start_tick() };
    }

    unsafe fn stop_tick(this: *mut Self) {
        // SAFETY: as in start_tick.
        unsafe { (*std::ptr::addr_of_mut!((*this)._preempting_io)).stop_tick() };
    }

    unsafe fn arm_highres_timer(this: *mut Self, its: &Itimerspec) {
        // An error arming the high resolution timer aborts: enabling a timer cannot fail.
        // SAFETY: the table's contract; a shared reborrow of the descriptor for one syscall.
        if let Err(e) = unsafe {
            (*std::ptr::addr_of!((*this)._hrtimer_timerfd)).timerfd_settime(TFD_TIMER_ABSTIME, its)
        } {
            exception_in_noexcept(e);
        }
    }

    unsafe fn make_pollable_fd_state(
        _this: *mut Self,
        fd: FileDesc,
        speculate: Speculation,
    ) -> PollableFdStatePtr {
        let p = Box::into_raw(AioPollableFdState::new(fd, speculate)).cast::<PollableFdState>();
        // SAFETY: a fresh state from `Box::into_raw` of the derived state (`#[repr(C)]`, base
        // first): `p` keeps the provenance of the whole derived state, and is its root.
        unsafe {
            (*p)._this = p;
            PollableFdStatePtr::new(p)
        }
    }
}

/// A descriptor and its events for `poll(2)` (`struct pollfd`).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Pollfd {
    fd: i32,
    events: i16,
    revents: i16,
}

/// A scheduling priority for `pthread_setschedparam` (`struct sched_param`).
#[repr(C)]
struct SchedParam {
    sched_priority: i32,
}

const SCHED_FIFO: i32 = 1;
const SIGSEGV: i32 = 11;
const POLL_IN: i16 = 1;

unsafe extern "C" {
    fn poll(fds: *mut Pollfd, nfds: u64, timeout: i32) -> i32;
    fn sigfillset(set: *mut SigsetT) -> i32;
    fn sigdelset(set: *mut SigsetT, signo: i32) -> i32;
    fn pthread_setname_np(thread: usize, name: *const std::ffi::c_char) -> i32;
    fn pthread_setschedparam(thread: usize, policy: i32, param: *const SchedParam) -> i32;
    #[link_name = "read"]
    fn libc_read(fd: i32, buf: *mut c_void, n: usize) -> isize;
}

/// What the epoll backend's timer thread may touch: the task
/// quota timer's descriptor, the reactor's `_dying` flag and preemption monitor, the
/// backend's `_highres_timer_pending` flag and its timer-thread timerfd. All of them are
/// atomics or descriptors; no reference to the reactor or the backend is formed on that
/// thread.
struct EpollTimerThreadState {
    shard: u32,
    task_quota_timer: i32,
    steady_clock_timer: i32,
    dying: *const AtomicBool,
    highres_timer_pending: *const AtomicBool,
    preemption_monitor: *const PreemptionMonitor,
}

// SAFETY: the pointers refer to atomics of the reactor and of the backend, which join the
// thread (stop_tick) before they are destroyed; atomics may be shared between threads.
unsafe impl Send for EpollTimerThreadState {}

impl EpollTimerThreadState {
    /// Reads the 8-byte counter of a timerfd or eventfd; a short read leaves `events` as it is.
    fn read_counter(fd: i32) {
        let mut events: u64 = 0;
        // SAFETY: reading 8 bytes into a local from a descriptor that outlives the thread.
        let _ = unsafe { libc_read(fd, &mut events as *mut u64 as *mut c_void, 8) };
    }

    /// Requests preemption of the reactor thread, from the timer thread.
    fn request_preemption(&self) {
        // SAFETY: see the type's contract.
        unsafe { (*self.preemption_monitor).head.store(1, Ordering::Relaxed) };
    }

    /// The timer thread's body: blocks signals, then waits on the task-quota and high
    /// resolution timerfds and requests preemption of the reactor thread whenever either
    /// expires (flagging the high resolution timer as pending), until the reactor dies.
    fn run(self) {
        let thread_name = format!("timer-{}", self.shard);
        let cname = std::ffi::CString::new(thread_name.clone()).unwrap_or_default();
        // SAFETY: `cname` is a valid C string for the duration of the call.
        unsafe { pthread_setname_np(pthread_self(), cname.as_ptr()) };

        let mut mask = SigsetT::default();
        // SAFETY: `mask` is a valid signal set.
        unsafe { sigfillset(&mut mask) };
        // Block every signal except the ones in this list (kept as a list for extension).
        #[allow(clippy::single_element_loop)]
        for sig in [SIGSEGV] {
            // SAFETY: as above.
            unsafe { sigdelset(&mut mask, sig) };
        }
        // SAFETY: as above.
        let r = unsafe { pthread_sigmask(SIG_BLOCK, &mask, std::ptr::null_mut()) };
        if r != 0 {
            engine_logger().error(
                FormatInfo::new("Thread {}: failed to block signals. Aborting."),
                format_args!("Thread {}: failed to block signals. Aborting.", thread_name),
            );
            std::process::abort();
        }

        // We need to wait until task quota is set before we can calculate how many ticks are to
        // a minute. Technically task_quota is used from many threads, but since it is read-only here
        // and only used during initialization we will avoid complicating the code.
        Self::read_counter(self.task_quota_timer);
        self.request_preemption();

        // SAFETY: see the type's contract.
        while !unsafe { (*self.dying).load(Ordering::Relaxed) } {
            // Wait for either the task quota timer, or the high resolution timer, or both,
            // to expire.
            let mut pfds = [Pollfd::default(); 2];
            pfds[0].fd = self.task_quota_timer;
            pfds[0].events = POLL_IN;
            pfds[1].fd = self.steady_clock_timer;
            pfds[1].events = POLL_IN;
            // SAFETY: `pfds` is a valid array of two pollfd.
            let r = unsafe { poll(pfds.as_mut_ptr(), 2, -1) };
            engine_assert!(r != -1);

            if pfds[0].revents & POLL_IN != 0 {
                Self::read_counter(self.task_quota_timer);
            }
            if pfds[1].revents & POLL_IN != 0 {
                Self::read_counter(self.steady_clock_timer);
                // SAFETY: see the type's contract.
                unsafe { (*self.highres_timer_pending).store(true, Ordering::Relaxed) };
            }
            self.request_preemption();

            // We're in a different thread, but guaranteed to be on the same core, so even
            // a signal fence is overdoing it
            compiler_fence(Ordering::SeqCst);
        }
    }
}

impl ReactorBackendEpoll {
    /// The owning reactor, as a raw pointer.
    #[inline]
    fn r(&self) -> *mut Reactor {
        self._r
    }

    /// The epoll backend for reactor `r`.
    ///
    /// # Safety
    /// `r` is the reactor that will own the backend, constructed in place, which outlives it.
    pub unsafe fn new(r: *mut Reactor) -> Result<NodePtr<Self>, ExceptionPtr> {
        // SAFETY: guaranteed by the caller.
        let storage_context = unsafe { AioStorageContext::new(r) }?;
        let node = NodePtr::new(ReactorBackendEpoll {
            _r: r,
            _highres_timer_pending: AtomicBool::new(false),
            _task_quota_timer_thread: None,
            _steady_clock_timer_deadline: Itimerspec::default(),
            _steady_clock_timer_reactor_thread: FileDesc::timerfd_create(
                CLOCK_MONOTONIC,
                TFD_NONBLOCK | TFD_CLOEXEC,
            )?,
            _steady_clock_timer_timer_thread: FileDesc::timerfd_create(
                CLOCK_MONOTONIC,
                TFD_NONBLOCK | TFD_CLOEXEC,
            )?,
            _epollfd: FileDesc::epoll_create(EPOLL_CLOEXEC)?,
            _storage_context: storage_context,
            _need_epoll_events: false,
        });
        // The backend's root pointer: the members' pointers to each other derive from it.
        let p = node.as_ptr();
        // SAFETY: the context at its final address, derived from the root pointer.
        unsafe { AioStorageContext::init(std::ptr::addr_of_mut!((*p)._storage_context)) };
        // SAFETY: no reference to the backend is live; the reborrow ends with this function.
        let this = unsafe { &mut *p };
        let mut event = EpollEvent { events: EPOLLIN, data: 0 };
        // SAFETY: plain epoll_ctl on descriptors we own; the notify eventfd is a field of the
        // live reactor, read for this call.
        let mut ret = unsafe {
            epoll_ctl(this._epollfd.get(), EPOLL_CTL_ADD, (*r)._notify_eventfd.get(), &mut event)
        };
        posix::throw_system_error_on(ret == -1, "epoll_ctl")?;
        event.events = EPOLLIN;
        // SAFETY: a field address from the root pointer, which the kernel keeps.
        event.data = unsafe { std::ptr::addr_of!((*p)._steady_clock_timer_reactor_thread) } as u64;
        // SAFETY: as above.
        ret = unsafe {
            epoll_ctl(
                this._epollfd.get(),
                EPOLL_CTL_ADD,
                this._steady_clock_timer_reactor_thread.get(),
                &mut event,
            )
        };
        posix::throw_system_error_on(ret == -1, "epoll_ctl")?;
        Ok(node)
    }

    /// Makes `to` the armed high resolution timer and disarms the other: the two timers are
    /// fields of the backend, named here by which one is `to`.
    fn switch_steady_clock_timers(&self, to_reactor_thread: bool) {
        let deadline = &self._steady_clock_timer_deadline;
        if deadline.it_value.tv_sec == 0 && deadline.it_value.tv_nsec == 0 {
            return;
        }
        let (from, to) = if to_reactor_thread {
            (&self._steady_clock_timer_timer_thread, &self._steady_clock_timer_reactor_thread)
        } else {
            (&self._steady_clock_timer_reactor_thread, &self._steady_clock_timer_timer_thread)
        };
        // Enable-then-disable, so the hardware timer doesn't have to be reprogrammed. Probably pointless.
        if let Err(e) = to.timerfd_settime(TFD_TIMER_ABSTIME, deadline) {
            exception_in_reactor_main_loop(e);
        }
        if let Err(e) = from.timerfd_settime(TFD_TIMER_ABSTIME, &Itimerspec::default()) {
            exception_in_reactor_main_loop(e);
        }
    }

    fn maybe_switch_steady_clock_timers(&self, timeout: i32, to_reactor_thread: bool) {
        if timeout != 0 {
            self.switch_steady_clock_timers(to_reactor_thread);
        }
    }

    /// Waits for epoll events (at most `timeout` ms, -1 for no limit) with `active_sigmask`
    /// applied, and handles them; returns `true` if any work was done. The body runs between
    /// the switch of the steady clock timers and its undo (the body has no early exit, so the
    /// undo is written after it).
    fn wait_and_process(&mut self, timeout: i32, active_sigmask: Option<&SigsetT>) -> bool {
        // If we plan to sleep, disable the timer thread steady clock timer (since it won't
        // wake us up from sleep, and timer thread wakeup will just waste CPU time) and enable
        // reactor thread steady clock timer.
        self.maybe_switch_steady_clock_timers(timeout, true);
        let r = self.wait_and_process_body(timeout, active_sigmask);
        self.maybe_switch_steady_clock_timers(timeout, false);
        // (a failure to switch the timers back aborts inside timerfd_settime:
        // "Switching steady_clock timers back failed: {}. Aborting...")
        r
    }

    fn wait_and_process_body(&mut self, timeout: i32, active_sigmask: Option<&SigsetT>) -> bool {
        // The address the reactor thread's timer is registered with (its epoll data).
        let reactor_thread = std::ptr::addr_of!(self._steady_clock_timer_reactor_thread);
        // Left uninitialized: the kernel fills the first `nr` events.
        let mut eevt = [MaybeUninit::<EpollEvent>::uninit(); 128];
        let before_pwait = Reactor::now();
        let sigmask = active_sigmask.map_or(std::ptr::null(), |m| m as *const SigsetT);
        // SAFETY: `eevt` is a valid buffer of 128 events.
        let nr = unsafe {
            epoll_pwait(
                self._epollfd.get(),
                eevt.as_mut_ptr().cast::<EpollEvent>(),
                eevt.len() as i32,
                timeout,
                sigmask,
            )
        };
        // SAFETY: `_r` is the reactor that owns this backend; a short access to a counter.
        unsafe { (*self.r())._total_sleep += Reactor::now() - before_pwait };
        if nr == -1 && errno() == EINTR {
            return false; // gdb can cause this
        }
        engine_assert!(nr != -1);
        for slot in &mut eevt[..nr.max(0) as usize] {
            // SAFETY: the kernel wrote the first `nr` events.
            let evt = unsafe { slot.assume_init_mut() };
            let pfd = evt.data as *mut PollableFdState;
            if pfd.is_null() {
                let mut dummy = [0u8; 8];
                // SAFETY: `_r` is the reactor that owns this backend, live while the backend is; it is reached for this access only.
                let _ = unsafe {
                    (*self.r())._notify_eventfd.read(dummy.as_mut_ptr() as *mut c_void, 8)
                };
                continue;
            }
            if evt.data == reactor_thread as usize as u64 {
                let mut dummy = [0u8; 8];
                // SAFETY: the buffer is valid for writes of the length passed.
                let _ = unsafe {
                    self._steady_clock_timer_reactor_thread
                        .read(dummy.as_mut_ptr() as *mut c_void, 8)
                };
                self._highres_timer_pending.store(true, Ordering::Relaxed);
                self._steady_clock_timer_deadline = Itimerspec::default();
                continue;
            }
            // SAFETY: the data of every other registered event is a live descriptor state.
            let pfd = unsafe { &mut *pfd };
            let has_error = evt.events & (EPOLLHUP | EPOLLERR) != 0;
            if has_error {
                // treat the events as required events when error occurs, let
                // send/recv/accept/connect handle the specific error.
                evt.events = pfd.events_requested as u32;
            }
            let events = (evt.events & (EPOLLIN | EPOLLOUT | EPOLLRDHUP)) as i32;
            let events_to_remove = events & !pfd.events_requested;
            self.complete_epoll_event(pfd, events, EPOLLRDHUP as i32);
            if pfd.events_rw {
                // accept() signals normal completions via EPOLLIN, but errors (due to shutdown())
                // via EPOLLOUT|EPOLLHUP, so we have to wait for both EPOLLIN and EPOLLOUT with the
                // same future
                self.complete_epoll_event(pfd, events, (EPOLLIN | EPOLLOUT) as i32);
            } else {
                // Normal processing where EPOLLIN and EPOLLOUT are waited for via different
                // futures.
                self.complete_epoll_event(pfd, events, EPOLLIN as i32);
                self.complete_epoll_event(pfd, events, EPOLLOUT as i32);
            }
            if events_to_remove != 0 {
                pfd.events_epoll &= !events_to_remove;
                evt.events = pfd.events_epoll as u32;
                let op = if evt.events != 0 { EPOLL_CTL_MOD } else { EPOLL_CTL_DEL };
                // SAFETY: plain epoll_ctl on a registered descriptor.
                unsafe { epoll_ctl(self._epollfd.get(), op, pfd.fd.get(), evt) };
            } else if has_error {
                // In the error case, all requested events are cleared (as we handle all
                // requested events on error), so unconditionally delete the fd from epoll,
                // which avoids edge conditions where we otherwise might get stuck spinning.
                pfd.events_epoll = 0;
                // SAFETY: plain epoll_ctl on a registered descriptor; DEL takes no event.
                unsafe {
                    epoll_ctl(self._epollfd.get(), EPOLL_CTL_DEL, pfd.fd.get(), std::ptr::null_mut())
                };
            }
        }
        nr != 0
    }

    /// Runs the expired high resolution timers, whose callbacks may come back into the
    /// backend.
    ///
    /// # Safety
    /// `this` is the backend's root pointer, with no reference to it live.
    unsafe fn complete_hrtimer(this: *mut Self) -> bool {
        // This can be set from either the task quota timer thread, or
        // wait_and_process(), above.
        // SAFETY: the contract above; the flag is an atomic, reached by a shared reborrow
        // that ends before the timers run.
        unsafe {
            let pending = &*std::ptr::addr_of!((*this)._highres_timer_pending);
            if pending.load(Ordering::Relaxed) {
                pending.store(false, Ordering::Relaxed);
                // `_r` is the reactor that owns this backend; called from the reactor's
                // poll or sleep, with no reference to the reactor live.
                Reactor::service_highres_timer((*this)._r);
                return true;
            }
        }
        false
    }

    fn complete_epoll_event(&mut self, pfd: &mut PollableFdState, events: i32, event: i32) {
        if pfd.events_requested & events & event != 0 {
            pfd.events_requested &= !event;
            pfd.events_known &= !event;
            // SAFETY: every descriptor state made by this backend is an `EpollPollableFdState`;
            // its completions are reached through the root pointer.
            unsafe {
                EpollPollableFdState::complete_with(pfd.derived::<EpollPollableFdState>(), event)
            };
        }
    }

    fn get_epoll_future(&mut self, pfd: &mut PollableFdState, event: i32) -> Future<()> {
        if pfd.events_known & event != 0 {
            pfd.events_known &= !event;
            return make_ready_future(());
        }
        pfd.events_rw = event == (EPOLLIN | EPOLLOUT) as i32;
        pfd.events_requested |= event;
        if (pfd.events_epoll & event) != event {
            let ctl = if pfd.events_epoll != 0 { EPOLL_CTL_MOD } else { EPOLL_CTL_ADD };
            pfd.events_epoll |= event;
            // The kernel keeps the root pointer (see `PollableFdState::root`).
            let mut eevt = EpollEvent { events: pfd.events_epoll as u32, data: pfd.root() as u64 };
            // SAFETY: plain epoll_ctl on a descriptor owned by `pfd`.
            let r = unsafe { epoll_ctl(self._epollfd.get(), ctl, pfd.fd.get(), &mut eevt) };
            engine_assert!(r == 0);
            self._need_epoll_events = true;
        }

        // SAFETY: every descriptor state made by this backend is an `EpollPollableFdState`.
        unsafe {
            EpollPollableFdState::get_completion_future(
                pfd.derived::<EpollPollableFdState>(),
                event,
            )
        }
    }
}

impl Drop for ReactorBackendEpoll {
    fn drop(&mut self) {}
}

#[repr(C)]
/// The descriptor state of the epoll backend: the base state and one completion each for
/// `POLLIN`, `POLLOUT` and `POLLRDHUP` waiters.
pub struct EpollPollableFdState {
    /// The base state.
    pub base: PollableFdState,
    _pollin: PollableFdStateCompletion,
    _pollout: PollableFdStateCompletion,
    _pollrdhup: PollableFdStateCompletion,
}

impl EpollPollableFdState {
    unsafe fn get_desc(this: *mut Self, events: i32) -> *mut PollableFdStateCompletion {
        // SAFETY: see AioPollableFdState; no reference is formed.
        unsafe {
            if events & EPOLLIN as i32 != 0 {
                return std::ptr::addr_of_mut!((*this)._pollin);
            }
            if events & EPOLLOUT as i32 != 0 {
                return std::ptr::addr_of_mut!((*this)._pollout);
            }
            std::ptr::addr_of_mut!((*this)._pollrdhup)
        }
    }
    /// A state for `fd` with speculation `speculate`.
    pub fn new(fd: FileDesc, speculate: Speculation) -> Box<Self> {
        Box::new(EpollPollableFdState {
            base: PollableFdState::new(fd, speculate),
            _pollin: PollableFdStateCompletion::default(),
            _pollout: PollableFdStateCompletion::default(),
            _pollrdhup: PollableFdStateCompletion::default(),
        })
    }
    /// # Safety
    /// `this` is the root pointer of a live state of this type.
    pub unsafe fn get_completion_future(this: *mut Self, event: i32) -> Future<()> {
        // SAFETY: see AioPollableFdState; the completion is reborrowed for this call only.
        unsafe {
            let desc = Self::get_desc(this, event);
            *desc = PollableFdStateCompletion::default();
            (*desc).get_future()
        }
    }

    /// # Safety
    /// `this` is the root pointer of a live state of this type.
    pub unsafe fn complete_with(this: *mut Self, event: i32) {
        // SAFETY: see AioPollableFdState; `desc` is one of this state's completions, live and
        // not otherwise referenced.
        unsafe {
            <PollableFdStateCompletion as KernelCompletionImpl>::complete_with(
                Self::get_desc(this, event),
                event as isize,
            )
        };
    }
}

// The entries of `BackendEntries` (see ReactorBackendAio's).
impl ReactorBackendEpoll {
    unsafe fn reap_kernel_completions(this: *mut Self) -> bool {
        // epoll does not have a separate submission stage, and just
        // calls epoll_ctl everytime it needs, so this method and
        // kernel_submit_work are essentially the same. Ordering also
        // doesn't matter much. wait_and_process is actually completing,
        // but we prefer to call it in kernel_submit_work because the
        // reactor register two pollers for completions and one for submission,
        // since completion is cheaper for other backends like aio. This avoids
        // calling epoll_wait twice.
        //
        // We will only reap the io completions
        // SAFETY: the table's contract; the storage completions only resolve promises.
        unsafe { (*this)._storage_context.reap_completions(true) }
    }

    unsafe fn kernel_submit_work(this: *mut Self) -> bool {
        let mut result = false;
        // SAFETY: the table's contract. The epoll completions only resolve promises, so the
        // reborrow `wait_and_process` takes is not re-entered; the timers run after it.
        unsafe {
            if let Err(e) = (*this)._storage_context.submit_work() {
                exception_in_reactor_main_loop(e);
            }
            if (*this)._need_epoll_events {
                result |= (*this).wait_and_process(0, None);
            }

            result |= Self::complete_hrtimer(this);
        }

        result
    }

    unsafe fn wait_and_process_events(this: *mut Self, active_sigmask: Option<&SigsetT>) {
        // SAFETY: as in kernel_submit_work.
        unsafe {
            (*this).wait_and_process(-1, active_sigmask);
            Self::complete_hrtimer(this);
        }
    }

    unsafe fn reset_preemption_monitor(this: *mut Self) {
        // SAFETY: the table's contract; `_r` is the reactor that owns this backend; a store
        // to an atomic.
        unsafe {
            (*std::ptr::addr_of!((*(*this)._r)._preemption_monitor))
                .head
                .store(0, Ordering::Relaxed)
        };
    }

    unsafe fn request_preemption(this: *mut Self) {
        // SAFETY: as above.
        unsafe {
            (*std::ptr::addr_of!((*(*this)._r)._preemption_monitor))
                .head
                .store(1, Ordering::Relaxed)
        };
    }
}

impl ReactorBackend for ReactorBackendEpoll {
    fn entries(&self) -> &'static BackendEntries {
        backend_entries!(ReactorBackendEpoll)
    }

    fn kernel_events_can_sleep(&self) -> bool {
        self._storage_context.can_sleep()
    }

    fn signal_received(&self, signo: i32, siginfo: *mut SiginfoT, ignore: *mut c_void) {
        if crate::engine::core::reactor::engine_is_ready() {
            crate::engine::core::reactor::Signals::action(signo, siginfo, ignore);
        } else {
            crate::engine::core::reactor::Signals::failed_to_handle(signo);
        }
    }

    fn start_handling_signal(&self) {
        // The epoll backend uses signals for the high resolution timer. That is used for thread_scheduling_group, so we
        // request preemption so when we receive a signal.
        // SAFETY: as in request_preemption; signal-safe (a store to an atomic).
        unsafe {
            (*std::ptr::addr_of!((*self.r())._preemption_monitor)).head.store(1, Ordering::Relaxed)
        };
    }
}

// The I/O entries of `BackendEntries` (the table's contract). The polls only change the
// epoll set and the socket operations are the reactor's generic ones, which come back here
// through the table; none runs a completion, so the short reborrows below overlap nothing.
impl ReactorBackendEpoll {
    unsafe fn readable(this: *mut Self, fd: *mut PollableFdState) -> Future<()> {
        // SAFETY: see above; one call that only changes the epoll set.
        unsafe { (*this).get_epoll_future(&mut *fd, EPOLLIN as i32) }
    }

    unsafe fn writeable(this: *mut Self, fd: *mut PollableFdState) -> Future<()> {
        // SAFETY: as in readable.
        unsafe { (*this).get_epoll_future(&mut *fd, EPOLLOUT as i32) }
    }

    unsafe fn readable_or_writeable(this: *mut Self, fd: *mut PollableFdState) -> Future<()> {
        // SAFETY: as in readable.
        unsafe { (*this).get_epoll_future(&mut *fd, (EPOLLIN | EPOLLOUT) as i32) }
    }

    unsafe fn poll_rdhup(this: *mut Self, fd: *mut PollableFdState) -> Future<()> {
        // SAFETY: as in readable.
        unsafe { (*this).get_epoll_future(&mut *fd, POLLRDHUP) }
    }

    unsafe fn forget(this: *mut Self, fd: *mut PollableFdState) {
        // SAFETY: the table's contract; short reads of the base and of the epoll descriptor.
        unsafe {
            if (*fd).events_epoll != 0 {
                // plain epoll_ctl on a registered descriptor
                epoll_ctl(
                    (*this)._epollfd.get(),
                    EPOLL_CTL_DEL,
                    (*fd).fd.get(),
                    std::ptr::null_mut(),
                );
            }
        }
        // SAFETY: the state was allocated by `make_pollable_fd_state` as a boxed
        // `EpollPollableFdState`, and `fd` is its root pointer.
        drop(unsafe { Box::from_raw(fd.cast::<EpollPollableFdState>()) });
    }

    unsafe fn accept(
        _this: *mut Self,
        listenfd: *mut PollableFdState,
    ) -> Future<(PollableFd, SocketAddress)> {
        // SAFETY: forwarded (the table's contract).
        unsafe { Reactor::do_accept(listenfd) }
    }

    unsafe fn connect(
        _this: *mut Self,
        fd: *mut PollableFdState,
        sa: &mut SocketAddress,
    ) -> Future<()> {
        // SAFETY: forwarded.
        unsafe { Reactor::do_connect(fd, sa) }
    }

    unsafe fn shutdown(
        _this: *mut Self,
        fd: *mut PollableFdState,
        how: i32,
    ) -> Result<(), ExceptionPtr> {
        // SAFETY: the table's contract; a shared reborrow of the descriptor for one syscall.
        unsafe { (*fd).fd.shutdown(how) }
    }

    unsafe fn read(
        _this: *mut Self,
        fd: *mut PollableFdState,
        buffer: *mut c_void,
        len: usize,
    ) -> Future<usize> {
        // SAFETY: forwarded.
        unsafe { Reactor::do_read(fd, buffer, len) }
    }

    unsafe fn recvmsg(
        _this: *mut Self,
        fd: *mut PollableFdState,
        iov: &Vec<Iovec>,
    ) -> Future<usize> {
        // SAFETY: forwarded.
        unsafe { Reactor::do_recvmsg(fd, iov) }
    }

    unsafe fn read_some(
        _this: *mut Self,
        fd: *mut PollableFdState,
        ba: *mut dyn BufferAllocator,
    ) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: forwarded.
        unsafe { Reactor::do_read_some(fd, ba) }
    }

    unsafe fn send(
        _this: *mut Self,
        fd: *mut PollableFdState,
        buffer: *const c_void,
        len: usize,
    ) -> Future<usize> {
        // SAFETY: forwarded.
        unsafe { Reactor::do_send(fd, buffer, len) }
    }

    unsafe fn sendmsg(_this: *mut Self, fd: *mut PollableFdState, p: &mut Packet) -> Future<usize> {
        // SAFETY: forwarded.
        unsafe { Reactor::do_sendmsg(fd, p) }
    }

    unsafe fn recv_some(
        _this: *mut Self,
        fd: *mut PollableFdState,
        ba: *mut dyn BufferAllocator,
    ) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: forwarded.
        unsafe { Reactor::do_recv_some(fd, ba) }
    }

    unsafe fn start_tick(this: *mut Self) {
        // SAFETY: the table's contract.
        let r = unsafe { (*this)._r };
        // SAFETY: `_r` is the reactor that owns this backend; only descriptors and the
        // addresses of atomics are taken (with the provenance of the reactor and of the
        // backend's root pointer), which outlive the thread (stop_tick joins it).
        let state = unsafe {
            EpollTimerThreadState {
                shard: (*r)._id,
                task_quota_timer: (*r)._task_quota_timer.get(),
                steady_clock_timer: (*this)._steady_clock_timer_timer_thread.get(),
                dying: std::ptr::addr_of!((*r)._dying),
                highres_timer_pending: std::ptr::addr_of!((*this)._highres_timer_pending),
                preemption_monitor: std::ptr::addr_of!((*r)._preemption_monitor),
            }
        };
        let handle = std::thread::spawn(move || state.run());

        let sp = SchedParam { sched_priority: 1 };
        use std::os::unix::thread::JoinHandleExt;
        // SAFETY: the handle refers to a live thread.
        let sched_ok =
            unsafe { pthread_setschedparam(handle.as_pthread_t() as usize, SCHED_FIFO, &sp) };
        // SAFETY: the table's contract; one field write.
        unsafe { (*this)._task_quota_timer_thread = Some(handle) };
        // SAFETY: `_r` is the reactor that owns this backend; a short read.
        if sched_ok != 0 && unsafe { (*r)._id } == 0 {
            engine_logger().warn(FormatInfo::new("Unable to set SCHED_FIFO scheduling policy for timer thread; latency impact possible. Try adding CAP_SYS_NICE"), format_args!(
                "Unable to set SCHED_FIFO scheduling policy for timer thread; latency impact possible. Try adding CAP_SYS_NICE"
            ));
        }
    }

    unsafe fn stop_tick(this: *mut Self) {
        // SAFETY: the table's contract.
        let r = unsafe { (*this)._r };
        // SAFETY: `_r` is the reactor that owns this backend; short accesses to an atomic and
        // to a descriptor.
        unsafe {
            (*std::ptr::addr_of!((*r)._dying)).store(true, Ordering::Relaxed);
            if let Err(e) = (*std::ptr::addr_of_mut!((*r)._task_quota_timer)).timerfd_settime(
                0,
                &posix::to_relative_itimerspec(Duration::from_nanos(1), Duration::from_millis(1)),
            ) {
                // stop_tick() runs from the reactor destructor.
                exception_in_noexcept(e);
            } // Make the timer fire soon
        }
        // SAFETY: the table's contract; one field access.
        if let Some(t) = unsafe { (*this)._task_quota_timer_thread.take() } {
            let _ = t.join();
        }
    }

    unsafe fn arm_highres_timer(this: *mut Self, its: &Itimerspec) {
        // SAFETY: the table's contract; one field write and a shared reborrow of the
        // descriptor for one syscall.
        unsafe {
            (*this)._steady_clock_timer_deadline = *its;
            // An error arming the high resolution timer aborts: enabling a timer cannot fail.
            if let Err(e) = (*std::ptr::addr_of!((*this)._steady_clock_timer_timer_thread))
                .timerfd_settime(TFD_TIMER_ABSTIME, its)
            {
                exception_in_noexcept(e);
            }
        }
    }

    unsafe fn make_pollable_fd_state(
        _this: *mut Self,
        fd: FileDesc,
        speculate: Speculation,
    ) -> PollableFdStatePtr {
        let p = Box::into_raw(EpollPollableFdState::new(fd, speculate)).cast::<PollableFdState>();
        // SAFETY: a fresh state from `Box::into_raw` of the derived state (`#[repr(C)]`, base
        // first): `p` keeps the provenance of the whole derived state, and is its root.
        unsafe {
            (*p)._this = p;
            PollableFdStatePtr::new(p)
        }
    }
}

// io_uring: the part of the ring protocol the backend uses, written over the
// io_uring_setup, io_uring_enter and io_uring_register system calls.

/// The io_uring kernel ABI and the minimal ring management the io_uring backend uses.
pub mod uring {
    use super::*;
    use std::sync::atomic::{AtomicU32, fence};

    pub use crate::engine::core::internal::linux_abi::nr::{
        IO_URING_ENTER as SYS_IO_URING_ENTER, IO_URING_REGISTER as SYS_IO_URING_REGISTER,
        IO_URING_SETUP as SYS_IO_URING_SETUP,
    };

    /// Opcode: no operation.
    pub const IORING_OP_NOP: u8 = 0;
    /// Opcode: vectored read.
    pub const IORING_OP_READV: u8 = 1;
    /// Opcode: vectored write.
    pub const IORING_OP_WRITEV: u8 = 2;
    /// Opcode: `fsync`/`fdatasync`.
    pub const IORING_OP_FSYNC: u8 = 3;
    /// Opcode: wait for poll events.
    pub const IORING_OP_POLL_ADD: u8 = 6;
    /// Opcode: remove a poll.
    pub const IORING_OP_POLL_REMOVE: u8 = 7;
    /// Opcode: `sendmsg`.
    pub const IORING_OP_SENDMSG: u8 = 9;
    /// Opcode: `recvmsg`.
    pub const IORING_OP_RECVMSG: u8 = 10;
    /// Opcode: `accept`.
    pub const IORING_OP_ACCEPT: u8 = 13;
    /// Opcode: cancel a request.
    pub const IORING_OP_ASYNC_CANCEL: u8 = 14;
    /// Opcode: `connect`.
    pub const IORING_OP_CONNECT: u8 = 16;
    /// Opcode: read.
    pub const IORING_OP_READ: u8 = 22;
    /// Opcode: write.
    pub const IORING_OP_WRITE: u8 = 23;
    /// Opcode: `send`.
    pub const IORING_OP_SEND: u8 = 26;
    /// Opcode: `recv`.
    pub const IORING_OP_RECV: u8 = 27;

    /// Feature: the SQ and CQ rings share one mapping.
    pub const IORING_FEAT_SINGLE_MMAP: u32 = 1 << 0;
    /// Feature: completions are never dropped.
    pub const IORING_FEAT_NODROP: u32 = 1 << 1;
    /// Feature: submitted data is stable once `io_uring_enter` returns.
    pub const IORING_FEAT_SUBMIT_STABLE: u32 = 1 << 2;

    /// Setup flag: a kernel thread polls the submission queue.
    pub const IORING_SETUP_SQPOLL: u32 = 1 << 1;
    /// SQ ring flag: the polling thread needs a wake-up.
    pub const IORING_SQ_NEED_WAKEUP: u32 = 1 << 0;
    /// SQ ring flag: the completion queue overflowed.
    pub const IORING_SQ_CQ_OVERFLOW: u32 = 1 << 1;
    /// SQ ring flag: task work is pending.
    pub const IORING_SQ_TASKRUN: u32 = 1 << 2;
    /// Setup flag: busy-poll for completions.
    pub const IORING_SETUP_IOPOLL: u32 = 1 << 0;
    /// `io_uring_enter` flag: wait for completions.
    pub const IORING_ENTER_GETEVENTS: u32 = 1 << 0;
    /// `io_uring_enter` flag: wake the polling thread.
    pub const IORING_ENTER_SQ_WAKEUP: u32 = 1 << 1;
    /// fsync flag: `fdatasync` semantics.
    pub const IORING_FSYNC_DATASYNC: u32 = 1 << 0;
    /// `io_uring_register` opcode: probe the supported operations.
    pub const IORING_REGISTER_PROBE: u32 = 8;
    /// Probe flag: the operation is supported.
    pub const IO_URING_OP_SUPPORTED: u16 = 1 << 0;

    /// `mmap` offset of the SQ ring.
    pub const IORING_OFF_SQ_RING: i64 = 0;
    /// `mmap` offset of the CQ ring.
    pub const IORING_OFF_CQ_RING: i64 = 0x8000000;
    /// `mmap` offset of the submission entries.
    pub const IORING_OFF_SQES: i64 = 0x10000000;

    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;
    const MAP_SHARED: i32 = 1;
    const MAP_POPULATE: i32 = 0x8000;
    const MAP_FAILED: *mut c_void = !0usize as *mut c_void;
    const MADV_DONTFORK: i32 = 10;
    const NSIG_BYTES: usize = 8; // _NSIG / 8

    unsafe extern "C" {
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            off: i64,
        ) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> i32;
        fn madvise(addr: *mut c_void, len: usize, advice: i32) -> i32;
        fn close(fd: i32) -> i32;
    }

    /// Offsets of the SQ ring fields in its mapping.
    #[repr(C)]
    #[derive(Clone, Copy, Default, Debug)]
    pub struct IoSqringOffsets {
        /// Head index.
        pub head: u32,
        /// Tail index.
        pub tail: u32,
        /// Index mask.
        pub ring_mask: u32,
        /// Number of entries.
        pub ring_entries: u32,
        /// Ring flags.
        pub flags: u32,
        /// Dropped entries counter.
        pub dropped: u32,
        /// Index array.
        pub array: u32,
        /// Reserved.
        pub resv1: u32,
        /// User-provided ring address (unused).
        pub user_addr: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default, Debug)]
    /// Offsets of the CQ ring fields in its mapping.
    pub struct IoCqringOffsets {
        /// Head index.
        pub head: u32,
        /// Tail index.
        pub tail: u32,
        /// Index mask.
        pub ring_mask: u32,
        /// Number of entries.
        pub ring_entries: u32,
        /// Overflow counter.
        pub overflow: u32,
        /// Completion entries.
        pub cqes: u32,
        /// Ring flags.
        pub flags: u32,
        /// Reserved.
        pub resv1: u32,
        /// User-provided ring address (unused).
        pub user_addr: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default, Debug)]
    /// The parameters of `io_uring_setup`, filled in by the kernel.
    pub struct IoUringParams {
        /// Number of submission entries.
        pub sq_entries: u32,
        /// Number of completion entries.
        pub cq_entries: u32,
        /// `IORING_SETUP_*` flags.
        pub flags: u32,
        /// CPU of the SQ polling thread.
        pub sq_thread_cpu: u32,
        /// Idle time of the SQ polling thread.
        pub sq_thread_idle: u32,
        /// `IORING_FEAT_*` features.
        pub features: u32,
        /// Ring whose work queue to share.
        pub wq_fd: u32,
        /// Reserved.
        pub resv: [u32; 3],
        /// SQ ring offsets.
        pub sq_off: IoSqringOffsets,
        /// CQ ring offsets.
        pub cq_off: IoCqringOffsets,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default, Debug)]
    /// A submission queue entry.
    pub struct IoUringSqe {
        /// The operation.
        pub opcode: u8,
        /// `IOSQE_*` flags.
        pub flags: u8,
        /// I/O priority.
        pub ioprio: u16,
        /// The descriptor.
        pub fd: i32,
        /// The file offset (or second address).
        pub off: u64,
        /// The buffer or iovec array.
        pub addr: u64,
        /// The length (or iovec count).
        pub len: u32,
        /// The per-operation flags (`rw_flags`, `fsync_flags`, poll events, `msg_flags`,
        /// `accept_flags`...).
        pub rw_flags: u32,
        /// User data returned in the completion.
        pub user_data: u64,
        /// Registered buffer index.
        pub buf_index: u16,
        /// Credentials personality.
        pub personality: u16,
        /// Splice input descriptor.
        pub splice_fd_in: i32,
        /// Padding.
        pub __pad2: [u64; 2],
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default, Debug)]
    /// A completion queue entry.
    pub struct IoUringCqe {
        /// The submission's user data.
        pub user_data: u64,
        /// The result: a byte count or a negative errno.
        pub res: i32,
        /// `IORING_CQE_F_*` flags.
        pub flags: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    /// One operation of an [`IoUringProbe`].
    pub struct IoUringProbeOp {
        /// The opcode.
        pub op: u8,
        /// Reserved.
        pub resv: u8,
        /// `IO_URING_OP_*` flags.
        pub flags: u16,
        /// Reserved.
        pub resv2: u32,
    }

    /// Number of operations a probe can describe.
    pub const PROBE_OPS: usize = 256;

    #[repr(C)]
    /// The operations the kernel supports (`IORING_REGISTER_PROBE`).
    pub struct IoUringProbe {
        /// The highest supported opcode.
        pub last_op: u8,
        /// Number of valid entries of `ops`.
        pub ops_len: u8,
        /// Reserved.
        pub resv: u16,
        /// Reserved.
        pub resv2: [u32; 3],
        /// One entry per opcode.
        pub ops: [IoUringProbeOp; PROBE_OPS],
    }

    /// The mapped submission queue.
    pub struct IoUringSq {
        /// Kernel head index.
        pub khead: *mut u32,
        /// Kernel tail index.
        pub ktail: *mut u32,
        /// Kernel index mask.
        pub kring_mask: *mut u32,
        /// Kernel entry count.
        pub kring_entries: *mut u32,
        /// Kernel ring flags.
        pub kflags: *mut u32,
        /// Kernel dropped counter.
        pub kdropped: *mut u32,
        /// Index array.
        pub array: *mut u32,
        /// The submission entries.
        pub sqes: *mut IoUringSqe,
        /// First entry not yet submitted.
        pub sqe_head: u32,
        /// Next entry to hand out.
        pub sqe_tail: u32,
        /// Size of the ring mapping.
        pub ring_sz: usize,
        /// The ring mapping.
        pub ring_ptr: *mut c_void,
        /// Index mask.
        pub ring_mask: u32,
        /// Number of entries.
        pub ring_entries: u32,
    }

    /// The mapped completion queue.
    pub struct IoUringCq {
        /// Kernel head index.
        pub khead: *mut u32,
        /// Kernel tail index.
        pub ktail: *mut u32,
        /// Kernel index mask.
        pub kring_mask: *mut u32,
        /// Kernel entry count.
        pub kring_entries: *mut u32,
        /// Kernel ring flags.
        pub kflags: *mut u32,
        /// Kernel overflow counter.
        pub koverflow: *mut u32,
        /// The completion entries.
        pub cqes: *mut IoUringCqe,
        /// Size of the ring mapping.
        pub ring_sz: usize,
        /// The ring mapping.
        pub ring_ptr: *mut c_void,
        /// Index mask.
        pub ring_mask: u32,
        /// Number of entries.
        pub ring_entries: u32,
    }

    /// An io_uring instance: its descriptor and its two mapped queues.
    pub struct IoUring {
        /// The submission queue.
        pub sq: IoUringSq,
        /// The completion queue.
        pub cq: IoUringCq,
        /// The setup flags.
        pub flags: u32,
        /// The ring descriptor.
        pub ring_fd: i32,
        /// The kernel features.
        pub features: u32,
    }

    #[inline]
    fn sys_ret(r: i64) -> i32 {
        if r < 0 { -errno() } else { r as i32 }
    }

    /// The `io_uring_setup` system call; returns the ring descriptor or `-errno`.
    pub fn io_uring_setup(entries: u32, p: &mut IoUringParams) -> i32 {
        // SAFETY: `p` is a valid io_uring_params.
        sys_ret(unsafe { syscall(SYS_IO_URING_SETUP, entries, p as *mut IoUringParams) })
    }

    /// The `io_uring_enter` system call; returns its result or `-errno`.
    ///
    /// # Safety
    /// `sig` is null or points to a signal set; `fd` is a ring.
    pub unsafe fn io_uring_enter(
        fd: i32,
        to_submit: u32,
        min_complete: u32,
        flags: u32,
        sig: *const SigsetT,
    ) -> i32 {
        // SAFETY: `sig` is null or a valid signal set.
        sys_ret(unsafe {
            syscall(SYS_IO_URING_ENTER, fd, to_submit, min_complete, flags, sig, NSIG_BYTES)
        })
    }

    /// The `io_uring_register` system call; returns its result or `-errno`.
    ///
    /// # Safety
    /// `arg` points to `nr_args` arguments of the type `opcode` expects.
    pub unsafe fn io_uring_register(fd: i32, opcode: u32, arg: *mut c_void, nr_args: u32) -> i32 {
        // SAFETY: `arg` is valid for the given opcode.
        sys_ret(unsafe { syscall(SYS_IO_URING_REGISTER, fd, opcode, arg, nr_args) })
    }

    #[inline]
    unsafe fn load_acquire(p: *const u32) -> u32 {
        // SAFETY: `p` points into a ring shared with the kernel; it is 4-byte aligned.
        unsafe { (*(p as *const AtomicU32)).load(Ordering::Acquire) }
    }

    #[inline]
    unsafe fn store_release(p: *mut u32, v: u32) {
        // SAFETY: as above.
        unsafe { (*(p as *const AtomicU32)).store(v, Ordering::Release) }
    }

    unsafe fn io_uring_unmap_rings(sq: &mut IoUringSq, cq: &mut IoUringCq) {
        // SAFETY: the rings were mapped by `io_uring_mmap`.
        unsafe {
            munmap(sq.ring_ptr, sq.ring_sz);
            if !cq.ring_ptr.is_null() && cq.ring_ptr != sq.ring_ptr {
                munmap(cq.ring_ptr, cq.ring_sz);
            }
        }
    }

    fn io_uring_mmap(fd: i32, p: &IoUringParams, sq: &mut IoUringSq, cq: &mut IoUringCq) -> i32 {
        sq.ring_sz = p.sq_off.array as usize + p.sq_entries as usize * std::mem::size_of::<u32>();
        cq.ring_sz =
            p.cq_off.cqes as usize + p.cq_entries as usize * std::mem::size_of::<IoUringCqe>();
        if p.features & IORING_FEAT_SINGLE_MMAP != 0 {
            if cq.ring_sz > sq.ring_sz {
                sq.ring_sz = cq.ring_sz;
            }
            cq.ring_sz = sq.ring_sz;
        }
        // SAFETY: mapping the rings of a freshly set up io_uring.
        unsafe {
            sq.ring_ptr = mmap(
                std::ptr::null_mut(),
                sq.ring_sz,
                PROT_READ | PROT_WRITE,
                MAP_SHARED | MAP_POPULATE,
                fd,
                IORING_OFF_SQ_RING,
            );
            if sq.ring_ptr == MAP_FAILED {
                return -errno();
            }
            if p.features & IORING_FEAT_SINGLE_MMAP != 0 {
                cq.ring_ptr = sq.ring_ptr;
            } else {
                cq.ring_ptr = mmap(
                    std::ptr::null_mut(),
                    cq.ring_sz,
                    PROT_READ | PROT_WRITE,
                    MAP_SHARED | MAP_POPULATE,
                    fd,
                    IORING_OFF_CQ_RING,
                );
                if cq.ring_ptr == MAP_FAILED {
                    cq.ring_ptr = std::ptr::null_mut();
                    let e = -errno();
                    io_uring_unmap_rings(sq, cq);
                    return e;
                }
            }
            let sb = sq.ring_ptr as *mut u8;
            sq.khead = sb.add(p.sq_off.head as usize) as *mut u32;
            sq.ktail = sb.add(p.sq_off.tail as usize) as *mut u32;
            sq.kring_mask = sb.add(p.sq_off.ring_mask as usize) as *mut u32;
            sq.kring_entries = sb.add(p.sq_off.ring_entries as usize) as *mut u32;
            sq.kflags = sb.add(p.sq_off.flags as usize) as *mut u32;
            sq.kdropped = sb.add(p.sq_off.dropped as usize) as *mut u32;
            sq.array = sb.add(p.sq_off.array as usize) as *mut u32;

            let sqes_sz = p.sq_entries as usize * std::mem::size_of::<IoUringSqe>();
            let sqes = mmap(
                std::ptr::null_mut(),
                sqes_sz,
                PROT_READ | PROT_WRITE,
                MAP_SHARED | MAP_POPULATE,
                fd,
                IORING_OFF_SQES,
            );
            if sqes == MAP_FAILED {
                let e = -errno();
                io_uring_unmap_rings(sq, cq);
                return e;
            }
            sq.sqes = sqes as *mut IoUringSqe;

            let cb = cq.ring_ptr as *mut u8;
            cq.khead = cb.add(p.cq_off.head as usize) as *mut u32;
            cq.ktail = cb.add(p.cq_off.tail as usize) as *mut u32;
            cq.kring_mask = cb.add(p.cq_off.ring_mask as usize) as *mut u32;
            cq.kring_entries = cb.add(p.cq_off.ring_entries as usize) as *mut u32;
            cq.koverflow = cb.add(p.cq_off.overflow as usize) as *mut u32;
            cq.cqes = cb.add(p.cq_off.cqes as usize) as *mut IoUringCqe;
            cq.kflags = if p.cq_off.flags != 0 {
                cb.add(p.cq_off.flags as usize) as *mut u32
            } else {
                std::ptr::null_mut()
            };

            sq.ring_mask = *sq.kring_mask;
            sq.ring_entries = *sq.kring_entries;
            cq.ring_mask = *cq.kring_mask;
            cq.ring_entries = *cq.kring_entries;

            // Directly map SQ slots to SQEs
            for index in 0..sq.ring_entries {
                *sq.array.add(index as usize) = index;
            }
        }
        0
    }

    /// Creates a ring with `entries` entries and maps its queues.
    ///
    /// # Errors
    ///
    /// Returns `-errno` of the setup or mapping.
    pub fn io_uring_queue_init_params(entries: u32, p: &mut IoUringParams) -> Result<IoUring, i32> {
        let fd = io_uring_setup(entries, p);
        if fd < 0 {
            return Err(fd);
        }
        let mut sq = IoUringSq {
            khead: std::ptr::null_mut(),
            ktail: std::ptr::null_mut(),
            kring_mask: std::ptr::null_mut(),
            kring_entries: std::ptr::null_mut(),
            kflags: std::ptr::null_mut(),
            kdropped: std::ptr::null_mut(),
            array: std::ptr::null_mut(),
            sqes: std::ptr::null_mut(),
            sqe_head: 0,
            sqe_tail: 0,
            ring_sz: 0,
            ring_ptr: std::ptr::null_mut(),
            ring_mask: 0,
            ring_entries: 0,
        };
        let mut cq = IoUringCq {
            khead: std::ptr::null_mut(),
            ktail: std::ptr::null_mut(),
            kring_mask: std::ptr::null_mut(),
            kring_entries: std::ptr::null_mut(),
            kflags: std::ptr::null_mut(),
            koverflow: std::ptr::null_mut(),
            cqes: std::ptr::null_mut(),
            ring_sz: 0,
            ring_ptr: std::ptr::null_mut(),
            ring_mask: 0,
            ring_entries: 0,
        };
        let ret = io_uring_mmap(fd, p, &mut sq, &mut cq);
        if ret != 0 {
            // SAFETY: closing the descriptor returned by io_uring_setup.
            unsafe { close(fd) };
            return Err(ret);
        }
        Ok(IoUring { sq, cq, flags: p.flags, ring_fd: fd, features: p.features })
    }

    /// Unmaps the ring's queues and closes its descriptor.
    pub fn io_uring_queue_exit(ring: &mut IoUring) {
        let sqes_sz = ring.sq.ring_entries as usize * std::mem::size_of::<IoUringSqe>();
        // SAFETY: unmapping what io_uring_mmap mapped and closing the ring.
        unsafe {
            munmap(ring.sq.sqes as *mut c_void, sqes_sz);
            io_uring_unmap_rings(&mut ring.sq, &mut ring.cq);
            close(ring.ring_fd);
        }
    }

    /// Excludes the ring's mappings from child processes; returns 0 or `-errno`.
    pub fn io_uring_ring_dontfork(ring: &mut IoUring) -> i32 {
        if ring.sq.ring_ptr.is_null() || ring.sq.sqes.is_null() || ring.cq.ring_ptr.is_null() {
            return -EINVAL;
        }
        let len = ring.sq.ring_entries as usize * std::mem::size_of::<IoUringSqe>();
        // SAFETY: advising mappings owned by the ring.
        unsafe {
            if madvise(ring.sq.sqes as *mut c_void, len, MADV_DONTFORK) != 0 {
                return -errno();
            }
            if madvise(ring.sq.ring_ptr, ring.sq.ring_sz, MADV_DONTFORK) != 0 {
                return -errno();
            }
            if ring.cq.ring_ptr != ring.sq.ring_ptr
                && madvise(ring.cq.ring_ptr, ring.cq.ring_sz, MADV_DONTFORK) != 0
            {
                return -errno();
            }
        }
        0
    }

    /// The kernel's list of supported operations, or `None` if probing fails.
    pub fn io_uring_get_probe_ring(ring: &mut IoUring) -> Option<Box<IoUringProbe>> {
        // SAFETY: the probe is plain data for which all-zero bytes is a valid value.
        let mut probe: Box<IoUringProbe> = unsafe { Box::new(std::mem::zeroed()) };
        // SAFETY: `probe` holds the room for the operations the kernel writes.
        let r = unsafe {
            io_uring_register(
                ring.ring_fd,
                IORING_REGISTER_PROBE,
                &mut *probe as *mut IoUringProbe as *mut c_void,
                PROBE_OPS as u32,
            )
        };
        if r >= 0 { Some(probe) } else { None }
    }

    /// Frees a probe.
    pub fn io_uring_free_probe(_probe: Box<IoUringProbe>) {}

    /// Whether the probe reports `op` as supported.
    pub fn io_uring_opcode_supported(p: &IoUringProbe, op: u8) -> bool {
        if op > p.last_op {
            return false;
        }
        p.ops[op as usize].flags & IO_URING_OP_SUPPORTED != 0
    }

    /// A free submission entry, or null if the submission queue is full.
    pub fn io_uring_get_sqe(ring: &mut IoUring) -> *mut IoUringSqe {
        let sq = &mut ring.sq;
        // SAFETY: `khead` points into the mapped SQ ring.
        let head = unsafe { load_acquire(sq.khead) };
        let next = sq.sqe_tail.wrapping_add(1);
        if next.wrapping_sub(head) <= sq.ring_entries {
            // SAFETY: the index is masked into the SQE array.
            let sqe = unsafe { sq.sqes.add((sq.sqe_tail & sq.ring_mask) as usize) };
            sq.sqe_tail = next;
            return sqe;
        }
        std::ptr::null_mut()
    }

    /// The number of submission entries
    /// `io_uring_get_sqe` can still hand out.
    pub fn io_uring_sq_space_left(ring: &IoUring) -> u32 {
        let sq = &ring.sq;
        // SAFETY: `khead` points into the mapped SQ ring.
        let head = unsafe { load_acquire(sq.khead) };
        sq.ring_entries - sq.sqe_tail.wrapping_sub(head)
    }

    fn io_uring_flush_sq(ring: &mut IoUring) -> u32 {
        let sq = &mut ring.sq;
        let tail = sq.sqe_tail;
        if sq.sqe_head != tail {
            sq.sqe_head = tail;
            // SAFETY: `ktail` points into the mapped SQ ring.
            unsafe { store_release(sq.ktail, tail) };
        }
        // SAFETY: `khead` points into the mapped SQ ring.
        tail.wrapping_sub(unsafe { *sq.khead })
    }

    fn sq_ring_needs_enter(ring: &IoUring, submit: u32, flags: &mut u32) -> bool {
        if submit == 0 {
            return false;
        }
        if ring.flags & IORING_SETUP_SQPOLL == 0 {
            return true;
        }
        fence(Ordering::SeqCst);
        // SAFETY: `kflags` points into the mapped SQ ring.
        if unsafe { (*(ring.sq.kflags as *const AtomicU32)).load(Ordering::Relaxed) }
            & IORING_SQ_NEED_WAKEUP
            != 0
        {
            *flags |= IORING_ENTER_SQ_WAKEUP;
            return true;
        }
        false
    }

    /// Whether completions wait in the kernel's overflow list, or
    /// task work must run for them to be posted.
    fn cq_ring_needs_flush(ring: &IoUring) -> bool {
        // SAFETY: `kflags` points into the mapped SQ ring.
        unsafe {
            (*(ring.sq.kflags as *const AtomicU32)).load(Ordering::Relaxed)
                & (IORING_SQ_CQ_OVERFLOW | IORING_SQ_TASKRUN)
                != 0
        }
    }

    /// Whether the kernel must be entered to post completions.
    fn cq_ring_needs_enter(ring: &IoUring) -> bool {
        ring.flags & IORING_SETUP_IOPOLL != 0 || cq_ring_needs_flush(ring)
    }

    /// Submits the prepared entries; returns the number submitted or `-errno`. It also
    /// enters the kernel, with `IORING_ENTER_GETEVENTS`, when the completion queue
    /// it also enters the kernel, with `IORING_ENTER_GETEVENTS`, when the completion queue
    /// needs flushing, so overflowed completions reach the ring.
    pub fn io_uring_submit(ring: &mut IoUring) -> i32 {
        let submitted = io_uring_flush_sq(ring);
        let cq_needs_enter = cq_ring_needs_enter(ring);
        let mut flags = 0;
        if sq_ring_needs_enter(ring, submitted, &mut flags) || cq_needs_enter {
            if cq_needs_enter {
                flags |= IORING_ENTER_GETEVENTS;
            }
            // SAFETY: the ring's descriptor, and a null signal set or the caller's.
            return unsafe { io_uring_enter(ring.ring_fd, submitted, 0, flags, std::ptr::null()) };
        }
        submitted as i32
    }

    /// Collects up to `count` ready completions into `cqes` (filling the first returned-count
    /// slots); `count` is at most
    /// `cqes.len()`.
    pub fn io_uring_peek_batch_cqe(
        ring: &mut IoUring,
        cqes: &mut [MaybeUninit<*mut IoUringCqe>],
        count: u32,
    ) -> u32 {
        debug_assert!(count as usize <= cqes.len());
        let mut overflow_checked = false;
        loop {
            // SAFETY: `ktail`/`khead` point into the mapped CQ ring.
            let ready = unsafe { load_acquire(ring.cq.ktail).wrapping_sub(*ring.cq.khead) };
            if ready != 0 {
                // SAFETY: as above.
                let head = unsafe { *ring.cq.khead };
                let count = count.min(ready).min(cqes.len() as u32);
                for i in 0..count {
                    // SAFETY: the index is masked into the CQE array, and `i < cqes.len()`.
                    unsafe {
                        cqes.get_unchecked_mut(i as usize).write(
                            ring.cq.cqes.add((head.wrapping_add(i) & ring.cq.ring_mask) as usize),
                        )
                    };
                }
                return count;
            }
            if overflow_checked {
                return 0;
            }
            if cq_ring_needs_flush(ring) {
                // SAFETY: the ring's descriptor, and a null signal set or the caller's.
                unsafe {
                    io_uring_enter(ring.ring_fd, 0, 0, IORING_ENTER_GETEVENTS, std::ptr::null())
                };
                overflow_checked = true;
                continue;
            }
            return 0;
        }
    }

    /// Marks `nr` completions as consumed.
    pub fn io_uring_cq_advance(ring: &mut IoUring, nr: u32) {
        if nr != 0 {
            // SAFETY: `khead` points into the mapped CQ ring.
            unsafe { store_release(ring.cq.khead, (*ring.cq.khead).wrapping_add(nr)) };
        }
    }

    fn io_uring_peek_cqe(ring: &mut IoUring, nr_available: &mut u32) -> *mut IoUringCqe {
        // SAFETY: `ktail`/`khead` point into the mapped CQ ring.
        let tail = unsafe { load_acquire(ring.cq.ktail) };
        // SAFETY: `khead` points into the CQ ring, mapped while `ring` lives; only this
        // thread writes it.
        let head = unsafe { *ring.cq.khead };
        let available = tail.wrapping_sub(head);
        *nr_available = available;
        if available == 0 {
            return std::ptr::null_mut();
        }
        // SAFETY: the index is masked into the CQE array.
        unsafe { ring.cq.cqes.add((head & ring.cq.ring_mask) as usize) }
    }

    ///
    /// # Safety
    /// `sigmask` is null or points to a signal set.
    pub unsafe fn io_uring_wait_cqes(
        ring: &mut IoUring,
        cqe_ptr: &mut *mut IoUringCqe,
        wait_nr: u32,
        sigmask: *const SigsetT,
    ) -> i32 {
        let mut submit: u32 = 0;
        let mut err = 0;
        let mut cqe: *mut IoUringCqe;
        loop {
            let mut need_enter = false;
            let mut flags = 0;
            let mut nr_available = 0;
            cqe = io_uring_peek_cqe(ring, &mut nr_available);
            if cqe.is_null() && wait_nr == 0 && submit == 0 {
                err = -EAGAIN;
                break;
            }
            if wait_nr > nr_available || cq_ring_needs_flush(ring) {
                flags = IORING_ENTER_GETEVENTS;
                need_enter = true;
            }
            if sq_ring_needs_enter(ring, submit, &mut flags) {
                need_enter = true;
            }
            if !need_enter {
                break;
            }
            // SAFETY: the ring's descriptor, and a null signal set or the caller's.
            let ret = unsafe { io_uring_enter(ring.ring_fd, submit, wait_nr, flags, sigmask) };
            if ret < 0 {
                err = ret;
                break;
            }
            submit -= (ret as u32).min(submit);
            if !cqe.is_null() {
                break;
            }
        }
        *cqe_ptr = cqe;
        err
    }

    #[inline]
    ///
    /// # Safety
    /// `sqe` is an entry of a ring, from `io_uring_get_sqe`, not yet submitted; the buffers, iovecs, messages and addresses passed stay valid for the kernel until the request completes.
    pub unsafe fn io_uring_prep_rw(
        op: u8,
        sqe: *mut IoUringSqe,
        fd: i32,
        addr: *const c_void,
        len: u32,
        offset: u64,
    ) {
        // SAFETY: `sqe` was returned by io_uring_get_sqe.
        unsafe {
            *sqe = IoUringSqe {
                opcode: op,
                fd,
                off: offset,
                addr: addr as u64,
                len,
                ..IoUringSqe::default()
            };
        }
    }

    ///
    /// # Safety
    /// `sqe` is an entry of a ring, from `io_uring_get_sqe`, not yet submitted; the buffers, iovecs, messages and addresses passed stay valid for the kernel until the request completes.
    pub unsafe fn io_uring_prep_poll_add(sqe: *mut IoUringSqe, fd: i32, poll_mask: u32) {
        // SAFETY: forwarded from the caller.
        unsafe { io_uring_prep_rw(IORING_OP_POLL_ADD, sqe, fd, std::ptr::null(), 0, 0) };
        // SAFETY: as above.
        unsafe { (*sqe).rw_flags = poll_mask.to_le() };
    }

    ///
    /// # Safety
    /// `sqe` is an entry of a ring, from `io_uring_get_sqe`, not yet submitted; the buffers, iovecs, messages and addresses passed stay valid for the kernel until the request completes.
    pub unsafe fn io_uring_prep_read(
        sqe: *mut IoUringSqe,
        fd: i32,
        buf: *mut c_void,
        nbytes: u32,
        offset: u64,
    ) {
        // SAFETY: forwarded from the caller.
        unsafe { io_uring_prep_rw(IORING_OP_READ, sqe, fd, buf, nbytes, offset) };
    }

    ///
    /// # Safety
    /// `sqe` is an entry of a ring, from `io_uring_get_sqe`, not yet submitted; the buffers, iovecs, messages and addresses passed stay valid for the kernel until the request completes.
    pub unsafe fn io_uring_prep_write(
        sqe: *mut IoUringSqe,
        fd: i32,
        buf: *const c_void,
        nbytes: u32,
        offset: u64,
    ) {
        // SAFETY: forwarded from the caller.
        unsafe { io_uring_prep_rw(IORING_OP_WRITE, sqe, fd, buf, nbytes, offset) };
    }

    ///
    /// # Safety
    /// `sqe` is an entry of a ring, from `io_uring_get_sqe`, not yet submitted; the buffers, iovecs, messages and addresses passed stay valid for the kernel until the request completes.
    pub unsafe fn io_uring_prep_readv(
        sqe: *mut IoUringSqe,
        fd: i32,
        iovecs: *const Iovec,
        nr_vecs: u32,
        offset: u64,
    ) {
        // SAFETY: forwarded from the caller.
        unsafe {
            io_uring_prep_rw(IORING_OP_READV, sqe, fd, iovecs as *const c_void, nr_vecs, offset)
        };
    }

    ///
    /// # Safety
    /// `sqe` is an entry of a ring, from `io_uring_get_sqe`, not yet submitted; the buffers, iovecs, messages and addresses passed stay valid for the kernel until the request completes.
    pub unsafe fn io_uring_prep_writev(
        sqe: *mut IoUringSqe,
        fd: i32,
        iovecs: *const Iovec,
        nr_vecs: u32,
        offset: u64,
    ) {
        // SAFETY: forwarded from the caller.
        unsafe {
            io_uring_prep_rw(IORING_OP_WRITEV, sqe, fd, iovecs as *const c_void, nr_vecs, offset)
        };
    }

    ///
    /// # Safety
    /// `sqe` is an entry of a ring, from `io_uring_get_sqe`, not yet submitted; the buffers, iovecs, messages and addresses passed stay valid for the kernel until the request completes.
    pub unsafe fn io_uring_prep_fsync(sqe: *mut IoUringSqe, fd: i32, fsync_flags: u32) {
        // SAFETY: forwarded from the caller.
        unsafe { io_uring_prep_rw(IORING_OP_FSYNC, sqe, fd, std::ptr::null(), 0, 0) };
        // SAFETY: as above.
        unsafe { (*sqe).rw_flags = fsync_flags };
    }

    ///
    /// # Safety
    /// `sqe` is an entry of a ring, from `io_uring_get_sqe`, not yet submitted; the buffers, iovecs, messages and addresses passed stay valid for the kernel until the request completes.
    pub unsafe fn io_uring_prep_recv(
        sqe: *mut IoUringSqe,
        sockfd: i32,
        buf: *mut c_void,
        len: usize,
        flags: i32,
    ) {
        // SAFETY: forwarded from the caller.
        unsafe { io_uring_prep_rw(IORING_OP_RECV, sqe, sockfd, buf, len as u32, 0) };
        // SAFETY: as above.
        unsafe { (*sqe).rw_flags = flags as u32 };
    }

    ///
    /// # Safety
    /// `sqe` is an entry of a ring, from `io_uring_get_sqe`, not yet submitted; the buffers, iovecs, messages and addresses passed stay valid for the kernel until the request completes.
    pub unsafe fn io_uring_prep_recvmsg(
        sqe: *mut IoUringSqe,
        fd: i32,
        msg: *mut Msghdr,
        flags: u32,
    ) {
        // SAFETY: forwarded from the caller.
        unsafe { io_uring_prep_rw(IORING_OP_RECVMSG, sqe, fd, msg as *const c_void, 1, 0) };
        // SAFETY: as above.
        unsafe { (*sqe).rw_flags = flags };
    }

    ///
    /// # Safety
    /// `sqe` is an entry of a ring, from `io_uring_get_sqe`, not yet submitted; the buffers, iovecs, messages and addresses passed stay valid for the kernel until the request completes.
    pub unsafe fn io_uring_prep_send(
        sqe: *mut IoUringSqe,
        sockfd: i32,
        buf: *const c_void,
        len: usize,
        flags: i32,
    ) {
        // SAFETY: forwarded from the caller.
        unsafe { io_uring_prep_rw(IORING_OP_SEND, sqe, sockfd, buf, len as u32, 0) };
        // SAFETY: as above.
        unsafe { (*sqe).rw_flags = flags as u32 };
    }

    ///
    /// # Safety
    /// `sqe` is an entry of a ring, from `io_uring_get_sqe`, not yet submitted; the buffers, iovecs, messages and addresses passed stay valid for the kernel until the request completes.
    pub unsafe fn io_uring_prep_sendmsg(
        sqe: *mut IoUringSqe,
        fd: i32,
        msg: *const Msghdr,
        flags: u32,
    ) {
        // SAFETY: forwarded from the caller.
        unsafe { io_uring_prep_rw(IORING_OP_SENDMSG, sqe, fd, msg as *const c_void, 1, 0) };
        // SAFETY: as above.
        unsafe { (*sqe).rw_flags = flags };
    }

    ///
    /// # Safety
    /// `sqe` is an entry of a ring, from `io_uring_get_sqe`, not yet submitted; the buffers, iovecs, messages and addresses passed stay valid for the kernel until the request completes.
    pub unsafe fn io_uring_prep_accept(
        sqe: *mut IoUringSqe,
        fd: i32,
        addr: *mut c_void,
        addrlen: *mut u32,
        flags: i32,
    ) {
        // SAFETY: forwarded from the caller.
        unsafe { io_uring_prep_rw(IORING_OP_ACCEPT, sqe, fd, addr, 0, addrlen as u64) };
        // SAFETY: as above.
        unsafe { (*sqe).rw_flags = flags as u32 };
    }

    ///
    /// # Safety
    /// `sqe` is an entry of a ring, from `io_uring_get_sqe`, not yet submitted; the buffers, iovecs, messages and addresses passed stay valid for the kernel until the request completes.
    pub unsafe fn io_uring_prep_connect(
        sqe: *mut IoUringSqe,
        fd: i32,
        addr: *const c_void,
        addrlen: u32,
    ) {
        // SAFETY: forwarded from the caller.
        unsafe { io_uring_prep_rw(IORING_OP_CONNECT, sqe, fd, addr, 0, addrlen as u64) };
    }

    /// Stores the completion `data` as the user data of `sqe`.
    #[inline]
    ///
    /// # Safety
    /// `sqe` is an entry of a ring, from `io_uring_get_sqe`, not yet submitted; `data` stays live until its completion is reaped.
    pub unsafe fn io_uring_sqe_set_data(sqe: *mut IoUringSqe, data: *mut KernelCompletion) {
        // SAFETY: as above.
        unsafe { (*sqe).user_data = data as u64 };
    }

    /// The kernel completion whose address is the user data of `cqe`.
    #[inline]
    pub fn cqe_get_data(cqe: &IoUringCqe) -> *mut KernelCompletion {
        cqe.user_data as *mut KernelCompletion
    }
}

use uring::*;

fn try_create_uring(queue_len: u32, throw_on_error: bool) -> Result<Option<IoUring>, ExceptionPtr> {
    let required_features = IORING_FEAT_SUBMIT_STABLE | IORING_FEAT_NODROP;
    let required_ops = [
        IORING_OP_POLL_ADD, // linux 5.1
        IORING_OP_READV,
        IORING_OP_WRITEV,
        IORING_OP_FSYNC,
        IORING_OP_SENDMSG, // linux 5.3
        IORING_OP_RECVMSG,
        IORING_OP_ACCEPT,
        IORING_OP_CONNECT,
        IORING_OP_READ, // linux 5.6
        IORING_OP_WRITE,
        IORING_OP_SEND,
        IORING_OP_RECV,
    ];
    let maybe_throw = |exception: ExceptionPtr| -> Result<Option<IoUring>, ExceptionPtr> {
        if throw_on_error {
            return Err(exception);
        }
        Ok(None)
    };

    let mut params = IoUringParams::default();
    let mut ring = match io_uring_queue_init_params(queue_len, &mut params) {
        Ok(r) => r,
        Err(err) => {
            return maybe_throw(ExceptionPtr::from(SystemError::new(
                -err,
                "trying to create io_uring",
            )));
        }
    };
    // The ring is closed on the error paths below and kept on success.
    io_uring_ring_dontfork(&mut ring);
    let result = (|| {
        if !ring.features & required_features != 0 {
            return Some(ExceptionPtr::new(RuntimeError::new(format!(
                "missing required io_ring features, required 0x{:x} available 0x{:x}",
                required_features, ring.features
            ))));
        }

        let probe = match io_uring_get_probe_ring(&mut ring) {
            Some(p) => p,
            None => {
                return Some(ExceptionPtr::new(RuntimeError::new(
                    "unable to create io_uring probe".to_string(),
                )));
            }
        };

        for op in required_ops {
            if !io_uring_opcode_supported(&probe, op) {
                return Some(ExceptionPtr::new(RuntimeError::new(format!(
                    "required io_uring opcode {} not supported",
                    op as i32
                ))));
            }
        }
        io_uring_free_probe(probe);
        None
    })();
    if let Some(e) = result {
        io_uring_queue_exit(&mut ring);
        return maybe_throw(e);
    }

    Ok(Some(ring))
}

fn have_md_devices() -> bool {
    if let Ok(dir) = std::fs::read_dir("/sys/block") {
        for entry in dir.flatten() {
            if entry.path().is_dir() && entry.path().join("md").exists() {
                return true;
            }
        }
    }
    false
}

fn mlock_limit() -> usize {
    let mut lim = Rlimit::default();
    // SAFETY: `lim` is a valid rlimit.
    let r = unsafe { getrlimit(RLIMIT_MEMLOCK, &mut lim) };
    if r == -1 {
        return 0; // assume the worst; this is advisory anyway
    }
    lim.rlim_cur as usize
}

fn detect_io_uring() -> bool {
    if !kernel_uname().whitelisted(&["5.17"]) && have_md_devices() {
        // Older kernels fall back to workqueues for RAID devices
        return false;
    }
    if !kernel_uname().whitelisted(&["5.12"]) && mlock_limit() < (8 << 20) {
        // Older kernels lock about 32k/vcpu for the ring itself. Require 8MB of
        // locked memory to be safe (8MB is what newer kernels and newer systemd provide)
        return false;
    }
    let ring_opt = try_create_uring(1, false).ok().flatten();
    match ring_opt {
        Some(mut ring) => {
            io_uring_queue_exit(&mut ring);
            true
        }
        None => false,
    }
}

/// A backend using one io_uring ring for disk and network I/O, with a linux-aio preempting
/// context for the task-quota timer.
pub struct ReactorBackendUring {
    _r: *mut Reactor,
    _uring: IoUring,
    _did_work_while_getting_sqe: bool,
    _has_pending_submissions: bool,
    _hrtimer_timerfd: FileDesc,
    _preempt_io_context: PreemptIoContext,
    _hrtimer_completion: HrtimerCompletion,
    _smp_wakeup_completion: SmpWakeupCompletion,
}

#[repr(C)]
/// The descriptor state of the io_uring backend: the base state and one completion each for
/// `POLLIN`, `POLLOUT` and `POLLRDHUP` waiters.
pub struct UringPollableFdState {
    /// The base state.
    pub base: PollableFdState,
    _completion_pollin: PollableFdStateCompletion,
    _completion_pollout: PollableFdStateCompletion,
    _completion_pollrdhup: PollableFdStateCompletion,
}

impl UringPollableFdState {
    /// A state for `desc` with speculation `speculate`.
    pub fn new(desc: FileDesc, speculate: Speculation) -> Box<Self> {
        Box::new(UringPollableFdState {
            base: PollableFdState::new(desc, speculate),
            _completion_pollin: PollableFdStateCompletion::default(),
            _completion_pollout: PollableFdStateCompletion::default(),
            _completion_pollrdhup: PollableFdStateCompletion::default(),
        })
    }
    /// # Safety
    /// `this` is the root pointer of a live state of this type.
    pub unsafe fn get_desc(this: *mut Self, events: i32) -> *mut PollableFdStateCompletion {
        // SAFETY: see AioPollableFdState; no reference is formed.
        unsafe {
            if events & POLLIN != 0 {
                std::ptr::addr_of_mut!((*this)._completion_pollin)
            } else if events & POLLOUT != 0 {
                std::ptr::addr_of_mut!((*this)._completion_pollout)
            } else {
                std::ptr::addr_of_mut!((*this)._completion_pollrdhup)
            }
        }
    }
    /// # Safety
    /// As [`UringPollableFdState::get_desc`].
    pub unsafe fn get_completion_future(this: *mut Self, events: i32) -> Future<()> {
        // SAFETY: see AioPollableFdState; the completion is reborrowed for this call only.
        unsafe { (*Self::get_desc(this, events)).get_future() }
    }
}

// eventfd and timerfd both need an 8-byte read after completion
#[repr(C)]
/// A poll completion on an eventfd or timerfd that is re-armed after each expiry (and reads
/// the descriptor's 8-byte counter).
pub struct RecurringEventfdOrTimerfdCompletion {
    fd_kc: FdKernelCompletion,
    _armed: bool,
}

impl RecurringEventfdOrTimerfdCompletion {
    /// A completion reading `fd`, as the base of the containing type `T`.
    ///
    /// # Safety
    /// As `FdKernelCompletion::new`.
    pub unsafe fn new<T: KernelCompletionImpl>(fd: *mut FileDesc) -> Self {
        RecurringEventfdOrTimerfdCompletion {
            // SAFETY: guaranteed by the caller.
            fd_kc: unsafe { FdKernelCompletion::new::<T>(fd) },
            _armed: false,
        }
    }
    fn base_complete_with(&mut self, _res: isize) {
        let mut garbage = [0u8; 8];
        // SAFETY: the buffer is valid for writes of the length passed.
        let ret = match unsafe { self.fd_kc.fd().read(garbage.as_mut_ptr() as *mut c_void, 8) } {
            Ok(ret) => ret,
            Err(e) => exception_in_reactor_main_loop(e),
        };
        // Note: for hrtimer_completion we can have spurious wakeups,
        // since we wait for this using both _preempt_io_context and the
        // ring. So don't assert that we read anything.
        engine_assert!(ret.is_none_or(|r| r == 8));
        self._armed = false;
    }
    /// Queues the poll again if it is not armed; the completion is a field of `be`, so
    /// both are taken as pointers and never reborrowed at the same time.
    ///
    /// # Safety
    /// `this` is a completion of the backend `be` (or its first field, whose header
    /// dispatches to the derived completion), both derived from the backend's root pointer;
    /// no reference to either is live.
    unsafe fn maybe_rearm(this: *mut Self, be: *mut ReactorBackendUring) {
        // SAFETY: the contract above; each reborrow lasts one call or one field access.
        unsafe {
            if (*this)._armed {
                return;
            }
            let sqe = ReactorBackendUring::get_sqe(be);
            io_uring_prep_poll_add(sqe, (*this).fd_kc.fd().get(), POLLIN as u32);
            io_uring_sqe_set_data(sqe, this.cast::<KernelCompletion>());
            (*this)._armed = true;
            (*be)._has_pending_submissions = true;
        }
    }
}

/// The completion of the inter-shard wake-up eventfd.
pub type SmpWakeupCompletion = RecurringEventfdOrTimerfdCompletion;

// SAFETY: `#[repr(C)]` with `fd_kc` at offset 0; built with `new::<Self>()` when used directly.
unsafe impl KernelCompletionImpl for RecurringEventfdOrTimerfdCompletion {
    unsafe fn complete_with(this: *mut Self, res: isize) {
        // SAFETY: the trait's contract: `this` is live and no reference to it is live;
        // `me` is not used after the object is destroyed.
        let me = unsafe { &mut *this };
        me.base_complete_with(res);
    }
}

// Completion for high resolution timerfd, used in wait_and_process_events()
// (while running tasks it's waited for in _preempt_io_context)
#[repr(C)]
/// The completion of the high resolution timer's timerfd while the reactor sleeps.
pub struct HrtimerCompletion {
    base: RecurringEventfdOrTimerfdCompletion,
    _r: *mut Reactor,
}

impl HrtimerCompletion {
    /// A completion for the timerfd `timerfd` of reactor `r`.
    ///
    /// # Safety
    /// `r` is the reactor that owns the backend; `timerfd` as in `FdKernelCompletion::new`.
    pub unsafe fn new(r: *mut Reactor, timerfd: *mut FileDesc) -> Self {
        HrtimerCompletion {
            // SAFETY: guaranteed by the caller.
            base: unsafe { RecurringEventfdOrTimerfdCompletion::new::<Self>(timerfd) },
            _r: r,
        }
    }
}

// SAFETY: `#[repr(C)]` with `base` (header built for `Self`) at offset 0.
unsafe impl KernelCompletionImpl for HrtimerCompletion {
    unsafe fn complete_with(this: *mut Self, res: isize) {
        // SAFETY: the trait's contract: `this` is live and no reference to it is live;
        // `me` is not used after the object is destroyed.
        let me = unsafe { &mut *this };
        me.base.base_complete_with(res);
        // SAFETY: `_r` is the reactor that owns the backend; the completion runs from the
        // backend's reap, with no reference to the reactor live.
        unsafe { Reactor::service_highres_timer(me._r) };
    }
}

const IOV_MAX: usize = 1024;

impl ReactorBackendUring {
    // s_queue_len is more or less arbitrary. Too low and we'll be
    // issuing too small batches, too high and we require too much locked
    // memory, but otherwise it doesn't matter.
    const S_QUEUE_LEN: u32 = 200;

    fn make_timerfd() -> Result<FileDesc, ExceptionPtr> {
        FileDesc::timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC | TFD_NONBLOCK)
    }

    // Can fail if the completion queue is full
    fn try_get_sqe(&mut self) -> *mut IoUringSqe {
        io_uring_get_sqe(&mut self._uring)
    }

    /// The free entries of the submission ring: that many `get_sqe` calls return at once,
    /// without running completions.
    fn sq_space_left(&self) -> u32 {
        io_uring_sq_space_left(&self._uring)
    }

    fn do_flush_submission_ring(&mut self) -> bool {
        if self._has_pending_submissions {
            self._has_pending_submissions = false;
            self._did_work_while_getting_sqe = false;
            io_uring_submit(&mut self._uring);
            true
        } else {
            std::mem::replace(&mut self._did_work_while_getting_sqe, false)
        }
    }

    /// A free submission entry: when the ring is full it flushes it and runs the ready
    /// completions, among them the high resolution timer's, whose callbacks may come back
    /// into the backend (to queue a poll, which calls this again) and into the reactor, the
    /// descriptor states and the I/O sink. So the backend is taken by its root pointer and
    /// reborrowed only for the ring operations, never across the completions; the callers
    /// hold no reference to the backend, to a descriptor state or to the sink across it.
    ///
    /// # Safety
    /// `this` is the backend's root pointer, with no reference to the backend live.
    unsafe fn get_sqe(this: *mut Self) -> *mut IoUringSqe {
        let mut sqe;
        loop {
            // SAFETY: the contract above; each reborrow lasts one call or field access.
            unsafe {
                sqe = (*this).try_get_sqe();
                if !sqe.is_null() {
                    break;
                }
                (*this).do_flush_submission_ring();
                Self::do_process_kernel_completions_step(this);
                (*this)._did_work_while_getting_sqe = true;
            }
        }
        sqe
    }

    /// # Safety
    /// `this` is the backend's root pointer and `fd` the root pointer of a state it made,
    /// with no reference to either live.
    unsafe fn poll(this: *mut Self, fd: *mut PollableFdState, events: i32) -> Future<()> {
        // SAFETY: the contract above.
        let sqe = unsafe { Self::get_sqe(this) };
        // SAFETY: `sqe` comes from get_sqe and is not yet submitted. Every descriptor state
        // made by this backend is an `UringPollableFdState` and `fd` is its root pointer; the
        // kernel gets the completion through it. Short field accesses through the roots.
        unsafe {
            io_uring_prep_poll_add(sqe, (*fd).fd.get(), events as u32);
            let ufd = fd.cast::<UringPollableFdState>();
            io_uring_sqe_set_data(
                sqe,
                UringPollableFdState::get_desc(ufd, events).cast::<KernelCompletion>(),
            );
            (*this)._has_pending_submissions = true;
            UringPollableFdState::get_completion_future(ufd, events)
        }
    }

    /// # Safety
    /// `this` is the backend's root pointer, with no reference to the backend live; the
    /// ring has a free entry (see `queue_pending_file_io`), so `get_sqe` runs no completion
    /// while `req` (in the sink) is borrowed.
    unsafe fn submit_io_request(this: *mut Self, req: &IoRequest, completion: *mut IoCompletion) {
        // SAFETY: the contract above.
        let sqe = unsafe { Self::get_sqe(this) };
        match req.opcode() {
            Operation::Read => {
                let op = req.as_read();
                // SAFETY: `sqe` comes from get_sqe and is not yet submitted; the buffers of the request are kept by its completion until it completes.
                unsafe { io_uring_prep_read(sqe, op.fd, op.addr.cast(), op.size as u32, op.pos) };
            }
            Operation::Write => {
                let op = req.as_write();
                // SAFETY: `sqe` comes from get_sqe and is not yet submitted; the buffers of the request are kept by its completion until it completes.
                unsafe { io_uring_prep_write(sqe, op.fd, op.addr.cast(), op.size as u32, op.pos) };
            }
            Operation::Readv => {
                let op = req.as_readv();
                // SAFETY: `sqe` comes from get_sqe and is not yet submitted; the buffers of the request are kept by its completion until it completes.
                unsafe { io_uring_prep_readv(sqe, op.fd, op.iovec, op.iov_len as u32, op.pos) };
            }
            Operation::Writev => {
                let op = req.as_writev();
                // SAFETY: `sqe` comes from get_sqe and is not yet submitted; the buffers of the request are kept by its completion until it completes.
                unsafe { io_uring_prep_writev(sqe, op.fd, op.iovec, op.iov_len as u32, op.pos) };
            }
            Operation::Fdatasync => {
                let op = req.as_fdatasync();
                // SAFETY: `sqe` comes from get_sqe and is not yet submitted; the buffers of the request are kept by its completion until it completes.
                unsafe { io_uring_prep_fsync(sqe, op.fd, IORING_FSYNC_DATASYNC) };
            }
            Operation::Recv => {
                let op = req.as_recv();
                // SAFETY: `sqe` comes from get_sqe and is not yet submitted; the buffers of the request are kept by its completion until it completes.
                unsafe { io_uring_prep_recv(sqe, op.fd, op.addr.cast(), op.size, op.flags) };
            }
            Operation::Recvmsg => {
                let op = req.as_recvmsg();
                // SAFETY: `sqe` comes from get_sqe and is not yet submitted; the buffers of the request are kept by its completion until it completes.
                unsafe { io_uring_prep_recvmsg(sqe, op.fd, op.msghdr, op.flags as u32) };
            }
            Operation::Send => {
                let op = req.as_send();
                // SAFETY: `sqe` comes from get_sqe and is not yet submitted; the buffers of the request are kept by its completion until it completes.
                unsafe { io_uring_prep_send(sqe, op.fd, op.addr.cast(), op.size, op.flags) };
            }
            Operation::Sendmsg => {
                let op = req.as_sendmsg();
                // SAFETY: `sqe` comes from get_sqe and is not yet submitted; the buffers of the request are kept by its completion until it completes.
                unsafe { io_uring_prep_sendmsg(sqe, op.fd, op.msghdr, op.flags as u32) };
            }
            Operation::Accept => {
                let op = req.as_accept();
                // SAFETY: `sqe` comes from get_sqe and is not yet submitted; the buffers of the request are kept by its completion until it completes.
                unsafe { io_uring_prep_accept(sqe, op.fd, op.sockaddr, op.socklen_ptr, op.flags) };
            }
            Operation::Connect => {
                let op = req.as_connect();
                // SAFETY: `sqe` comes from get_sqe and is not yet submitted; the buffers of the request are kept by its completion until it completes.
                unsafe { io_uring_prep_connect(sqe, op.fd, op.sockaddr, op.socklen) };
            }
            Operation::PollAdd | Operation::PollRemove | Operation::Cancel => {
                // The reactor does not generate these types of I/O requests yet, so
                // this path is unreachable. As more features of io_uring are exploited,
                // we'll utilize more of these opcodes.
                engine_logger().error(
                    FormatInfo::new("Invalid operation for iocb: {}"),
                    format_args!("Invalid operation for iocb: {}", req.opname()),
                );
                std::process::abort();
            }
        }
        // SAFETY: `sqe` comes from get_sqe and is not yet submitted; the completion deletes itself when it completes, after the kernel is done with it.
        unsafe { io_uring_sqe_set_data(sqe, IoCompletion::as_kernel_completion(completion)) };

        // SAFETY: the contract above; one field write.
        unsafe { (*this)._has_pending_submissions = true };
    }

    // Returns true if any work was done
    //
    // The sink is drained up to the free entries of the ring, so no completion runs while
    // the drain borrows the sink (a full ring would make `get_sqe` run completions in the
    // middle of the drain); when the ring is full and requests remain, the ring is flushed
    // and the completions run between two drains,
    //
    // # Safety
    // `this` is the backend's root pointer, with no reference to the backend live.
    unsafe fn queue_pending_file_io(this: *mut Self) -> bool {
        // SAFETY: `_r` is the reactor that owns this backend, live while the
        // backend is; the sink is reached through it for each drain only.
        let sink = unsafe { std::ptr::addr_of_mut!((*(*this)._r)._io_sink) };
        let mut drained = 0usize;
        loop {
            // SAFETY: the contract above; short reborrows.
            let space = unsafe { (*this).sq_space_left() };
            if space == 0 {
                // SAFETY: as above.
                if unsafe { (*sink)._pending_io.empty() } {
                    break;
                }
                // SAFETY: as above; no reference to the sink or the backend is live.
                unsafe {
                    (*this).do_flush_submission_ring();
                    Self::do_process_kernel_completions_step(this);
                    (*this)._did_work_while_getting_sqe = true;
                }
                continue;
            }
            let mut left = space;
            // SAFETY: as above; while the drain borrows the sink, `submit_io_request` finds a
            // free entry and runs no completion, so nothing else reaches the sink.
            let sink_ref = unsafe { &mut *sink };
            let n = sink_ref.drain(|req: &IoRequest, completion: *mut IoCompletion| -> bool {
                if left == 0 {
                    return false;
                }
                left -= 1;
                // SAFETY: the ring has a free entry for this request.
                unsafe { Self::submit_io_request(this, req, completion) };
                true
            });
            drained += n;
            if left != 0 {
                // the sink is empty
                break;
            }
        }
        drained != 0
    }

    // Process kernel completions already extracted from the ring.
    // This is needed because we sometimes extract completions without
    // waiting, and sometimes with waiting.
    fn do_process_ready_kernel_completions(buf: &[*mut IoUringCqe]) {
        for &cqe in buf {
            // SAFETY: each pointer was handed out by io_uring_peek_batch_cqe and is still in the ring.
            let cqe = unsafe { &*cqe };
            let completion = cqe_get_data(cqe);
            // SAFETY: the completion lives until it is completed.
            unsafe { KernelCompletion::complete_with(completion, cqe.res as isize) };
        }
    }

    // Returns true if completions were processed
    //
    // The completions (the high resolution timer's runs the expired timers) may come back
    // into the backend, so it is taken as a pointer and the ring is reborrowed only around
    // the completions.
    //
    // # Safety
    // `this` is the backend's root pointer (or derived from it), with no reference to the
    // backend live.
    unsafe fn do_process_kernel_completions_step(this: *mut Self) -> bool {
        // Left uninitialized: the kernel's entries are copied into the first `n` slots.
        let mut buf = [MaybeUninit::<*mut IoUringCqe>::uninit(); Self::S_QUEUE_LEN as usize];
        // SAFETY: the contract above; the ring is reborrowed for this call only.
        let n =
            io_uring_peek_batch_cqe(unsafe { &mut (*this)._uring }, &mut buf, Self::S_QUEUE_LEN);
        // SAFETY: the first `n` slots were written by io_uring_peek_batch_cqe.
        let ready = unsafe {
            std::slice::from_raw_parts(buf.as_ptr().cast::<*mut IoUringCqe>(), n as usize)
        };
        Self::do_process_ready_kernel_completions(ready);
        // SAFETY: as above.
        io_uring_cq_advance(unsafe { &mut (*this)._uring }, n);
        n != 0
    }

    // Returns true if completions were processed
    //
    // # Safety
    // As `do_process_kernel_completions_step`.
    unsafe fn do_process_kernel_completions(this: *mut Self) -> bool {
        let mut did_work = false;
        // SAFETY: forwarded.
        while unsafe { Self::do_process_kernel_completions_step(this) } {
            did_work = true;
        }
        // SAFETY: as above; one field access.
        did_work | std::mem::replace(unsafe { &mut (*this)._did_work_while_getting_sqe }, false)
    }

    /// # Safety
    /// `this` is the backend's root pointer.
    #[inline]
    unsafe fn submit_request<C: IoCompletionImpl + HasFuture>(
        this: *mut Self,
        mut desc: Box<C>,
        req: IoRequest,
    ) -> Future<C::Output> {
        let fut = desc.get_future();
        // `desc.release()`: the completion deletes itself when it completes.
        // SAFETY: `_r` is the reactor that owns this backend, live while the
        // backend is; the sink is reached for this call only, which runs no completion.
        unsafe {
            (*std::ptr::addr_of_mut!((*(*this)._r)._io_sink))
                .submit(Box::into_raw(desc).cast::<IoCompletion>(), req)
        };
        fut
    }

    /// The io_uring backend for reactor `r`; the completions are wired to the timer
    /// descriptor once the backend is at its heap address.
    ///
    /// # Safety
    /// `r` is the reactor that will own the backend, constructed in place, which outlives it.
    pub unsafe fn new(r: *mut Reactor) -> Result<NodePtr<Self>, ExceptionPtr> {
        let rp = r;
        // SAFETY: guaranteed by the caller; the descriptors are fields of the reactor, which
        // does not move.
        let (task_quota, notify, tfd) = unsafe {
            (
                std::ptr::addr_of_mut!((*r)._task_quota_timer),
                std::ptr::addr_of_mut!((*r)._notify_eventfd),
                (*r)._task_quota_timer.get(),
            )
        };
        let Some(uring) = try_create_uring(Self::S_QUEUE_LEN, true)? else {
            // With throw_on_error == true, try_create_uring() returns a ring or an error.
            return Err(ExceptionPtr::new(RuntimeError::new(
                "trying to create io_uring".to_string(),
            )));
        };
        // `_hrtimer_timerfd` is constructed before the completions that refer to it (see
        // ReactorBackendAio::new).
        let hrtimer_timerfd = Self::make_timerfd()?;
        let raw = hrtimer_timerfd.get();
        // SAFETY: as in ReactorBackendAio::new.
        let node = unsafe {
            NodePtr::new(ReactorBackendUring {
                _r: rp,
                _uring: uring,
                _did_work_while_getting_sqe: false,
                _has_pending_submissions: false,
                _hrtimer_timerfd: hrtimer_timerfd,
                _preempt_io_context: PreemptIoContext::new(
                    rp,
                    task_quota,
                    std::ptr::null_mut(),
                    raw,
                )?,
                _hrtimer_completion: HrtimerCompletion::new(rp, std::ptr::null_mut()),
                _smp_wakeup_completion: SmpWakeupCompletion::new::<SmpWakeupCompletion>(notify),
            })
        };
        // The backend's root pointer: the members' pointers to each other derive from it.
        let this = node.as_ptr();
        // SAFETY: `hrtimer` is the backend's own descriptor at its final address; no
        // reference to the backend is live.
        unsafe {
            let hrtimer = std::ptr::addr_of_mut!((*this)._hrtimer_timerfd);
            (*this)._preempt_io_context.rebind_hrtimer(hrtimer);
            (*this)._hrtimer_completion.base.fd_kc.rebind(hrtimer);
        }
        // Protect against spurious wakeups - if we get notified that the timer has
        // expired when it really hasn't, we don't want to block in read(tfd, ...).
        // SAFETY: plain fcntl calls on a descriptor we own.
        unsafe { fcntl(tfd, F_SETFL, fcntl(tfd, F_GETFL) | O_NONBLOCK) };
        Ok(node)
    }
}

impl Drop for ReactorBackendUring {
    fn drop(&mut self) {
        io_uring_queue_exit(&mut self._uring);
    }
}

/// The completions that `submit_request` takes: each resolves a future.
pub trait HasFuture {
    /// The value the future resolves with.
    type Output;
    /// The future of the completion. Call once.
    fn get_future(&mut self) -> Future<Self::Output>;
}

#[repr(C)]
struct AcceptCompletion {
    base: IoCompletion,
    _listenfd: *mut PollableFdState,
    _sa: SocketAddress,
    _result: Promise<(PollableFd, SocketAddress)>,
}

impl AcceptCompletion {
    fn posix_sockaddr(&mut self) -> *mut c_void {
        self._sa.as_posix_sockaddr_mut() as *mut _ as *mut c_void
    }
    fn socklen_ptr(&mut self) -> *mut u32 {
        &mut self._sa.addr_length
    }
}

// SAFETY: `#[repr(C)]` with the `IoCompletion` built for `Self` at offset 0.
unsafe impl IoCompletionImpl for AcceptCompletion {
    unsafe fn complete(this: *mut Self, fd: usize) {
        // SAFETY: the trait's contract: `this` is live and no reference to it is live;
        // `me` is not used after the object is destroyed.
        let me = unsafe { &mut *this };
        // SAFETY: the listening fd state outlives the accept.
        let listenfd = unsafe { &mut *me._listenfd };
        listenfd.speculate_epoll(EPOLLIN as i32);
        let pfd = PollableFd::new(FileDesc::from_fd(fd as i32), Speculation::new(EPOLLOUT as i32));
        let sa = std::mem::take(&mut me._sa);
        me._result.set_value((pfd, sa));
        // SAFETY: `delete this`: the completion was leaked by submit_request.
        drop(unsafe { Box::from_raw(this) });
    }
    unsafe fn set_exception(this: *mut Self, mut eptr: ExceptionPtr) {
        // SAFETY: the trait's contract: `this` is live and no reference to it is live;
        // `me` is not used after the object is destroyed.
        let me = unsafe { &mut *this };
        if let Some(e) = eptr.downcast_ref::<SystemError>()
            && e.code() == EINVAL
        {
            // The chances are that we shutting down the connection.
            // SAFETY: as above.
            if let Err(ex) = unsafe { (*me._listenfd).maybe_no_more_recv() } {
                eptr = ex;
            }
        }
        me._result.set_exception(eptr);
        // SAFETY: as above.
        drop(unsafe { Box::from_raw(this) });
    }
}
impl HasFuture for AcceptCompletion {
    type Output = (PollableFd, SocketAddress);
    fn get_future(&mut self) -> Future<Self::Output> {
        self._result.get_future()
    }
}

#[repr(C)]
struct ConnectCompletion {
    base: IoCompletion,
    _fd: *mut PollableFdState,
    _sa: SocketAddress,
    _result: Promise<()>,
}

impl ConnectCompletion {
    fn posix_sockaddr(&mut self) -> *mut c_void {
        self._sa.as_posix_sockaddr_mut() as *mut _ as *mut c_void
    }
    fn socklen(&self) -> u32 {
        self._sa.addr_length
    }
}

// SAFETY: `#[repr(C)]` with the `IoCompletion` built for `Self` at offset 0.
unsafe impl IoCompletionImpl for ConnectCompletion {
    unsafe fn complete(this: *mut Self, _fd: usize) {
        // SAFETY: the trait's contract: `this` is live and no reference to it is live;
        // `me` is not used after the object is destroyed.
        let me = unsafe { &mut *this };
        // SAFETY: the fd state outlives the connect.
        unsafe { (*me._fd).speculate_epoll(POLLOUT) };
        me._result.set_value(());
        // SAFETY: `delete this`.
        drop(unsafe { Box::from_raw(this) });
    }
    unsafe fn set_exception(this: *mut Self, eptr: ExceptionPtr) {
        // SAFETY: the trait's contract: `this` is live and no reference to it is live;
        // `me` is not used after the object is destroyed.
        let me = unsafe { &mut *this };
        me._result.set_exception(eptr);
        // SAFETY: `delete this`.
        drop(unsafe { Box::from_raw(this) });
    }
}
impl HasFuture for ConnectCompletion {
    type Output = ();
    fn get_future(&mut self) -> Future<()> {
        self._result.get_future()
    }
}

#[repr(C)]
struct RecvmsgCompletion {
    base: IoCompletion,
    _fd: *mut PollableFdState,
    _iov: Vec<Iovec>,
    _mh: Msghdr,
    _result: Promise<usize>,
}

impl RecvmsgCompletion {
    fn new(fd: *mut PollableFdState, iov: &[Iovec]) -> Box<Self> {
        let mut this = Box::new(RecvmsgCompletion {
            _fd: fd,
            _iov: iov.to_vec(),
            _mh: Msghdr::default(),
            _result: Promise::new(),
            base: IoCompletion::new::<Self>(),
        });
        this._mh.msg_iov = this._iov.as_mut_ptr();
        this._mh.msg_iovlen = this._iov.len();
        this
    }
    fn msghdr(&mut self) -> *mut Msghdr {
        &mut self._mh
    }
}

// SAFETY: `#[repr(C)]` with the `IoCompletion` built for `Self` at offset 0.
unsafe impl IoCompletionImpl for RecvmsgCompletion {
    unsafe fn complete(this: *mut Self, bytes: usize) {
        // SAFETY: the trait's contract: `this` is live and no reference to it is live;
        // `me` is not used after the object is destroyed.
        let me = unsafe { &mut *this };
        if bytes == iovec_len(&me._iov) {
            // SAFETY: the fd state outlives the read.
            unsafe { (*me._fd).speculate_epoll(EPOLLIN as i32) };
        }
        me._result.set_value(bytes);
        // SAFETY: `delete this`.
        drop(unsafe { Box::from_raw(this) });
    }
    unsafe fn set_exception(this: *mut Self, eptr: ExceptionPtr) {
        // SAFETY: the trait's contract: `this` is live and no reference to it is live;
        // `me` is not used after the object is destroyed.
        let me = unsafe { &mut *this };
        me._result.set_exception(eptr);
        // SAFETY: `delete this`.
        drop(unsafe { Box::from_raw(this) });
    }
}
impl HasFuture for RecvmsgCompletion {
    type Output = usize;
    fn get_future(&mut self) -> Future<usize> {
        self._result.get_future()
    }
}

/// `read_completion` of `read_some` and `recv_completion` of `recv_some` (same body).
#[repr(C)]
struct BufferReadCompletion {
    base: IoCompletion,
    _fd: *mut PollableFdState,
    _buffer: TemporaryBuffer<u8>,
    _result: Promise<TemporaryBuffer<u8>>,
}

impl BufferReadCompletion {
    fn new(fd: *mut PollableFdState, buffer: TemporaryBuffer<u8>) -> Box<Self> {
        Box::new(BufferReadCompletion {
            base: IoCompletion::new::<Self>(),
            _fd: fd,
            _buffer: buffer,
            _result: Promise::new(),
        })
    }
    fn get_write(&mut self) -> *mut u8 {
        self._buffer.get_write()
    }
    fn get_size(&self) -> usize {
        self._buffer.size()
    }
}

// SAFETY: `#[repr(C)]` with the `IoCompletion` built for `Self` at offset 0.
unsafe impl IoCompletionImpl for BufferReadCompletion {
    unsafe fn complete(this: *mut Self, bytes: usize) {
        // SAFETY: the trait's contract: `this` is live and no reference to it is live;
        // `me` is not used after the object is destroyed.
        let me = unsafe { &mut *this };
        if bytes == me._buffer.size() {
            // SAFETY: the fd state outlives the read.
            unsafe { (*me._fd).speculate_epoll(EPOLLIN as i32) };
        }
        // SAFETY: the kernel read at most the buffer's size.
        unsafe { me._buffer.trim_unchecked(bytes) };
        let b = std::mem::take(&mut me._buffer);
        me._result.set_value(b);
        // SAFETY: `delete this`.
        drop(unsafe { Box::from_raw(this) });
    }
    unsafe fn set_exception(this: *mut Self, eptr: ExceptionPtr) {
        // SAFETY: the trait's contract: `this` is live and no reference to it is live;
        // `me` is not used after the object is destroyed.
        let me = unsafe { &mut *this };
        me._result.set_exception(eptr);
        // SAFETY: `delete this`.
        drop(unsafe { Box::from_raw(this) });
    }
}
impl HasFuture for BufferReadCompletion {
    type Output = TemporaryBuffer<u8>;
    fn get_future(&mut self) -> Future<TemporaryBuffer<u8>> {
        self._result.get_future()
    }
}

/// `write_completion` of `sendmsg` and of `send`.
#[repr(C)]
struct WriteCompletion {
    base: IoCompletion,
    _fd: *mut PollableFdState,
    _mh: Msghdr,
    _to_write: usize,
    _result: Promise<usize>,
}

impl WriteCompletion {
    fn msghdr(&mut self) -> *mut Msghdr {
        &mut self._mh
    }
}

// SAFETY: `#[repr(C)]` with the `IoCompletion` built for `Self` at offset 0.
unsafe impl IoCompletionImpl for WriteCompletion {
    unsafe fn complete(this: *mut Self, bytes: usize) {
        // SAFETY: the trait's contract: `this` is live and no reference to it is live;
        // `me` is not used after the object is destroyed.
        let me = unsafe { &mut *this };
        if bytes == me._to_write {
            // SAFETY: the fd state outlives the write.
            unsafe { (*me._fd).speculate_epoll(EPOLLOUT as i32) };
        }
        me._result.set_value(bytes);
        // SAFETY: `delete this`.
        drop(unsafe { Box::from_raw(this) });
    }
    unsafe fn set_exception(this: *mut Self, eptr: ExceptionPtr) {
        // SAFETY: the trait's contract: `this` is live and no reference to it is live;
        // `me` is not used after the object is destroyed.
        let me = unsafe { &mut *this };
        me._result.set_exception(eptr);
        // SAFETY: `delete this`.
        drop(unsafe { Box::from_raw(this) });
    }
}
impl HasFuture for WriteCompletion {
    type Output = usize;
    fn get_future(&mut self) -> Future<usize> {
        self._result.get_future()
    }
}

// The entries of `BackendEntries` (see ReactorBackendAio's).
impl ReactorBackendUring {
    unsafe fn reap_kernel_completions(this: *mut Self) -> bool {
        // SAFETY: the table's contract.
        unsafe { Self::do_process_kernel_completions(this) }
    }
    unsafe fn kernel_submit_work(this: *mut Self) -> bool {
        let mut did_work = false;
        // SAFETY: the table's contract; the preempting context is reached through `this`,
        // and the ring and the sink are reborrowed for one call each.
        unsafe {
            did_work |= PreemptIoContext::service_preempting_io(std::ptr::addr_of_mut!(
                (*this)._preempt_io_context
            ));
            did_work |= Self::queue_pending_file_io(this);
            did_work |= io_uring_submit(&mut (*this)._uring) != 0;
        }
        did_work
    }
    unsafe fn wait_and_process_events(this: *mut Self, active_sigmask: Option<&SigsetT>) {
        // SAFETY: the table's contract; the completions are members of the backend reached
        // through `this` (`hrt` is offset 0 of the hrtimer completion, so the user data it
        // stores dispatches to `HrtimerCompletion`).
        unsafe {
            let smp = std::ptr::addr_of_mut!((*this)._smp_wakeup_completion);
            let hrt = std::ptr::addr_of_mut!((*this)._hrtimer_completion.base);
            RecurringEventfdOrTimerfdCompletion::maybe_rearm(smp, this);
            RecurringEventfdOrTimerfdCompletion::maybe_rearm(hrt, this);
            io_uring_submit(&mut (*this)._uring);
            let preempt = std::ptr::addr_of_mut!((*this)._preempt_io_context);
            let mut did_work = false;
            did_work |= PreemptIoContext::service_preempting_io(preempt);
            did_work |= std::mem::replace(&mut (*this)._did_work_while_getting_sqe, false);
            if did_work {
                return;
            }
            let mut cqe: *mut IoUringCqe = std::ptr::null_mut();
            // io_uring_wait_cqes() wants non-const: the reactor's mask is passed as is.
            let sigs = active_sigmask.map_or(std::ptr::null(), |m| m as *const SigsetT);
            let before_wait_cqes = Reactor::now();
            let r = io_uring_wait_cqes(&mut (*this)._uring, &mut cqe, 1, sigs);
            // `_r` is the reactor that owns this backend; a short access to a counter.
            (*(*this)._r)._total_sleep += Reactor::now() - before_wait_cqes;
            if r < 0 {
                match -r {
                    EINTR => return,
                    // `default: abort();`
                    _ => std::process::abort(),
                }
            }
            did_work |= Self::do_process_kernel_completions(this);
            let _ = did_work;
            PreemptIoContext::service_preempting_io(preempt);
        }
    }
    unsafe fn reset_preemption_monitor(this: *mut Self) {
        // SAFETY: the table's contract.
        unsafe {
            PreemptIoContext::reset_preemption_monitor(std::ptr::addr_of_mut!(
                (*this)._preempt_io_context
            ))
        };
    }
    unsafe fn request_preemption(this: *mut Self) {
        // SAFETY: the table's contract.
        unsafe {
            PreemptIoContext::request_preemption(std::ptr::addr_of_mut!(
                (*this)._preempt_io_context
            ))
        };
    }
}

impl ReactorBackend for ReactorBackendUring {
    fn entries(&self) -> &'static BackendEntries {
        backend_entries!(ReactorBackendUring)
    }
    fn kernel_events_can_sleep(&self) -> bool {
        // We never need to spin while I/O is in flight.
        true
    }
    fn do_blocking_io(&self) -> bool {
        true
    }
    fn signal_received(&self, signo: i32, siginfo: *mut SiginfoT, ignore: *mut c_void) {
        crate::engine::core::reactor::Signals::action(signo, siginfo, ignore);
    }
    fn start_handling_signal(&self) {
        // We don't have anything special wrt. signals
    }
}

// The I/O entries of `BackendEntries` (the table's contract: `this` and `fd` are root
// pointers with no reference to them live). Every poll takes a submission entry, which may
// run completions (see `get_sqe`), so the backend and the descriptor states are reached
// through their root pointers and reborrowed only around code that runs no completion; the
// completions and the continuations keep those root pointers.
impl ReactorBackendUring {
    unsafe fn readable(this: *mut Self, fd: *mut PollableFdState) -> Future<()> {
        // SAFETY: forwarded (the table's contract).
        unsafe { Self::poll(this, fd, POLLIN) }
    }
    unsafe fn writeable(this: *mut Self, fd: *mut PollableFdState) -> Future<()> {
        // SAFETY: forwarded.
        unsafe { Self::poll(this, fd, POLLOUT) }
    }
    unsafe fn readable_or_writeable(this: *mut Self, fd: *mut PollableFdState) -> Future<()> {
        // SAFETY: forwarded.
        unsafe { Self::poll(this, fd, POLLIN | POLLOUT) }
    }
    unsafe fn poll_rdhup(this: *mut Self, fd: *mut PollableFdState) -> Future<()> {
        // SAFETY: forwarded.
        unsafe { Self::poll(this, fd, POLLRDHUP) }
    }
    unsafe fn forget(_this: *mut Self, fd: *mut PollableFdState) {
        // SAFETY: the table's contract: the state was allocated by `make_pollable_fd_state` as
        // a boxed `UringPollableFdState`, and `fd` is its root pointer.
        drop(unsafe { Box::from_raw(fd.cast::<UringPollableFdState>()) });
    }
    unsafe fn accept(
        this: *mut Self,
        listenfd: *mut PollableFdState,
    ) -> Future<(PollableFd, SocketAddress)> {
        // SAFETY: the table's contract; the reborrow of the state ends before the poll, and
        // the speculative accept runs no completion (`PollableFd::new` reaches the backend
        // through the table, for a state of its own).
        if unsafe { (*listenfd).take_speculation(POLLIN) } {
            let attempt = (|| -> Result<Option<(PollableFd, SocketAddress)>, ExceptionPtr> {
                // SAFETY: as above.
                let lfd = unsafe { &mut *listenfd };
                lfd.maybe_no_more_recv()?;
                let mut sa = SocketAddress::default();
                let maybe_fd = lfd.fd.try_accept(&mut sa, SOCK_CLOEXEC)?;
                if let Some(nfd) = maybe_fd {
                    lfd.speculate_epoll(EPOLLIN as i32);
                    let pfd = PollableFd::new(nfd, Speculation::new(EPOLLOUT as i32));
                    return Ok(Some((pfd, sa)));
                }
                Ok(None)
            })();
            match attempt {
                Ok(Some(v)) => return make_ready_future(v),
                Ok(None) => {}
                Err(e) => return make_exception_future(e),
            }
        }
        // SAFETY: forwarded; the continuation keeps the two root pointers.
        unsafe { Self::readable_or_writeable(this, listenfd) }.then(move |()| {
            // SAFETY: the backend and the listening fd state outlive the accept; the
            // continuation runs from the reactor's task loop, with no
            // reference to either live, and queues the request without running completions.
            unsafe {
                let mut desc = Box::new(AcceptCompletion {
                    base: IoCompletion::new::<AcceptCompletion>(),
                    _listenfd: listenfd,
                    _sa: SocketAddress::default(),
                    _result: Promise::new(),
                });
                let req = IoRequest::make_accept(
                    (*listenfd).fd.get(),
                    desc.posix_sockaddr(),
                    desc.socklen_ptr(),
                    SOCK_NONBLOCK | SOCK_CLOEXEC,
                );
                Self::submit_request(this, desc, req)
            }
        })
    }
    unsafe fn connect(
        this: *mut Self,
        fd: *mut PollableFdState,
        sa: &mut SocketAddress,
    ) -> Future<()> {
        let mut desc = Box::new(ConnectCompletion {
            base: IoCompletion::new::<ConnectCompletion>(),
            _fd: fd,
            _sa: *sa,
            _result: Promise::new(),
        });
        // SAFETY: the table's contract; a short read of the descriptor.
        let req = IoRequest::make_connect(
            unsafe { (*fd).fd.get() },
            desc.posix_sockaddr(),
            desc.socklen(),
        );
        // SAFETY: forwarded.
        unsafe { Self::submit_request(this, desc, req) }
    }
    unsafe fn shutdown(
        _this: *mut Self,
        fd: *mut PollableFdState,
        how: i32,
    ) -> Result<(), ExceptionPtr> {
        // SAFETY: the table's contract; a shared reborrow of the descriptor for one syscall.
        unsafe { (*fd).fd.shutdown(how) }
    }
    unsafe fn read(
        _this: *mut Self,
        fd: *mut PollableFdState,
        buffer: *mut c_void,
        len: usize,
    ) -> Future<usize> {
        // SAFETY: forwarded.
        unsafe { Reactor::do_read(fd, buffer, len) }
    }
    // The iovecs are taken as `&Vec`, as the table entry and the other backends' recvmsg.
    #[allow(clippy::ptr_arg)]
    unsafe fn recvmsg(
        this: *mut Self,
        fd: *mut PollableFdState,
        iov: &Vec<Iovec>,
    ) -> Future<usize> {
        // SAFETY: the table's contract; the reborrow runs syscalls only.
        let speculated = unsafe {
            let f = &mut *fd;
            if f.take_speculation(POLLIN) {
                let mut mh = Msghdr {
                    msg_iov: iov.as_ptr() as *mut Iovec,
                    msg_iovlen: iov.len(),
                    ..Msghdr::default()
                };
                // the message header and the buffers it describes are live for the call
                match f.fd.recvmsg(&mut mh, MSG_DONTWAIT) {
                    Ok(Some(r)) => {
                        if r == iovec_len(iov) {
                            f.speculate_epoll(EPOLLIN as i32);
                        }
                        Some(make_ready_future(r))
                    }
                    Ok(None) => None,
                    Err(e) => Some(make_exception_future(e)),
                }
            } else {
                None
            }
        };
        if let Some(f) = speculated {
            return f;
        }
        let mut desc = RecvmsgCompletion::new(fd, iov);
        // SAFETY: as above; a short read.
        let req = IoRequest::make_recvmsg(unsafe { (*fd).fd.get() }, desc.msghdr(), 0);
        // SAFETY: forwarded.
        unsafe { Self::submit_request(this, desc, req) }
    }
    unsafe fn read_some(
        this: *mut Self,
        fd: *mut PollableFdState,
        ba: *mut dyn BufferAllocator,
    ) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: the table's contract; the reborrow runs syscalls only, and the buffer
        // allocator outlives the read.
        let speculated = unsafe {
            let f = &mut *fd;
            if f.take_speculation(POLLIN) {
                let mut buffer = (*ba).allocate_buffer();
                // the buffer is valid for writes of the length passed
                match f.fd.read(buffer.get_write() as *mut c_void, buffer.size()) {
                    Ok(Some(r)) => {
                        if r == buffer.size() {
                            f.speculate_epoll(EPOLLIN as i32);
                        }
                        // the kernel read at most `buffer.size()` bytes
                        buffer.trim_unchecked(r);
                        Some(make_ready_future(buffer))
                    }
                    Ok(None) => None,
                    Err(e) => Some(make_exception_future(e)),
                }
            } else {
                None
            }
        };
        if let Some(f) = speculated {
            return f;
        }
        // SAFETY: forwarded; the continuation keeps the two root pointers.
        unsafe { Self::readable(this, fd) }.then(move |()| {
            // SAFETY: the backend, fd state and allocator outlive the read; the continuation
            // runs from the task loop with no reference to them live and queues the request
            // without running completions.
            unsafe {
                let mut desc = BufferReadCompletion::new(fd, (*ba).allocate_buffer());
                let req = IoRequest::make_read(
                    (*fd).fd.get(),
                    u64::MAX,
                    desc.get_write() as *mut c_void,
                    desc.get_size(),
                    false,
                );
                Self::submit_request(this, desc, req)
            }
        })
    }
    unsafe fn sendmsg(this: *mut Self, fd: *mut PollableFdState, p: &mut Packet) -> Future<usize> {
        const _: () = assert!(
            std::mem::size_of::<Iovec>()
                == std::mem::size_of::<crate::engine::net::packet::Fragment>()
                && std::mem::align_of::<Iovec>()
                    == std::mem::align_of::<crate::engine::net::packet::Fragment>(),
            "net::fragment and iovec should be equivalent"
        );
        // SAFETY: the table's contract; the reborrow runs syscalls only.
        let speculated = unsafe {
            let f = &mut *fd;
            if f.take_speculation(EPOLLOUT as i32) {
                let mh = Msghdr {
                    msg_iov: p.fragment_array() as *mut Iovec,
                    msg_iovlen: (p.nr_frags() as usize).min(IOV_MAX),
                    ..Msghdr::default()
                };
                // the message header and the buffers it describes are live for the call
                match f.fd.sendmsg(&mh, MSG_NOSIGNAL | MSG_DONTWAIT) {
                    Ok(Some(r)) => {
                        if r == p.len() as usize {
                            f.speculate_epoll(EPOLLOUT as i32);
                        }
                        Some(make_ready_future(r))
                    }
                    Ok(None) => None,
                    Err(e) => Some(make_exception_future(e)),
                }
            } else {
                None
            }
        };
        if let Some(f) = speculated {
            return f;
        }
        let mut desc = Box::new(WriteCompletion {
            base: IoCompletion::new::<WriteCompletion>(),
            _fd: fd,
            _mh: Msghdr::default(),
            _to_write: p.len() as usize,
            _result: Promise::new(),
        });
        desc._mh.msg_iov = p.fragment_array() as *mut Iovec;
        desc._mh.msg_iovlen = (p.nr_frags() as usize).min(IOV_MAX);
        // SAFETY: as above; a short read.
        let req = IoRequest::make_sendmsg(unsafe { (*fd).fd.get() }, desc.msghdr(), MSG_NOSIGNAL);
        // SAFETY: forwarded.
        unsafe { Self::submit_request(this, desc, req) }
    }
    unsafe fn send(
        this: *mut Self,
        fd: *mut PollableFdState,
        buffer: *const c_void,
        len: usize,
    ) -> Future<usize> {
        // SAFETY: the table's contract; the reborrow runs syscalls only, and the buffer is
        // valid for reads of the length passed.
        let speculated = unsafe {
            let f = &mut *fd;
            if f.take_speculation(EPOLLOUT as i32) {
                match f.fd.send(buffer, len, MSG_NOSIGNAL | MSG_DONTWAIT) {
                    Ok(Some(r)) => {
                        if r == len {
                            f.speculate_epoll(EPOLLOUT as i32);
                        }
                        Some(make_ready_future(r))
                    }
                    Ok(None) => None,
                    Err(e) => Some(make_exception_future(e)),
                }
            } else {
                None
            }
        };
        if let Some(f) = speculated {
            return f;
        }
        let desc = Box::new(WriteCompletion {
            base: IoCompletion::new::<WriteCompletion>(),
            _fd: fd,
            _mh: Msghdr::default(),
            _to_write: len,
            _result: Promise::new(),
        });
        // SAFETY: as above; a short read.
        let req = IoRequest::make_send(unsafe { (*fd).fd.get() }, buffer, len, MSG_NOSIGNAL);
        // SAFETY: forwarded.
        unsafe { Self::submit_request(this, desc, req) }
    }

    unsafe fn recv_some(
        this: *mut Self,
        fd: *mut PollableFdState,
        ba: *mut dyn BufferAllocator,
    ) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: the table's contract; the reborrow runs syscalls only, and the buffer
        // allocator outlives the read.
        let speculated = unsafe {
            let f = &mut *fd;
            if f.take_speculation(POLLIN) {
                let mut buffer = (*ba).allocate_buffer();
                // the buffer is valid for writes of the length passed
                match f.fd.recv(buffer.get_write() as *mut c_void, buffer.size(), MSG_DONTWAIT) {
                    Ok(Some(r)) => {
                        if r as usize == buffer.size() {
                            f.speculate_epoll(EPOLLIN as i32);
                        }
                        // the kernel read at most `buffer.size()` bytes
                        buffer.trim_unchecked(r as usize);
                        Some(make_ready_future(buffer))
                    }
                    Ok(None) => None,
                    Err(e) => Some(make_exception_future(e)),
                }
            } else {
                None
            }
        };
        if let Some(f) = speculated {
            return f;
        }
        // SAFETY: as above.
        let mut desc = BufferReadCompletion::new(fd, unsafe { (*ba).allocate_buffer() });
        // SAFETY: as above; a short read.
        let req = IoRequest::make_recv(
            unsafe { (*fd).fd.get() },
            desc.get_write() as *mut c_void,
            desc.get_size(),
            0,
        );
        // SAFETY: forwarded.
        unsafe { Self::submit_request(this, desc, req) }
    }

    unsafe fn start_tick(this: *mut Self) {
        // SAFETY: the table's contract; the context is reborrowed for one call.
        unsafe { (*std::ptr::addr_of_mut!((*this)._preempt_io_context)).start_tick() };
    }
    unsafe fn stop_tick(this: *mut Self) {
        // SAFETY: as in start_tick.
        unsafe { (*std::ptr::addr_of_mut!((*this)._preempt_io_context)).stop_tick() };
    }
    unsafe fn arm_highres_timer(this: *mut Self, its: &Itimerspec) {
        // An error arming the high resolution timer aborts: enabling a timer cannot fail.
        // SAFETY: the table's contract; a shared reborrow of the descriptor for one syscall
        // (this runs from timer callbacks, possibly inside `get_sqe`).
        if let Err(e) = unsafe {
            (*std::ptr::addr_of!((*this)._hrtimer_timerfd)).timerfd_settime(TFD_TIMER_ABSTIME, its)
        } {
            exception_in_noexcept(e);
        }
    }
    unsafe fn make_pollable_fd_state(
        _this: *mut Self,
        fd: FileDesc,
        speculate: Speculation,
    ) -> PollableFdStatePtr {
        let p = Box::into_raw(UringPollableFdState::new(fd, speculate)).cast::<PollableFdState>();
        // SAFETY: a fresh state from `Box::into_raw` of the derived state (`#[repr(C)]`, base
        // first): `p` keeps the provenance of the whole derived state, and is its root.
        unsafe {
            (*p)._this = p;
            PollableFdStatePtr::new(p)
        }
    }
}

fn detect_aio_poll() -> Result<bool, ExceptionPtr> {
    let fd = FileDesc::eventfd(0, 0)?;
    let mut ioc: AioContextT = 0;
    setup_aio_context(1, &mut ioc)?;
    let _cleanup = crate::engine::util::defer::defer(move || {
        io_destroy(ioc);
    });
    let mut iocb = make_poll_iocb(fd.get(), POLLIN | POLLOUT);
    let mut a: [*mut Iocb; 1] = [&mut iocb];
    // SAFETY: `a` holds one valid iocb.
    let r = unsafe { io_submit(ioc, 1, a.as_mut_ptr()) };
    if r != 1 {
        return Ok(false);
    }
    let one: u64 = 1;
    // SAFETY: the buffer is valid for reads of the length passed.
    unsafe { fd.write(&one as *const u64 as *const c_void, 8) }?;
    // SAFETY: `IoEvent` is a plain C struct for which all-zero bytes is a valid value.
    let mut ev: [IoEvent; 1] = unsafe { std::mem::zeroed() };
    // We set force_syscall = true (the last parameter) to ensure
    // the system call exists and is usable. If IOCB_CMD_POLL exists then
    // io_pgetevents() will also exist, but some versions of docker
    // have a syscall whitelist that does not include io_pgetevents(),
    // which causes it to fail with -EPERM. See
    // https://github.com/moby/moby/issues/38894.
    // SAFETY: `ioc` was set up above and is destroyed only by `_cleanup`, after this call;
    // `ev` has the one slot asked for; no timeout.
    let r = unsafe {
        aio::io_pgetevents_forced(ioc, 1, 1, ev.as_mut_ptr(), std::ptr::null_mut(), None, true)
    };
    Ok(r == 1)
}

impl ReactorBackendSelector {
    fn has_enough_aio_nr() -> Result<bool, ExceptionPtr> {
        let aio_max_nr = read_first_line_as::<u32>(Path::new("/proc/sys/fs/aio-max-nr"))?;
        let aio_nr = read_first_line_as::<u32>(Path::new("/proc/sys/fs/aio-nr"))?;
        // `available()` runs early, before any `io_setup` and not per CPU, so it checks the
        // AIO capacity left on the system against the per-CPU request times the CPU count.
        if aio_max_nr.wrapping_sub(aio_nr)
            < Reactor::MAX_AIO * crate::engine::core::smp::Smp::count()
        {
            return Ok(false);
        }
        Ok(true)
    }

    /// Creates the selected backend for reactor `r`.
    ///
    /// # Errors
    ///
    /// Returns the error of the backend's construction, or a logic error for an unknown
    /// name.
    ///
    /// # Safety
    /// `r` is the reactor that will own the backend, constructed in place, which outlives it.
    ///
    /// The backend lives in a node that is never moved, so the pointers its members keep to
    /// each other (and that the kernel keeps) stay valid; the node's pointer is the backend's
    /// root pointer, which `BackendEntries` take.
    pub unsafe fn create(
        &self,
        r: *mut Reactor,
    ) -> Result<NodePtr<dyn ReactorBackend>, ExceptionPtr> {
        // Each constructor's contract is this function's, which the caller guarantees. Each
        // `from_raw` takes back the pointer the node's `into_raw` just gave up: it comes from
        // `Box::into_raw`, points to an initialized backend and has no other owner, and the
        // unsizing cast keeps the allocation, so the new owner frees it as a
        // `dyn ReactorBackend` with the same layout.
        if self._name == "io_uring" {
            // SAFETY: this function's contract is the constructor's (see above).
            let n = unsafe { ReactorBackendUring::new(r) }?;
            // SAFETY: the pointer was just released by `into_raw` (see above).
            return Ok(unsafe { NodePtr::from_raw(n.into_raw() as *mut dyn ReactorBackend) });
        }
        if self._name == "linux-aio" {
            // SAFETY: this function's contract is the constructor's (see above).
            let n = unsafe { ReactorBackendAio::new(r) }?;
            // SAFETY: the pointer was just released by `into_raw` (see above).
            return Ok(unsafe { NodePtr::from_raw(n.into_raw() as *mut dyn ReactorBackend) });
        } else if self._name == "epoll" {
            // SAFETY: this function's contract is the constructor's (see above).
            let n = unsafe { ReactorBackendEpoll::new(r) }?;
            // SAFETY: the pointer was just released by `into_raw` (see above).
            return Ok(unsafe { NodePtr::from_raw(n.into_raw() as *mut dyn ReactorBackend) });
        }
        Err(ExceptionPtr::new(LogicError::new("bad reactor backend".to_string())))
    }

    /// The preferred available backend: io_uring, then linux-aio, then epoll.
    ///
    /// # Errors
    ///
    /// Returns the error of reading `/proc/sys/fs/aio-max-nr` or `aio-nr`, or of probing the
    /// kernel.
    pub fn default_backend() -> Result<ReactorBackendSelector, ExceptionPtr> {
        let mut available = Self::available()?;
        // `available()` always ends with epoll, so it is never empty.
        Ok(available.swap_remove(0))
    }

    /// The backends the running kernel supports, in order of preference.
    ///
    /// # Errors
    ///
    /// Returns the error of reading `/proc/sys/fs/aio-max-nr` or `aio-nr`, or of probing the
    /// kernel.
    pub fn available() -> Result<Vec<ReactorBackendSelector>, ExceptionPtr> {
        let mut ret = Vec::new();
        if detect_io_uring() {
            ret.push(ReactorBackendSelector::new("io_uring".to_string()));
        }
        if Self::has_enough_aio_nr()? && detect_aio_poll()? {
            ret.push(ReactorBackendSelector::new("linux-aio".to_string()));
        }
        ret.push(ReactorBackendSelector::new("epoll".to_string()));
        Ok(ret)
    }
}
