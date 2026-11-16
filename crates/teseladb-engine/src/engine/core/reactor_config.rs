//! Reactor configuration.
//!
//! [`ReactorOptions`] is the user-facing option group (command-line style options such as the
//! task quota, the I/O backend or polling behaviour). The reactor reads it at start-up into a
//! [`ReactorConfig`], the plain values it consults at run time on every shard.
use std::pin::Pin;

use crate::engine::core::memory::AllocFailureKind;
use crate::engine::core::reactor::{NetworkStackFactory, SchedDuration};
use crate::engine::core::reactor_backend::ReactorBackendSelector;
use crate::engine::util::program_options::{OptionGroup, SelectionValue, Switch, Value};

/// The run-time configuration of a reactor, derived from [`ReactorOptions`].
#[derive(Clone, Debug)]
pub struct ReactorConfig {
    /// Maximum time a task may run before the reactor polls again.
    pub task_quota: SchedDuration,
    /// Maximum time the reactor may go without polling.
    pub max_poll_time: SchedDuration,
    /// Whether the reactor installs a SIGINT handler.
    pub handle_sigint: bool,
    /// Whether SIGINT and SIGTERM stop the reactor automatically.
    pub auto_handle_sigint_sigterm: bool,
    /// Maximum number of linux-aio control blocks used for networking per shard.
    pub max_networking_aio_io_control_blocks: u32,
    /// Make `io_getevents` always issue the system call instead of reading the completion ring.
    pub force_io_getevents_syscall: bool,
    /// Use the kernel page cache (buffered I/O) instead of `O_DIRECT`.
    pub kernel_page_cache: bool,
    /// Whether `fsync` is submitted through linux-aio.
    pub have_aio_fsync: bool,
    /// Number of queued tasks above which the reactor stops polling for new events.
    pub max_task_backlog: u32,
    /// Fail instead of falling back to buffered I/O when `O_DIRECT` is unavailable.
    pub strict_o_direct: bool,
    /// Turn `fsync` into a no-op (may lose data; for testing).
    pub bypass_fsync: bool,
    /// Do not busy-poll for disk I/O completions.
    pub no_poll_aio: bool,
}

impl Default for ReactorConfig {
    fn default() -> Self {
        ReactorConfig {
            task_quota: SchedDuration::default(),
            max_poll_time: SchedDuration::default(),
            handle_sigint: true,
            auto_handle_sigint_sigterm: true,
            max_networking_aio_io_control_blocks: 10000,
            force_io_getevents_syscall: false,
            kernel_page_cache: false,
            have_aio_fsync: false,
            max_task_backlog: 1000,
            strict_o_direct: true,
            bypass_fsync: false,
            no_poll_aio: false,
        }
    }
}

/// Configuration for the reactor.
pub struct ReactorOptions {
    pub(crate) group: OptionGroup,
    /// Selects the network stack to use.
    ///
    /// Each network stack has its own option group to further tune it. The available stacks
    /// are:
    /// * POSIX stack (default): no tunable options;
    /// * native stack: tuned by the native stack options.
    pub network_stack: SelectionValue<NetworkStackFactory>,
    /// Poll continuously (100% cpu use).
    pub poll_mode: Switch,
    /// Idle polling time in microseconds.
    ///
    /// Reduce for overprovisioned environments or laptops.
    pub idle_poll_time_us: Value<u32>,
    /// Busy-poll for disk I/O.
    ///
    /// Reduces latency and increases throughput.
    pub poll_aio: Value<bool>,
    /// Maximum time (ms) between polls.
    ///
    /// Default: 0.5.
    pub task_quota_ms: Value<f64>,
    /// Maximum time (ms) I/O operations should take.
    ///
    /// Default: 1.5 * task_quota_ms value
    pub io_latency_goal_ms: Value<f64>,
    /// Dispatch rate to completion rate ratio threshold.
    ///
    /// The worst ratio at which the reactor may delay I/O request completion. If exceeded,
    /// the scheduler considers the disk the reason for the completion slow-down and scales
    /// down dispatching.
    ///
    /// Default: 1.1
    pub io_flow_ratio_threshold: Value<f64>,
    /// If an I/O request takes longer than this, it is logged with extra debugging
    /// information.
    ///
    /// Default: infinite (detection is OFF)
    pub io_completion_notify_ms: Value<u32>,
    /// Maximum task backlog to allow.
    ///
    /// When the number of tasks grow above this, we stop polling (e.g. I/O)
    /// until it goes back below the limit.
    /// Default: 1000.
    pub max_task_backlog: Value<u32>,
    /// Threshold in milliseconds over which the reactor is considered blocked if no progress
    /// is made.
    ///
    /// Default: 25.
    pub blocked_reactor_notify_ms: Value<u32>,
    /// Maximum number of backtraces reported by the stall detector per minute.
    ///
    /// Default: 5.
    pub blocked_reactor_reports_per_minute: Value<u32>,
    /// Print a simplified backtrace on a single line.
    ///
    /// Default: `true`.
    pub blocked_reactor_report_format_oneline: Value<bool>,
    /// Allow using buffered I/O if DMA is not available (reduces performance).
    pub relaxed_dma: Switch,
    /// Use the Linux `RWF_NOWAIT` asynchronous I/O feature, which reduces reactor stalls due
    /// to linux-aio (autodetected).
    pub linux_aio_nowait: Value<bool>,
    /// Bypass `fsync()`; may result in data loss.
    ///
    /// Use for testing on consumer drives.
    /// Default: `false`.
    pub unsafe_bypass_fsync: Value<bool>,
    /// Use the kernel page cache.
    ///
    /// This disables DMA (`O_DIRECT`). Useful for short-lived functional tests with a small
    /// data set.
    /// Default: `false`.
    pub kernel_page_cache: Value<bool>,
    /// Run in an overprovisioned environment (such as a container or a laptop).
    ///
    /// Equivalent to:
    /// * `idle_poll_time_us` = 0
    /// * the SMP option `thread_affinity` = 0
    /// * `poll_aio` = 0
    pub overprovisioned: Switch,
    /// Abort when the engine's allocator cannot allocate memory.
    pub abort_on_engine_bad_alloc: Switch,
    /// Force `io_getevents(2)` to issue a system call, instead of bypassing the kernel when
    /// possible.
    ///
    /// This makes `strace` output more useful, but slows down the application.
    /// Default: `false`.
    pub force_aio_syscalls: Value<bool>,
    /// Dump diagnostics of the engine allocator state on allocation failure.
    ///
    /// See [`AllocFailureKind`] for the allowed values. The diagnostics are written to the
    /// `engine_memory` logger at error level, regardless of that logger's configured level.
    /// Default: [`AllocFailureKind`]'s critical kind.
    pub dump_memory_diagnostics_on_alloc_failure_kind: Value<AllocFailureKind>,
    /// Reactor I/O backend.
    ///
    /// Available backends:
    /// * `linux-aio`
    /// * `epoll`
    /// * `io_uring`
    ///
    /// Default: `linux-aio` (if available).
    pub reactor_backend: SelectionValue<ReactorBackendSelector>,
    /// Use linux-aio for `fsync()` calls.
    ///
    /// This reduces latency. Requires Linux 4.18 or later.
    pub aio_fsync: Value<bool>,
    /// Maximum number of I/O control blocks (IOCBs) to allocate per shard.
    ///
    /// This bounds the number of sockets supported per shard. Requires tuning
    /// `/proc/sys/fs/aio-max-nr`. Only valid for the `linux-aio` reactor backend (see
    /// `reactor_backend`).
    /// Default: 10000.
    pub max_networking_io_control_blocks: Value<u32>,
    /// Leave this many I/O control blocks (IOCBs) as reserve.
    ///
    /// This leaves a (small) reserve aside so that other applications also using IOCBs can
    /// run alongside this one. The reserve takes precedence over
    /// `max_networking_io_control_blocks`.
    ///
    /// Default: 0
    pub reserve_io_control_blocks: Value<u32>,
    /// Enable heap profiling.
    ///
    /// Allocations are sampled every N bytes on average. Zero means off. Unused when the
    /// crate is built without heap profiling support.
    ///
    /// Default: 0
    pub heapprof: Value<u32>,
    /// Ignore SIGINT (for debuggers).
    pub no_handle_interrupt: Switch,
    /// The program name (`argv[0]`), for diagnostics.
    pub _argv0: String,
    /// Whether SIGINT and SIGTERM stop the reactor automatically.
    pub _auto_handle_sigint_sigterm: bool,
}

impl ReactorOptions {
    /// Second construction phase: links the group under `parent_group` and registers every
    /// value with it, in place and in declaration order. The values themselves are built by
    /// the reactor module.
    pub fn link(self: Pin<&mut Self>, parent_group: Option<Pin<&mut OptionGroup>>) {
        crate::link_option_group!(
            self,
            parent_group,
            group,
            network_stack,
            poll_mode,
            idle_poll_time_us,
            poll_aio,
            task_quota_ms,
            io_latency_goal_ms,
            io_flow_ratio_threshold,
            io_completion_notify_ms,
            max_task_backlog,
            blocked_reactor_notify_ms,
            blocked_reactor_reports_per_minute,
            blocked_reactor_report_format_oneline,
            relaxed_dma,
            linux_aio_nowait,
            unsafe_bypass_fsync,
            kernel_page_cache,
            overprovisioned,
            abort_on_engine_bad_alloc,
            force_aio_syscalls,
            dump_memory_diagnostics_on_alloc_failure_kind,
            reactor_backend,
            aio_fsync,
            max_networking_io_control_blocks,
            reserve_io_control_blocks,
            heapprof,
            no_handle_interrupt
        );
    }

    /// The underlying option group.
    #[inline]
    pub fn group(&self) -> &OptionGroup {
        &self.group
    }

    /// The underlying option group, mutably.
    #[inline]
    pub fn group_mut(&mut self) -> &mut OptionGroup {
        &mut self.group
    }
}
