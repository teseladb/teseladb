//! Linux asynchronous I/O (linux-aio) bindings used by the reactor backends.
//!
//! [`internal::linux_abi`] mirrors the kernel ABI (`aio_context_t`, `struct iocb`,
//! `struct io_event` and the completion ring header). The rest of [`internal`] builds control
//! blocks for reads, writes, polls and `fdsync`, and wraps the `io_setup`, `io_submit`,
//! `io_cancel`, `io_getevents` and `io_pgetevents` system calls. [`internal::io_getevents`]
//! reaps completions straight from the ring the kernel maps at the context address when it can,
//! without a system call; only one thread may reap from a given context.

/// Control-block helpers and system call wrappers.
pub mod internal {
    use std::sync::atomic::Ordering;

    use crate::engine::core::future::{ExceptionPtr, RuntimeError};
    use crate::engine::core::internal::io_desc::KernelCompletion;
    use crate::engine::core::posix::Timespec;
    use crate::engine::core::reactor::{SigsetT, aio_nowait_supported};
    use crate::engine::core::reactor_backend::{EAGAIN, ENOSYS, Iovec, errno, set_errno};

    /// The kernel ABI of linux-aio.
    pub mod linux_abi {
        /// A linux-aio context handle (`aio_context_t`): the address of its completion ring.
        pub type AioContextT = u64;

        /// The operation codes of a control block (`aio_lio_opcode`).
        #[repr(u16)]
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
        pub enum IocbCmd {
            /// Positioned read.
            #[default]
            Pread = 0,
            /// Positioned write.
            Pwrite = 1,
            /// `fsync` of the file.
            Fsync = 2,
            /// `fdatasync` of the file.
            Fdsync = 3,
            /// Wait for poll events on the descriptor.
            Poll = 5,
            /// No operation.
            Noop = 6,
            /// Positioned vectored read.
            Preadv = 7,
            /// Positioned vectored write.
            Pwritev = 8,
        }

        /// A completion event (`struct io_event`).
        #[repr(C)]
        #[derive(Clone, Copy, Debug, Default)]
        pub struct IoEvent {
            /// The `aio_data` of the completed control block.
            pub data: u64,
            /// The address of the completed control block.
            pub obj: u64,
            /// The operation's result: a byte count or a negative errno.
            pub res: i64,
            /// Secondary result (unused by the operations issued here).
            pub res2: i64,
        }

        /// Control-block flag: signal the eventfd in `aio_resfd` on completion.
        pub const IOCB_FLAG_RESFD: u32 = 1;

        /// A control block (`struct iocb`), with the little-endian order of `aio_key` and
        /// `aio_rw_flags`.
        #[cfg(target_endian = "little")]
        #[repr(C)]
        #[derive(Clone, Copy, Debug, Default)]
        pub struct Iocb {
            /// User data returned in [`IoEvent::data`].
            pub aio_data: u64,
            /// Kernel-internal key; zero on submission.
            pub aio_key: u32,
            /// Per-request `RWF_*` flags.
            pub aio_rw_flags: i32,
            /// The operation.
            pub aio_lio_opcode: IocbCmd,
            /// Request priority.
            pub aio_reqprio: i16,
            /// The file descriptor.
            pub aio_fildes: u32,
            /// The buffer (or iovec array, for vectored operations; poll events for polls).
            pub aio_buf: u64,
            /// The byte count (or iovec count, for vectored operations).
            pub aio_nbytes: u64,
            /// The file offset.
            pub aio_offset: i64,
            /// Reserved; zero.
            pub aio_reserved2: u64,
            /// `IOCB_FLAG_*` flags.
            pub aio_flags: u32,
            /// The eventfd signalled on completion when [`IOCB_FLAG_RESFD`] is set.
            pub aio_resfd: u32,
        }

        /// A control block (`struct iocb`), with the big-endian order of `aio_key` and
        /// `aio_rw_flags`.
        #[cfg(target_endian = "big")]
        #[repr(C)]
        #[derive(Clone, Copy, Debug, Default)]
        pub struct Iocb {
            /// User data returned in [`IoEvent::data`].
            pub aio_data: u64,
            /// Per-request `RWF_*` flags.
            pub aio_rw_flags: i32,
            /// Kernel-internal key; zero on submission.
            pub aio_key: u32,
            /// The operation.
            pub aio_lio_opcode: IocbCmd,
            /// Request priority.
            pub aio_reqprio: i16,
            /// The file descriptor.
            pub aio_fildes: u32,
            /// The buffer (or iovec array, for vectored operations; poll events for polls).
            pub aio_buf: u64,
            /// The byte count (or iovec count, for vectored operations).
            pub aio_nbytes: u64,
            /// The file offset.
            pub aio_offset: i64,
            /// Reserved; zero.
            pub aio_reserved2: u64,
            /// `IOCB_FLAG_*` flags.
            pub aio_flags: u32,
            /// The eventfd signalled on completion when [`IOCB_FLAG_RESFD`] is set.
            pub aio_resfd: u32,
        }

        /// The signal mask argument of `io_pgetevents` (`struct aio_sigset`).
        #[repr(C)]
        pub struct AioSigset {
            /// The signal mask to apply while waiting, or null.
            pub sigmask: *const crate::engine::core::reactor::SigsetT,
            /// The size of the kernel signal set, in bytes.
            pub sigsetsize: usize,
        }

        /// The header of the completion ring that the kernel maps at the address of the aio
        /// context; the [`IoEvent`] slots follow it.
        #[repr(C)]
        pub struct LinuxAioRing {
            /// Ring identifier.
            pub id: u32,
            /// Number of event slots.
            pub nr: u32,
            /// Consumer index, advanced by the reaper.
            pub head: std::sync::atomic::AtomicU32,
            /// Producer index, advanced by the kernel.
            pub tail: std::sync::atomic::AtomicU32,
            /// Magic value identifying a usable ring.
            pub magic: u32,
            /// Compatible feature flags.
            pub compat_features: u32,
            /// Incompatible feature flags; the ring is read in user space only when zero.
            pub incompat_features: u32,
            /// Size of this header in bytes.
            pub header_length: u32,
        }
    }

    use linux_abi::*;

    /// Per-request flag: fail with `EAGAIN` instead of blocking.
    const RWF_NOWAIT: i32 = 0x00000008;

    use crate::engine::core::internal::linux_abi::nr::{
        IO_CANCEL as SYS_IO_CANCEL, IO_DESTROY as SYS_IO_DESTROY, IO_GETEVENTS as SYS_IO_GETEVENTS,
        IO_SETUP as SYS_IO_SETUP, IO_SUBMIT as SYS_IO_SUBMIT,
    };
    /// The `io_pgetevents` system call number, defined on every supported architecture.
    const NR_IO_PGETEVENTS: Option<i64> =
        Some(crate::engine::core::internal::linux_abi::nr::IO_PGETEVENTS);

    unsafe extern "C" {
        fn syscall(num: i64, ...) -> i64;
        fn strerror_r(
            errnum: i32,
            buf: *mut std::ffi::c_char,
            buflen: usize,
        ) -> *mut std::ffi::c_char;
    }

    /// A control block that reads `len` bytes at `offset` of `fd` into `buffer`.
    #[inline]
    pub fn make_read_iocb(fd: i32, offset: u64, buffer: *mut std::ffi::c_void, len: usize) -> Iocb {
        Iocb {
            aio_lio_opcode: IocbCmd::Pread,
            aio_fildes: fd as u32,
            aio_offset: offset as i64,
            aio_buf: buffer as usize as u64,
            aio_nbytes: len as u64,
            ..Iocb::default()
        }
    }

    /// A control block that writes `len` bytes from `buffer` at `offset` of `fd`.
    #[inline]
    pub fn make_write_iocb(
        fd: i32,
        offset: u64,
        buffer: *const std::ffi::c_void,
        len: usize,
    ) -> Iocb {
        Iocb {
            aio_lio_opcode: IocbCmd::Pwrite,
            aio_fildes: fd as u32,
            aio_offset: offset as i64,
            aio_buf: buffer as usize as u64,
            aio_nbytes: len as u64,
            ..Iocb::default()
        }
    }

    /// A control block that reads at `offset` of `fd` into the `niov` buffers of `iov`.
    #[inline]
    pub fn make_readv_iocb(fd: i32, offset: u64, iov: *const Iovec, niov: usize) -> Iocb {
        Iocb {
            aio_lio_opcode: IocbCmd::Preadv,
            aio_fildes: fd as u32,
            aio_offset: offset as i64,
            aio_buf: iov as usize as u64,
            aio_nbytes: niov as u64,
            ..Iocb::default()
        }
    }

    /// A control block that writes the `niov` buffers of `iov` at `offset` of `fd`.
    #[inline]
    pub fn make_writev_iocb(fd: i32, offset: u64, iov: *const Iovec, niov: usize) -> Iocb {
        Iocb {
            aio_lio_opcode: IocbCmd::Pwritev,
            aio_fildes: fd as u32,
            aio_offset: offset as i64,
            aio_buf: iov as usize as u64,
            aio_nbytes: niov as u64,
            ..Iocb::default()
        }
    }

    /// A control block that waits for the poll `events` on `fd`.
    #[inline]
    pub fn make_poll_iocb(fd: i32, events: i32) -> Iocb {
        Iocb {
            aio_lio_opcode: IocbCmd::Poll,
            aio_fildes: fd as u32,
            aio_buf: events as u32 as u64,
            ..Iocb::default()
        }
    }

    /// A control block that runs `fdatasync` on `fd`.
    #[inline]
    pub fn make_fdsync_iocb(fd: i32) -> Iocb {
        Iocb { aio_lio_opcode: IocbCmd::Fdsync, aio_fildes: fd as u32, ..Iocb::default() }
    }

    /// Stores the completion `data` that the event of this control block will carry.
    #[inline]
    pub fn set_user_data(iocb: &mut Iocb, data: *mut KernelCompletion) {
        iocb.aio_data = data as usize as u64;
    }

    /// The completion stored in a control block by [`set_user_data`].
    #[inline]
    pub fn get_user_data_iocb(iocb: &Iocb) -> *mut KernelCompletion {
        iocb.aio_data as usize as *mut KernelCompletion
    }

    /// The completion carried by a completion event.
    #[inline]
    pub fn get_user_data(ev: &IoEvent) -> *mut KernelCompletion {
        ev.data as usize as *mut KernelCompletion
    }

    /// Makes the kernel signal `eventfd` when this control block completes.
    #[inline]
    pub fn set_eventfd_notification(iocb: &mut Iocb, eventfd: i32) {
        iocb.aio_flags |= IOCB_FLAG_RESFD;
        iocb.aio_resfd = eventfd as u32;
    }

    /// The control block a completion event refers to.
    #[inline]
    pub fn get_iocb(ev: &IoEvent) -> *mut Iocb {
        ev.obj as usize as *mut Iocb
    }

    /// Sets or clears `RWF_NOWAIT` on the control block; a no-op when the kernel does not
    /// support it.
    #[inline]
    pub fn set_nowait(iocb: &mut Iocb, nowait: bool) {
        if aio_nowait_supported() {
            if nowait {
                iocb.aio_rw_flags |= RWF_NOWAIT;
            } else {
                iocb.aio_rw_flags &= !RWF_NOWAIT;
            }
        }
    }

    #[inline]
    fn to_ring(io_context: AioContextT) -> *const LinuxAioRing {
        io_context as usize as *const LinuxAioRing
    }

    /// Whether the kernel's completion ring can be read in user space (known magic value, no
    /// incompatible features).
    #[inline]
    fn usable(ring: &LinuxAioRing) -> bool {
        ring.magic == 0xa10a10a1 && ring.incompat_features == 0
    }

    /// The `io_setup` system call: creates a context for `nr_events` events in `io_context`.
    /// Returns 0 or -1 with `errno` set.
    pub fn io_setup(nr_events: i32, io_context: &mut AioContextT) -> i32 {
        // SAFETY: `io_context` is a valid out-pointer.
        unsafe { syscall(SYS_IO_SETUP, nr_events, io_context as *mut AioContextT) as i32 }
    }

    /// The `io_destroy` system call. Returns 0 or -1 with `errno` set.
    pub fn io_destroy(io_context: AioContextT) -> i32 {
        // SAFETY: plain system call.
        unsafe { syscall(SYS_IO_DESTROY, io_context) as i32 }
    }

    /// The `io_submit` system call: submits `nr` control blocks. Returns the number submitted
    /// or -1 with `errno` set.
    ///
    /// # Safety
    ///
    /// `iocbs` must point to `nr` pointers to prepared iocbs that stay alive until completion.
    #[inline]
    pub unsafe fn io_submit(io_context: AioContextT, nr: i64, iocbs: *mut *mut Iocb) -> i32 {
        // SAFETY: forwarded from the caller.
        unsafe { syscall(SYS_IO_SUBMIT, io_context, nr, iocbs) as i32 }
    }

    /// The `io_cancel` system call: cancels a submitted control block.
    ///
    /// # Safety
    ///
    /// `iocb` must be a submitted iocb and `result` a valid out-pointer.
    pub unsafe fn io_cancel(io_context: AioContextT, iocb: *mut Iocb, result: *mut IoEvent) -> i32 {
        // SAFETY: forwarded from the caller.
        unsafe { syscall(SYS_IO_CANCEL, io_context, iocb, result) as i32 }
    }

    /// Reaps up to `nr` events from the user-space completion ring, if at least `min_nr` are
    /// available or `timeout` is zero; returns -1 when the system call is needed instead.
    ///
    /// # Safety
    ///
    /// `io_context` must be a live context from `io_setup`, `events` must hold `nr` writable
    /// slots and `timeout` be null or valid.
    #[inline]
    unsafe fn try_reap_events(
        io_context: AioContextT,
        min_nr: i64,
        nr: i64,
        events: *mut IoEvent,
        timeout: *const Timespec,
        force_syscall: bool,
    ) -> i32 {
        let ring_p = to_ring(io_context);
        // SAFETY: the kernel maps the ring header at the address of a live aio context (by
        // the caller).
        let ring = unsafe { &*ring_p };
        if usable(ring) && !force_syscall {
            // Try to complete in userspace, if enough available events,
            // or if the timeout is zero
            // We're the only writer to ->head, so we can load with memory_order_relaxed (assuming
            // only a single thread calls io_getevents()).
            let mut head = ring.head.load(Ordering::Relaxed);
            // The kernel will write to the ring from an interrupt and then release with a write
            // to ring->tail, so we must memory_order_acquire here.
            let tail = ring.tail.load(Ordering::Acquire); // kernel writes from interrupts
            let mut available = tail.wrapping_sub(head);
            if tail < head {
                available = available.wrapping_add(ring.nr);
            }
            // SAFETY: `timeout` is null or valid (by the caller).
            let zero_timeout =
                !timeout.is_null() && unsafe { (*timeout).tv_sec == 0 && (*timeout).tv_nsec == 0 };
            if available >= min_nr as u32 || zero_timeout {
                if available == 0 {
                    return 0;
                }
                let ring_events =
                    (io_context as usize + ring.header_length as usize) as *const IoEvent;
                let now = (nr as u32).min(available);
                // SAFETY: the ring holds `ring.nr` events after its header; `events` holds at
                // least `nr` slots (by the caller); the copied ranges stay within both.
                unsafe {
                    let start = ring_events.add(head as usize);
                    head += now;
                    if head < ring.nr {
                        std::ptr::copy_nonoverlapping(start, events, now as usize);
                    } else {
                        head -= ring.nr;
                        let first = (ring.nr as usize) - (start.offset_from(ring_events) as usize);
                        std::ptr::copy_nonoverlapping(start, events, first);
                        std::ptr::copy_nonoverlapping(
                            ring_events,
                            events.add(first),
                            head as usize,
                        );
                    }
                }
                // The kernel will read ring->head and update its view of how many entries
                // in the ring are available, so memory_order_release to make sure any ring
                // accesses are completed before the update to ring->head is visible.
                ring.head.store(head, Ordering::Release);
                return now as i32;
            }
        }
        -1
    }

    /// Reaps between `min_nr` and `nr` completion events into `events`, waiting at most
    /// `timeout` (null waits forever). Reads the user-space ring directly when it can, unless
    /// `force_syscall`; otherwise issues the `io_getevents` system call. Returns the number of
    /// events or -1 with `errno` set.
    ///
    /// # Safety
    ///
    /// `io_context` must be a live context from `io_setup`, `events` must hold `nr` writable
    /// slots and `timeout` be null or valid.
    #[inline]
    pub unsafe fn io_getevents(
        io_context: AioContextT,
        min_nr: i64,
        nr: i64,
        events: *mut IoEvent,
        timeout: *const Timespec,
        force_syscall: bool,
    ) -> i32 {
        // SAFETY: forwarded from the caller.
        let r = unsafe { try_reap_events(io_context, min_nr, nr, events, timeout, force_syscall) };
        if r >= 0 {
            return r;
        }
        // SAFETY: `events` holds `nr` slots; `timeout` is null or valid (by the caller).
        unsafe { syscall(SYS_IO_GETEVENTS, io_context, min_nr, nr, events, timeout) as i32 }
    }

    /// Like [`io_getevents`], applying `sigmask` while waiting (the `io_pgetevents` system
    /// call), without forcing the system call.
    ///
    /// # Safety
    ///
    /// As [`io_pgetevents_forced`].
    #[inline]
    pub unsafe fn io_pgetevents(
        io_context: AioContextT,
        min_nr: i64,
        nr: i64,
        events: *mut IoEvent,
        timeout: *const Timespec,
        sigmask: Option<&SigsetT>,
    ) -> i32 {
        // SAFETY: forwarded from the caller.
        unsafe { io_pgetevents_forced(io_context, min_nr, nr, events, timeout, sigmask, false) }
    }

    /// Like [`io_pgetevents`], with an explicit `force_syscall`. Returns -1 with `errno` set to
    /// `ENOSYS` when the kernel lacks `io_pgetevents`.
    ///
    /// # Safety
    ///
    /// `io_context` must be a live context from `io_setup`, `events` must hold `nr` writable
    /// slots and `timeout` be null or valid.
    pub unsafe fn io_pgetevents_forced(
        io_context: AioContextT,
        min_nr: i64,
        nr: i64,
        events: *mut IoEvent,
        timeout: *const Timespec,
        sigmask: Option<&SigsetT>,
        force_syscall: bool,
    ) -> i32 {
        let Some(nr_io_pgetevents) = NR_IO_PGETEVENTS else {
            set_errno(ENOSYS);
            return -1;
        };
        // SAFETY: forwarded from the caller.
        let r = unsafe { try_reap_events(io_context, min_nr, nr, events, timeout, force_syscall) };
        if r >= 0 {
            return r;
        }
        let as_ = AioSigset {
            sigmask: sigmask.map_or(std::ptr::null(), |m| m as *const SigsetT),
            sigsetsize: 8, // Can't use sizeof(*sigmask) because user and kernel sigset_t are inconsistent
        };
        // SAFETY: `events` holds `nr` slots; `timeout` is null or valid (by the caller); `as_`
        // lives across the call.
        unsafe {
            syscall(
                nr_io_pgetevents,
                io_context,
                min_nr,
                nr,
                events,
                timeout,
                &as_ as *const AioSigset,
            ) as i32
        }
    }

    /// Creates an aio context for `nr` events.
    ///
    /// # Errors
    ///
    /// Returns a runtime error with the kernel's message when `io_setup` fails; on `EAGAIN`
    /// the message says how to raise `/proc/sys/fs/aio-max-nr`.
    pub fn setup_aio_context(nr: u32, io_context: &mut AioContextT) -> Result<(), ExceptionPtr> {
        let r = io_setup(nr as i32, io_context);
        if r < 0 {
            let err = errno();
            let mut buf = [0 as std::ffi::c_char; 1024];
            // SAFETY: `buf` is a valid buffer; the GNU strerror_r returns a NUL-terminated string.
            let msg =
                unsafe { std::ffi::CStr::from_ptr(strerror_r(err, buf.as_mut_ptr(), buf.len())) }
                    .to_string_lossy()
                    .into_owned();
            if err == EAGAIN {
                let aio_max_nr = crate::engine::util::read_first_line::read_first_line_as::<u32>(
                    std::path::Path::new("/proc/sys/fs/aio-max-nr"),
                )?;
                return Err(ExceptionPtr::new(RuntimeError::new(format!(
                    "Could not setup Async I/O: {}. The required nr_events {} exceeds the capacity in /proc/sys/fs/aio-max-nr {}. Set /proc/sys/fs/aio-max-nr to at least {}.",
                    msg, nr, aio_max_nr, nr
                ))));
            } else {
                return Err(ExceptionPtr::new(RuntimeError::new(format!(
                    "Could not setup Async I/O: {}",
                    msg
                ))));
            }
        }
        Ok(())
    }

    const _: () = assert!(std::mem::size_of::<Iocb>() == 64);
    const _: () = assert!(std::mem::size_of::<IoEvent>() == 32);
}

/// Whether the kernel supports `RWF_NOWAIT` for linux-aio (probed, or set by the
/// `linux-aio-nowait` option).
pub use crate::engine::core::reactor::aio_nowait_supported;
