//! The CPU stall detector: reports tasks that run too long without yielding to the reactor.
//!
//! Each reactor thread has a [`CpuStallDetector`]. Before running tasks the reactor arms a
//! CPU-time timer for the stall threshold; when it fires, a real-time signal
//! ([`CpuStallDetector::signal_number`]) runs [`CpuStallDetector::on_signal`] on that thread,
//! which compares the reactor's task counter with the value seen at arm time. If no task
//! completed, the reactor is stalled and a backtrace is logged (rate-limited per minute), and
//! the timer is re-armed with a doubled period.
//!
//! Two timer sources exist ([`StallDetectorKind`]): a Linux `perf_event` task clock, which also
//! records the kernel call chain of the stall, and a POSIX CPU-time timer used when perf events
//! are unavailable (for example with `perf_event_paranoid` >= 2). Everything reached from the
//! signal handler is async-signal-safe and allocation-free.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering, compiler_fence, fence};
use std::time::Duration;

use crate::engine::core::future::{ExceptionPtr, SystemError};
use crate::engine::core::metrics;
use crate::engine::core::posix::{FileDesc, Itimerspec, to_relative_itimerspec};
use crate::engine::core::reactor::{
    BacktraceBuffer, Reactor, SchedDuration, SchedTimePoint, engine, engine_logger,
};
use crate::engine::core::reactor_backend::{F_GETFL, F_SETFL, errno};
use crate::engine::util::log::{DisplayExceptionPtr, FormatInfo};

/// Configuration of a [`CpuStallDetector`].
#[derive(Clone)]
pub struct CpuStallDetectorConfig {
    /// CPU time a task run may take before it is reported as a stall.
    pub threshold: Duration,
    /// Maximum number of stall reports per minute; further ones are counted and summarized.
    pub stall_detector_reports_per_minute: u32,
    /// Fraction of the threshold that a run may overshoot.
    pub slack: f32,
    /// Print a simplified backtrace on a single line.
    pub oneline: bool,
    /// Alternative reporting function, for tests.
    pub report: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Default for CpuStallDetectorConfig {
    fn default() -> Self {
        CpuStallDetectorConfig {
            threshold: Duration::from_secs(2),
            stall_detector_reports_per_minute: 1,
            slack: 0.3,
            oneline: true,
            report: None,
        }
    }
}

/// The control page the kernel maps in front of a perf event's ring buffer
/// (`struct perf_event_mmap_page`). Only `data_head`, `data_tail` and the data area fields are
/// read; the other fields keep the kernel layout.
#[repr(C)]
pub struct PerfEventMmapPage {
    /// Layout version.
    pub version: u32,
    /// Lowest compatible version.
    pub compat_version: u32,
    /// Sequence lock for the self-monitoring fields.
    pub lock: u32,
    /// Hardware counter index.
    pub index: u32,
    /// Counter offset.
    pub offset: i64,
    /// Time the event was enabled.
    pub time_enabled: u64,
    /// Time the event was running.
    pub time_running: u64,
    /// Capability bits.
    pub capabilities: u64,
    /// Counter width.
    pub pmc_width: u16,
    /// Time conversion shift.
    pub time_shift: u16,
    /// Time conversion multiplier.
    pub time_mult: u32,
    /// Time conversion offset.
    pub time_offset: u64,
    /// Time conversion zero.
    pub time_zero: u64,
    /// Header size.
    pub size: u32,
    /// Reserved.
    pub __reserved_1: u32,
    /// Cycle counter conversion.
    pub time_cycles: u64,
    /// Cycle counter mask.
    pub time_mask: u64,
    /// Padding up to the ring buffer fields.
    pub __reserved: [u8; 116 * 8],
    /// Ring buffer head, written by the kernel.
    pub data_head: u64,
    /// Ring buffer tail, written by the reader.
    pub data_tail: u64,
    /// Offset of the data area from the start of the mapping.
    pub data_offset: u64,
    /// Size of the data area.
    pub data_size: u64,
    /// AUX area head.
    pub aux_head: u64,
    /// AUX area tail.
    pub aux_tail: u64,
    /// AUX area offset.
    pub aux_offset: u64,
    /// AUX area size.
    pub aux_size: u64,
}

/// A POSIX timer handle.
pub type TimerT = *mut c_void;

/// The timer source of a stall detector.
pub enum StallDetectorKind {
    /// A POSIX CPU-time timer.
    PosixTimer(CpuStallDetectorPosixTimer),
    /// A Linux perf event task clock.
    LinuxPerfEvent(CpuStallDetectorLinuxPerfEvent),
}

/// Detects tasks that run for too long without returning to the reactor.
pub struct CpuStallDetector {
    /// The reactor's task counter when the timer was armed; 0 while no task runs.
    pub(crate) _last_tasks_processed_seen: AtomicU64,
    /// Reserved; currently always zero.
    pub(crate) _stall_detector_missed_ticks: AtomicU64,
    /// Reports made since the start of the current minute.
    pub(crate) _reported: u32,
    /// The total number of reports, in a node of its own: the `reported` metric reads it by
    /// the node's pointer, which stays valid while the detector moves (it is boxed after the
    /// metric is registered) and is written by the signal handler; relaxed atomic accesses
    /// keep a read interrupted by that handler whole.
    pub(crate) _total_reported: crate::engine::net::node_ptr::NodePtr<std::sync::atomic::AtomicU32>,
    /// Maximum reports per minute.
    pub(crate) _max_reports_per_minute: u32,
    /// The shard this detector belongs to.
    pub(crate) _shard_id: u32,
    /// Reserved; currently always zero.
    pub(crate) _thread_id: u32,
    /// Threshold multiple at which the next report is due (doubles after each report).
    pub(crate) _report_at: u32,
    /// Start of the current rate-limiting minute.
    pub(crate) _minute_mark: SchedTimePoint,
    /// Time after which the next task run re-arms the timer.
    pub(crate) _rearm_timer_at: SchedTimePoint,
    /// Start of the current task run.
    pub(crate) _run_started_at: SchedTimePoint,
    /// The stall threshold.
    pub(crate) _threshold: SchedDuration,
    /// The allowed overshoot.
    pub(crate) _slack: SchedDuration,
    /// The configuration.
    pub(crate) _config: CpuStallDetectorConfig,
    /// The detector's metrics.
    pub(crate) _metrics: metrics::MetricGroups,
    /// The timer source.
    pub(crate) kind: StallDetectorKind,
}

/// The clock the stall detector measures CPU time with.
pub type ClockType = crate::engine::core::thread_cputime_clock::ThreadCputimeClock;

/// A POSIX CPU-time timer source.
pub struct CpuStallDetectorPosixTimer {
    _timer: TimerT,
}

/// A Linux perf event task-clock source, which also samples the kernel call chain.
pub struct CpuStallDetectorLinuxPerfEvent {
    _fd: FileDesc,
    _enabled: bool,
    _current_period: u64,
    _mmap: *mut PerfEventMmapPage,
    _data_area: *mut u8,
    _data_area_mask: usize,
    // After the detector has been armed (`_enabled` is true), this is the moment at or after
    // which the next signal is expected to occur; used to detect spurious signals.
    _next_signal_time: SchedTimePoint,
}

/// Reads records from a perf event ring buffer; advances the kernel's tail on drop.
pub struct DataAreaReader<'a> {
    _p: &'a mut CpuStallDetectorLinuxPerfEvent,
    _data_area: *const u8,
    _data_area_mask: usize,
    _head: u64,
    _tail: u64,
}

impl<'a> DataAreaReader<'a> {
    /// A reader over `p`'s ring buffer, from the current tail to the current head.
    pub fn new(p: &'a mut CpuStallDetectorLinuxPerfEvent) -> Self {
        let data_area = p._data_area as *const u8;
        let mask = p._data_area_mask;
        // SAFETY: `_mmap` is the perf ring mapped in the constructor.
        let (head, tail) = unsafe {
            (
                std::ptr::read_volatile(&(*p._mmap).data_head),
                std::ptr::read_volatile(&(*p._mmap).data_tail),
            )
        };
        fence(Ordering::Acquire); // required after reading data_head
        DataAreaReader {
            _p: p,
            _data_area: data_area,
            _data_area_mask: mask,
            _head: head,
            _tail: tail,
        }
    }
    /// Reads the next 8 bytes.
    pub fn read_u64(&mut self) -> u64 {
        let mut ret = [0u8; 8];
        // We cannot wrap around if the 8-byte unit is aligned
        // SAFETY: the offset is masked into the data area of the ring.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self._data_area.add((self._tail as usize) & self._data_area_mask),
                ret.as_mut_ptr(),
                8,
            )
        };
        self._tail += 8;
        u64::from_ne_bytes(ret)
    }
    /// Reads the next `size_of::<S>()` bytes as an `S`.
    pub fn read_struct<S: Copy>(&mut self) -> S {
        assert!(std::mem::size_of::<S>().is_multiple_of(8));
        let mut ret = std::mem::MaybeUninit::<S>::uninit();
        let p = ret.as_mut_ptr() as *mut u8;
        let mut i = 0;
        while i != std::mem::size_of::<S>() {
            let w = self.read_u64();
            // SAFETY: `p + i` is within `ret`.
            unsafe { std::ptr::copy_nonoverlapping(w.to_ne_bytes().as_ptr(), p.add(i), 8) };
            i += 8;
        }
        // SAFETY: every byte of `ret` was written above; S is a plain record.
        unsafe { ret.assume_init() }
    }
    /// Skips `bytes_to_skip` bytes.
    pub fn skip(&mut self, bytes_to_skip: u64) {
        self._tail += bytes_to_skip;
    }
    /// Skips all the remaining data in the buffer, as if calling read until
    /// [`have_data`](Self::have_data) returns `false` (but much faster).
    pub fn skip_all(&mut self) {
        self._tail = self._head;
    }
    /// Whether unread data remains.
    pub fn have_data(&self) -> bool {
        self._head != self._tail
    }
}

impl<'a> Drop for DataAreaReader<'a> {
    fn drop(&mut self) {
        fence(Ordering::Release); // not documented, but probably required before writing data_tail
        // SAFETY: `_mmap` is the perf ring mapped in the constructor.
        unsafe { std::ptr::write_volatile(&mut (*self._p._mmap).data_tail, self._tail) };
    }
}

/// `struct sigevent` for SIGEV_THREAD_ID.
#[repr(C)]
struct Sigevent {
    sigev_value: usize,
    sigev_signo: i32,
    sigev_notify: i32,
    sigev_notify_thread_id: i32,
    _pad: [i32; 11],
}

/// `struct perf_event_attr` (PERF_ATTR_SIZE_VER0 fields and the flags word).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PerfEventAttr {
    type_: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    config1: u64,
}

/// `struct perf_event_header`
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PerfEventHeader {
    type_: u32,
    misc: u16,
    size: u16,
}

/// `struct f_owner_ex`
#[repr(C)]
struct FOwnerEx {
    type_: i32,
    pid: i32,
}

const SIGEV_THREAD_ID: i32 = 4;
const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
const PERF_TYPE_SOFTWARE: u32 = 1;
const PERF_COUNT_SW_TASK_CLOCK: u64 = 1;
const PERF_SAMPLE_CALLCHAIN: u64 = 1 << 5;
const PERF_RECORD_SAMPLE: u32 = 9;
const PERF_FLAG_FD_CLOEXEC: u64 = 1 << 3;
const PERF_ATTR_FLAG_DISABLED: u64 = 1 << 0;
const PERF_ATTR_FLAG_EXCLUDE_CALLCHAIN_USER: u64 = 1 << 22;
const PERF_EVENT_IOC_ENABLE: u64 = 0x2400;
const PERF_EVENT_IOC_DISABLE: u64 = 0x2401;
const PERF_EVENT_IOC_RESET: u64 = 0x2403;
const PERF_EVENT_IOC_PERIOD: u64 = 0x40082404;
const F_OWNER_TID: i32 = 0;
const F_SETOWN_EX: i32 = 15;
const F_SETSIG: i32 = 10;
const O_ASYNC: i32 = 0o20000;
const EACCES: i32 = 13;
const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_SHARED: i32 = 1;
const MAP_FAILED: *mut c_void = !0usize as *mut c_void;
use crate::engine::core::internal::linux_abi::nr::{
    GETTID as SYS_GETTID, PERF_EVENT_OPEN as NR_PERF_EVENT_OPEN,
};

unsafe extern "C" {
    fn timer_create(clockid: i32, sevp: *mut Sigevent, timerid: *mut TimerT) -> i32;
    fn timer_delete(timerid: TimerT) -> i32;
    fn timer_settime(
        timerid: TimerT,
        flags: i32,
        new_value: *const Itimerspec,
        old_value: *mut Itimerspec,
    ) -> i32;
    fn syscall(num: i64, ...) -> i64;
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64)
    -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> i32;
    fn getpagesize() -> i32;
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    fn __libc_current_sigrtmin() -> i32;
}

impl CpuStallDetector {
    /// The real-time signal the stall detector's timers deliver (`SIGRTMIN + 1`).
    pub fn signal_number() -> i32 {
        // SAFETY: plain libc call.
        unsafe { __libc_current_sigrtmin() + 1 }
    }

    /// Builds the detector around its timer source and registers its metrics.
    ///
    /// # Errors
    ///
    /// Returns the error of the metric registration.
    fn new_base(
        cfg: CpuStallDetectorConfig,
        kind: StallDetectorKind,
    ) -> Result<Box<Self>, ExceptionPtr> {
        let mut this = Box::new(CpuStallDetector {
            _last_tasks_processed_seen: AtomicU64::new(0),
            _stall_detector_missed_ticks: AtomicU64::new(0),
            _reported: 0,
            _total_reported: crate::engine::net::node_ptr::NodePtr::new(
                std::sync::atomic::AtomicU32::new(0),
            ),
            _max_reports_per_minute: 0,
            _shard_id: crate::engine::core::shard_id::this_shard_id(),
            _thread_id: 0,
            _report_at: 0,
            _minute_mark: SchedTimePoint::default(),
            _rearm_timer_at: SchedTimePoint::default(),
            _run_started_at: SchedTimePoint::default(),
            _threshold: SchedDuration::default(),
            _slack: SchedDuration::default(),
            _config: CpuStallDetectorConfig::default(),
            _metrics: metrics::MetricGroups::new(),
            kind,
        });
        // glib's backtrace() calls dlopen("libgcc_s.so.1") once to resolve unwind related symbols.
        // If first stall detector invocation happens during another dlopen() call the calling thread
        // will deadlock. The dummy call here makes sure that backtrace's initialization happens in
        // a safe place.
        crate::engine::util::backtrace::backtrace_with(
            |_f: crate::engine::util::backtrace::Frame| {},
            false,
        );
        this.update_config(cfg);

        use crate::engine::core::metrics as sm;
        let p = this._total_reported.as_ptr().cast_const();
        this._metrics.add_group(
            "stall_detector",
            vec![sm::make_counter(
                "reported",
                // SAFETY: the counter's node is owned by the detector, which owns the metric too;
                // no metric is read while the detector is dropped (both on this thread).
                move || unsafe { (*p).load(Ordering::Relaxed) } as u64,
                sm::description(
                    "Total number of reported stalls, look in the traces for the exact reason",
                ),
                vec![],
            )],
        )?;

        // note: if something is added here that can, it should take care to destroy _timer.
        Ok(this)
    }

    /// The current configuration.
    pub fn get_config(&self) -> CpuStallDetectorConfig {
        self._config.clone()
    }

    /// Applies a new configuration; the timer is re-armed with it on the next task run.
    pub fn update_config(&mut self, cfg: CpuStallDetectorConfig) {
        self._threshold = SchedDuration::from_std(cfg.threshold);
        self._slack = SchedDuration::from_std(cfg.threshold.mul_f32(cfg.slack));
        self._max_reports_per_minute = cfg.stall_detector_reports_per_minute;
        self._config = cfg;
        self._rearm_timer_at = Reactor::now();
    }

    fn maybe_report(&mut self) {
        let r = self._reported;
        self._reported += 1;
        if r < self._max_reports_per_minute {
            self.generate_trace();
        }
    }

    /// Handles a timer signal: reports a stall if no task completed since the timer was armed,
    /// then re-arms the timer.
    // We use a tick at every timer firing so we can report suppressed backtraces.
    // Best case it's a correctly predicted branch. If a backtrace had happened in
    // the near past it's an increment and two branches.
    //
    // We can do it a cheaper if we don't report suppressed backtraces.
    ///
    /// # Safety
    /// `this` is the live detector of this thread's reactor; called from the stall
    /// signal's handler, which forms no other reference to the reactor.
    pub unsafe fn on_signal(this: *mut Self) {
        // SAFETY: guaranteed by the caller; a plain read of the reactor's task counter, which
        // the signal handler may read while the reactor thread is interrupted.
        let tasks_processed = unsafe { *std::ptr::addr_of!((*engine())._global_tasks_processed) };
        // SAFETY: guaranteed by the caller.
        let this = unsafe { &mut *this };
        let last_seen = this._last_tasks_processed_seen.load(Ordering::Relaxed);
        if last_seen == 0 {
            return; // stall detector in not active
        } else if last_seen == tasks_processed {
            // we defer to the check of spuriousness to inside this unlikely condition
            // since the check itself can be costly, involving a syscall (reactor::now())
            if this.is_spurious_signal() {
                return;
            }
            // no task was processed - report unless supressed
            this.maybe_report();
            this._report_at <<= 1;
        } else {
            this._last_tasks_processed_seen.store(tasks_processed, Ordering::Relaxed);
        }
        if this.arm_timer().is_err() {
            // An error cannot leave the signal handler, and nothing that is not
            // async-signal-safe may run here, so the process aborts without a message.
            std::process::abort();
        }
    }

    fn report_suppressions(&mut self, now: SchedTimePoint) {
        if now > self._minute_mark + SchedDuration::from_std(Duration::from_secs(60)) {
            if self._reported > self._max_reports_per_minute {
                let suppressed = self._reported - self._max_reports_per_minute;
                let mut buf = BacktraceBuffer::new(false);
                // Reuse backtrace buffer infrastructure so we don't have to allocate here
                buf.append("Rate-limit: suppressed ");
                buf.append_decimal(suppressed as u64);
                if suppressed == 1 {
                    buf.append(" backtrace")
                } else {
                    buf.append(" backtraces")
                }
                buf.append(" on shard ");
                buf.append_decimal(self._shard_id as u64);
                buf.append("\n");
            }
            self.reset_suppression_state(now);
        }
    }

    /// Starts a new rate-limiting minute at `now`.
    pub(crate) fn reset_suppression_state(&mut self, now: SchedTimePoint) {
        self._reported = 0;
        self._minute_mark = now;
    }

    /// Called before the reactor runs tasks: re-arms the timer if due and records the task
    /// counter for the signal handler.
    ///
    /// # Errors
    ///
    /// Returns the error of the perf event `ioctl` that re-arms the timer.
    pub fn start_task_run(&mut self, now: SchedTimePoint) -> Result<(), ExceptionPtr> {
        if now > self._rearm_timer_at {
            self.report_suppressions(now);
            self._report_at = 1;
            self._run_started_at = now;
            self._rearm_timer_at = now + self._threshold * i64::from(self._report_at);
            self.arm_timer()?;
        }
        // SAFETY: on this shard's reactor thread; a plain read of the task counter.
        self._last_tasks_processed_seen.store(
            unsafe { *std::ptr::addr_of!((*engine())._global_tasks_processed) },
            Ordering::Relaxed,
        );
        compiler_fence(Ordering::Release); // Don't delay this write, so the signal handler can see it
        Ok(())
    }

    /// Called after the reactor ran tasks: disarms the check in the signal handler.
    pub fn end_task_run(&mut self, _now: SchedTimePoint) {
        compiler_fence(Ordering::Acquire); // Don't hoist this write, so the signal handler can see it
        self._last_tasks_processed_seen.store(0, Ordering::Relaxed);
    }

    /// Called when the reactor wakes from sleep (nothing to do).
    pub fn end_sleep(&mut self) {}

    /// Logs a stall report with the stall duration and the backtrace (or calls the test
    /// reporting function).
    pub fn generate_trace(&mut self) {
        let delta = Reactor::now() - self._run_started_at;

        // SAFETY: the node is live while the detector is; an atomic reached by `&`.
        let total = unsafe { self._total_reported.get() };
        total.store(total.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        if let Some(report) = &self._config.report {
            report();
            return;
        }

        let mut buf = BacktraceBuffer::new(false);
        buf.append("Reactor stalled for ");
        buf.append_decimal(delta.as_millis() as u64);
        buf.append(" ms");
        if std::thread::panicking() {
            buf.append(", backtrace omitted (uncaught exception in progress)\n");
        } else {
            crate::engine::core::reactor::print_with_backtrace_into(&mut buf, self._config.oneline);
        }
        self.maybe_report_kernel_trace(&mut buf);
    }

    // Dispatch to the timer source.

    fn is_spurious_signal(&mut self) -> bool {
        match &mut self.kind {
            StallDetectorKind::PosixTimer(_) => false,
            StallDetectorKind::LinuxPerfEvent(p) => p.is_spurious_signal(),
        }
    }

    fn maybe_report_kernel_trace(&mut self, buf: &mut BacktraceBuffer) {
        match &mut self.kind {
            StallDetectorKind::PosixTimer(_) => {}
            StallDetectorKind::LinuxPerfEvent(p) => p.maybe_report_kernel_trace(buf),
        }
    }

    fn arm_timer(&mut self) -> Result<(), ExceptionPtr> {
        let period = self._threshold * i64::from(self._report_at) + self._slack;
        match &mut self.kind {
            StallDetectorKind::PosixTimer(p) => {
                p.arm_timer(period);
                Ok(())
            }
            StallDetectorKind::LinuxPerfEvent(p) => p.arm_timer(period),
        }
    }

    /// Called before the reactor sleeps: stops the timer so sleeping is not a stall.
    ///
    /// # Errors
    ///
    /// Returns the error of the perf event `ioctl`.
    pub fn start_sleep(&mut self) -> Result<(), ExceptionPtr> {
        match &mut self.kind {
            StallDetectorKind::PosixTimer(p) => {
                p.start_sleep();
                self._rearm_timer_at = Reactor::now();
                Ok(())
            }
            StallDetectorKind::LinuxPerfEvent(p) => p.start_sleep(),
        }
    }
}

impl CpuStallDetectorPosixTimer {
    /// A detector using a POSIX CPU-time timer that signals the calling thread.
    ///
    /// # Errors
    ///
    /// Returns the system error of `timer_create` or of the metric registration.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(cfg: CpuStallDetectorConfig) -> Result<Box<CpuStallDetector>, ExceptionPtr> {
        let mut sev = Sigevent {
            sigev_value: 0,
            sigev_signo: 0,
            sigev_notify: 0,
            sigev_notify_thread_id: 0,
            _pad: [0; 11],
        };
        sev.sigev_notify = SIGEV_THREAD_ID;
        sev.sigev_signo = CpuStallDetector::signal_number();
        // SAFETY: plain gettid system call.
        sev.sigev_notify_thread_id = unsafe { syscall(SYS_GETTID) } as i32;
        let mut timer: TimerT = std::ptr::null_mut();
        // SAFETY: `sev` and `timer` are valid.
        let err = unsafe { timer_create(CLOCK_THREAD_CPUTIME_ID, &mut sev, &mut timer) };
        if err != 0 {
            return Err(ExceptionPtr::from(SystemError::from_errno(err)));
        }
        CpuStallDetector::new_base(
            cfg,
            StallDetectorKind::PosixTimer(CpuStallDetectorPosixTimer { _timer: timer }),
        )
    }

    fn arm_timer(&mut self, period: SchedDuration) {
        let its = to_relative_itimerspec(period.to_std(), Duration::ZERO);
        // SAFETY: `_timer` was created by timer_create.
        unsafe { timer_settime(self._timer, 0, &its, std::ptr::null_mut()) };
    }

    fn start_sleep(&mut self) {
        let its = to_relative_itimerspec(Duration::ZERO, Duration::ZERO);
        // SAFETY: as above.
        unsafe { timer_settime(self._timer, 0, &its, std::ptr::null_mut()) };
    }
}

impl Drop for CpuStallDetectorPosixTimer {
    fn drop(&mut self) {
        // SAFETY: `_timer` was created by timer_create.
        unsafe { timer_delete(self._timer) };
    }
}

fn perf_event_open(
    hw_event: *mut PerfEventAttr,
    pid: i32,
    cpu: i32,
    group_fd: i32,
    flags: u64,
) -> i64 {
    // SAFETY: `hw_event` is a valid perf_event_attr.
    unsafe { syscall(NR_PERF_EVENT_OPEN, hw_event, pid, cpu, group_fd, flags) }
}

impl CpuStallDetectorLinuxPerfEvent {
    /// A detector using the perf event `fd`, which must already deliver its signal to the
    /// calling thread.
    ///
    /// # Errors
    ///
    /// Returns the error of mapping the ring buffer or of the metric registration.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        fd: FileDesc,
        cfg: CpuStallDetectorConfig,
    ) -> Result<Box<CpuStallDetector>, ExceptionPtr> {
        // SAFETY: `getpagesize` takes no arguments and has no preconditions.
        let pagesize = unsafe { getpagesize() } as usize;
        // SAFETY: a new shared mapping (null hint, no fixed address) of the perf ring buffer
        // of the open descriptor `fd`; it overlaps no existing memory, and failure is checked.
        let ret = unsafe {
            mmap(
                std::ptr::null_mut(),
                2 * pagesize,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                fd.get(),
                0,
            )
        };
        if ret == MAP_FAILED {
            std::process::abort();
        }
        let mmap_page = ret as *mut PerfEventMmapPage;
        // SAFETY: the data area follows the first page of the mapping.
        let data_area = unsafe { (mmap_page as *mut u8).add(pagesize) };
        let this = CpuStallDetectorLinuxPerfEvent {
            _fd: fd,
            _enabled: false,
            _current_period: 0,
            _mmap: mmap_page,
            _data_area: data_area,
            _data_area_mask: pagesize - 1,
            _next_signal_time: SchedTimePoint::default(),
        };
        CpuStallDetector::new_base(cfg, StallDetectorKind::LinuxPerfEvent(this))
    }

    fn arm_timer(&mut self, period: SchedDuration) -> Result<(), ExceptionPtr> {
        let mut ns = period.count() as u64;
        self._next_signal_time = Reactor::now() + period;

        // clear out any existing records in the ring buffer, so when we get interrupted next time
        // we have only the stack associated with that interrupt, and so we don't overflow.
        DataAreaReader::new(self).skip_all();
        if self._enabled && self._current_period == ns {
            // Common case - we're re-arming with the same period, the counter
            // is already enabled.

            // We want to set the next interrupt to ns from now, and somewhat oddly the
            // way to do this is PERF_EVENT_IOC_PERIOD, even with the same period as
            // already configured, see the code at:
            //
            // https://elixir.bootlin.com/linux/v5.15.86/source/kernel/events/core.c#L5636
            //
            // Ths change is intentional: kernel commit bad7192b842c83e580747ca57104dd51fe08c223
            // so we can resumably rely on it.
            // SAFETY: the request reads and writes the value passed, of the type it expects.
            unsafe { self._fd.ioctl(PERF_EVENT_IOC_PERIOD, &mut ns) }?;
        } else {
            // Uncommon case - we're moving from disabled to enabled, or changing
            // the period. Issue more calls and be careful.
            self._fd.ioctl_int(PERF_EVENT_IOC_DISABLE, 0)?; // avoid false alarms while we modify stuff
            // SAFETY: the request reads and writes the value passed, of the type it expects.
            unsafe { self._fd.ioctl(PERF_EVENT_IOC_PERIOD, &mut ns) }?;
            self._fd.ioctl_int(PERF_EVENT_IOC_RESET, 0)?;
            self._fd.ioctl_int(PERF_EVENT_IOC_ENABLE, 0)?;
            self._enabled = true;
            self._current_period = ns;
        }
        Ok(())
    }

    fn start_sleep(&mut self) -> Result<(), ExceptionPtr> {
        self._fd.ioctl_int(PERF_EVENT_IOC_DISABLE, 0)?;
        self._enabled = false;
        Ok(())
    }

    fn is_spurious_signal(&mut self) -> bool {
        // If the current time is before the expected signal time, it is
        // probably a spurious signal. One reason this could occur is that
        // PERF_EVENT_IOC_PERIOD does not reset the current overflow point
        // on kernels prior to 3.14 (or 3.7 on Arm).
        Reactor::now() < self._next_signal_time
    }

    fn maybe_report_kernel_trace(&mut self, buf: &mut BacktraceBuffer) {
        let mut reader = DataAreaReader::new(self);

        while reader.have_data() {
            let record: PerfEventHeader = reader.read_struct::<PerfEventHeader>();

            if record.type_ != PERF_RECORD_SAMPLE {
                reader.skip(record.size as u64 - std::mem::size_of::<PerfEventHeader>() as u64);
                continue;
            }

            let nr = reader.read_u64();
            if nr > 0 {
                buf.append("kernel callstack:");
                for _ in 0..nr {
                    buf.append(" 0x");
                    buf.append_hex(reader.read_u64());
                }
                buf.append("\n");
            }
        }
    }

    /// Opens a task-clock perf event for the calling thread, routes its signal to the thread
    /// and builds a detector on it.
    ///
    /// # Errors
    ///
    /// Returns the system error of `perf_event_open` (for example `EACCES` when
    /// `perf_event_paranoid` forbids it).
    pub fn try_make(cfg: CpuStallDetectorConfig) -> Result<Box<CpuStallDetector>, ExceptionPtr> {
        let mut pea = PerfEventAttr {
            type_: PERF_TYPE_SOFTWARE,
            size: std::mem::size_of::<PerfEventAttr>() as u32,
            config: PERF_COUNT_SW_TASK_CLOCK, // more likely to work on virtual machines than hardware events
            sample_period: 1_000_000_000, // Needs non-zero value or PERF_IOC_PERIOD gets confused
            sample_type: PERF_SAMPLE_CALLCHAIN,
            // disabled = 1, exclude_callchain_user = 1 (we're using backtrace() to capture the user callchain)
            flags: PERF_ATTR_FLAG_DISABLED | PERF_ATTR_FLAG_EXCLUDE_CALLCHAIN_USER,
            wakeup_events: 1,
            ..PerfEventAttr::default()
        };
        let mut flags: u64 = 0;
        if crate::engine::core::internal::uname::kernel_uname().whitelisted(&["3.14"]) {
            flags |= PERF_FLAG_FD_CLOEXEC;
        }
        let fd = perf_event_open(&mut pea, 0, -1, -1, flags) as i32;
        if fd == -1 {
            return Err(ExceptionPtr::from(SystemError::new(errno(), "perf_event_open() failed")));
        }
        let desc = FileDesc::from_fd(fd);
        let sig_owner = FOwnerEx {
            type_: F_OWNER_TID,
            // SAFETY: plain gettid system call.
            pid: unsafe { syscall(SYS_GETTID) } as i32,
        };
        // SAFETY: plain fcntl calls on the perf descriptor.
        unsafe {
            let ret1 = fcntl(fd, F_SETOWN_EX, &sig_owner as *const FOwnerEx);
            if ret1 == -1 {
                std::process::abort();
            }
            let ret2 = fcntl(fd, F_SETSIG, CpuStallDetector::signal_number());
            if ret2 == -1 {
                std::process::abort();
            }
            let fd_flags = fcntl(fd, F_GETFL);
            if fd_flags == -1 {
                std::process::abort();
            }
            let ret3 = fcntl(fd, F_SETFL, fd_flags | O_ASYNC);
            if ret3 == -1 {
                std::process::abort();
            }
        }
        CpuStallDetectorLinuxPerfEvent::new(desc, cfg)
    }
}

impl Drop for CpuStallDetectorLinuxPerfEvent {
    fn drop(&mut self) {
        // SAFETY: unmapping what the constructor mapped.
        unsafe { munmap(self._mmap as *mut c_void, 2 * getpagesize() as usize) };
    }
}

/// Creates the calling thread's stall detector: a perf event detector if possible, otherwise
/// a POSIX timer detector (logging why).
///
/// # Errors
///
/// Returns the error of the POSIX timer detector when neither can be created.
pub fn make_cpu_stall_detector(
    cfg: CpuStallDetectorConfig,
) -> Result<Box<CpuStallDetector>, ExceptionPtr> {
    match CpuStallDetectorLinuxPerfEvent::try_make(cfg.clone()) {
        Ok(d) => Ok(d),
        Err(e) => {
            // This failure occurs when /proc/sys/kernel/perf_event_paranoid is set
            // to 2 or higher, and is expected since most distributions set it to that
            // way as of 2023. In this case we log a different message and only at INFO
            // level on shard 0.
            let was_eaccess_failure =
                e.downcast_ref::<SystemError>().is_some_and(|s| s.code() == EACCES);
            if was_eaccess_failure {
                engine_logger().info0(FormatInfo::new("Perf-based stall detector creation failed (EACCESS), try setting /proc/sys/kernel/perf_event_paranoid to 1 or less to enable kernel backtraces: falling back to posix timer."), format_args!(
                    "Perf-based stall detector creation failed (EACCESS), try setting /proc/sys/kernel/perf_event_paranoid to 1 or less to enable kernel backtraces: falling back to posix timer."
                ));
            } else {
                engine_logger().warn(FormatInfo::new("Creation of perf_event based stall detector failed: falling back to posix timer: {}"), format_args!("Creation of perf_event based stall detector failed: falling back to posix timer: {}", DisplayExceptionPtr(Some(&e))));
            }
            CpuStallDetectorPosixTimer::new(cfg)
        }
    }
}

/// Samples reactor stalls while a function under test runs (see the stall sampler module).
pub use crate::engine::core::reactor::internal_misc::ReactorStallSampler;
