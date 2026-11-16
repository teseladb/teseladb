//! Hardware and software performance counters through `perf_event_open(2)`.
//!
//! [`LinuxPerfEvent`] owns one counter file descriptor, opened from a [`PerfEventAttr`]
//! (the kernel's `struct perf_event_attr`), and can enable, disable and read it. The
//! micro-benchmark framework uses it to report instructions and cycles per iteration.
//! Opening fails silently (for example in containers without access to perf events): the
//! counter then reads as 0. On other systems than Linux it is always in that state.

#[cfg(target_os = "linux")]
use std::ffi::c_long;
use std::ffi::{c_int, c_ulong, c_void};

use crate::engine_assert;

/// `struct perf_event_attr` from `<linux/perf_event.h>` (`PERF_ATTR_SIZE_VER8`).
#[repr(C)]
#[derive(Default)]
pub struct PerfEventAttr {
    /// Major type: hardware/software/tracepoint/etc.
    pub type_: u32,
    /// Size of the attr structure, for fwd/bwd compat.
    pub size: u32,
    /// Type specific configuration information.
    pub config: u64,
    /// `sample_period` / `sample_freq`
    pub sample_period: u64,
    /// `sample_type`
    pub sample_type: u64,
    /// `read_format`
    pub read_format: u64,
    /// The bit fields: disabled (bit 0), ..., exclude_kernel (5), exclude_hv (6),
    /// exclude_idle (7), ...
    pub flags: u64,
    /// `wakeup_events` / `wakeup_watermark`
    pub wakeup_events: u32,
    /// `bp_type`
    pub bp_type: u32,
    /// `bp_addr` / `config1`
    pub config1: u64,
    /// `bp_len` / `config2`
    pub config2: u64,
    /// `branch_sample_type`
    pub branch_sample_type: u64,
    /// `sample_regs_user`
    pub sample_regs_user: u64,
    /// `sample_stack_user`
    pub sample_stack_user: u32,
    /// `clockid`
    pub clockid: i32,
    /// `sample_regs_intr`
    pub sample_regs_intr: u64,
    /// `aux_watermark`
    pub aux_watermark: u32,
    /// `sample_max_stack`
    pub sample_max_stack: u16,
    /// `__reserved_2`
    pub reserved_2: u16,
    /// `aux_sample_size`
    pub aux_sample_size: u32,
    /// `__reserved_3`
    pub reserved_3: u32,
    /// `sig_data`
    pub sig_data: u64,
    /// `config3`
    pub config3: u64,
}

const PERF_TYPE_HARDWARE: u32 = 0;
const PERF_COUNT_HW_CPU_CYCLES: u64 = 0;
const PERF_COUNT_HW_INSTRUCTIONS: u64 = 1;
const ATTR_DISABLED: u64 = 1 << 0;
const ATTR_EXCLUDE_KERNEL: u64 = 1 << 5;
const ATTR_EXCLUDE_HV: u64 = 1 << 6;
#[cfg(target_arch = "x86_64")]
const ATTR_EXCLUDE_IDLE: u64 = 1 << 7;
/// `PERF_EVENT_IOC_ENABLE`: `_IO('$', 0)`
const PERF_EVENT_IOC_ENABLE: c_ulong = 0x2400;
/// `PERF_EVENT_IOC_DISABLE`: `_IO('$', 1)`
const PERF_EVENT_IOC_DISABLE: c_ulong = 0x2401;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const NR_PERF_EVENT_OPEN: c_long = 298;
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const NR_PERF_EVENT_OPEN: c_long = 241;

unsafe extern "C" {
    #[cfg(target_os = "linux")]
    fn syscall(number: c_long, ...) -> c_long;
    fn close(fd: c_int) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

/// A hardware counter of the calling thread; the counters read 0 where the kernel
/// refuses them (containers, other systems).
#[derive(Debug)]
pub struct LinuxPerfEvent {
    _fd: c_int,
}

impl LinuxPerfEvent {
    /// Opens a counter (`perf_event_open(2)`).
    pub fn new(
        attr: &PerfEventAttr,
        pid: i32,
        cpu: c_int,
        group_fd: c_int,
        flags: c_ulong,
    ) -> Self {
        #[cfg(target_os = "linux")]
        // SAFETY: perf_event_open reads `attr`, which is valid for the call.
        let ret = unsafe {
            syscall(NR_PERF_EVENT_OPEN, attr as *const PerfEventAttr, pid, cpu, group_fd, flags)
        } as c_int;
        #[cfg(not(target_os = "linux"))]
        let ret: c_int = {
            let _ = (attr, pid, cpu, group_fd, flags);
            -1
        };
        let mut fd = -1;
        if ret != -1 {
            fd = ret; // ignore failures, can happen in constrained environments such as containers
        }
        LinuxPerfEvent { _fd: fd }
    }

    /// Replaces this counter with `x`, closing the current descriptor; `x` is left empty.
    pub fn assign(&mut self, mut x: LinuxPerfEvent) {
        if self._fd != -1 {
            // SAFETY: the fd is ours.
            unsafe { close(self._fd) };
        }
        self._fd = std::mem::replace(&mut x._fd, -1);
    }

    /// The counter value.
    pub fn read(&mut self) -> u64 {
        if self._fd == -1 {
            return 0;
        }
        let mut ret: u64 = 0;
        // SAFETY: `ret` has room for the 8 bytes read.
        let res = unsafe { read(self._fd, (&mut ret as *mut u64).cast(), size_of::<u64>()) };
        // "read(2) failed on perf_event fd"
        engine_assert!(res == size_of::<u64>() as isize);
        ret
    }

    /// Starts counting.
    pub fn enable(&mut self) {
        if self._fd == -1 {
            return;
        }
        // SAFETY: an ioctl on our fd.
        unsafe { ioctl(self._fd, PERF_EVENT_IOC_ENABLE, 0) };
    }

    /// Stops counting.
    pub fn disable(&mut self) {
        if self._fd == -1 {
            return;
        }
        // SAFETY: an ioctl on our fd.
        unsafe { ioctl(self._fd, PERF_EVENT_IOC_DISABLE, 0) };
    }

    /// The instructions retired in user space by the calling thread.
    pub fn user_instructions_retired() -> Self {
        make_linux_perf_event(PERF_COUNT_HW_INSTRUCTIONS, 0, -1, -1, 0)
    }

    /// The cpu cycles spent in user space by the calling thread.
    pub fn user_cpu_cycles_retired() -> Self {
        make_linux_perf_event(PERF_COUNT_HW_CPU_CYCLES, 0, -1, -1, 0)
    }
}

impl Drop for LinuxPerfEvent {
    fn drop(&mut self) {
        if self._fd != -1 {
            // SAFETY: the fd is ours.
            unsafe { close(self._fd) };
        }
    }
}

fn make_linux_perf_event(
    config: u64,
    pid: i32,
    cpu: c_int,
    group_fd: c_int,
    flags: c_ulong,
) -> LinuxPerfEvent {
    #[allow(unused_mut)]
    let mut attr_flags = ATTR_DISABLED | ATTR_EXCLUDE_KERNEL | ATTR_EXCLUDE_HV;
    // exclude_idle is not supported on all architectures (e.g. aarch64)
    // so enable it selectively only on architectures that support it.
    #[cfg(target_arch = "x86_64")]
    {
        attr_flags |= ATTR_EXCLUDE_IDLE;
    }
    let attr = PerfEventAttr {
        type_: PERF_TYPE_HARDWARE,
        size: size_of::<PerfEventAttr>() as u32,
        config,
        flags: attr_flags,
        ..PerfEventAttr::default()
    };
    LinuxPerfEvent::new(&attr, pid, cpu, group_fd, flags)
}
