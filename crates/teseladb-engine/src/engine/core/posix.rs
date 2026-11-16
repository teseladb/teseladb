//! Thin, mostly internal wrappers over the POSIX and Linux system interfaces.
//!
//! [`FileDesc`] owns a file descriptor and exposes the system calls the runtime uses on it
//! (sockets, eventfd, epoll, timerfd, inotify, `mmap`, `ioctl`...). [`PosixThread`] is a
//! pthread with its own huge-page backed stack, used for the reactor and helper threads. The
//! free functions convert failed calls into errors and build signal masks and CPU sets.
//!
//! Calls that fail return `Err(ExceptionPtr)` carrying a system error with `errno`. Calls on
//! non-blocking descriptors return `Ok(None)` when they would block (`EAGAIN`). None of these
//! functions involve the reactor: an application may use them directly as long as they do not
//! block.

use std::collections::BTreeSet;
use std::ffi::{c_char, c_void};
use std::time::Duration;

use crate::engine::core::future::{ExceptionPtr, SystemError};
use crate::engine::core::reactor::{
    SigsetT, is_abort_on_ebadf_enabled, pthread_self, sigaddset, sigemptyset, sigfillset,
};
use crate::engine::core::reactor_backend::{
    EAGAIN, EBADF, EINPROGRESS, Iovec, Msghdr, engine_assert, errno,
};
use crate::engine::core::sstring::Sstring;
use crate::engine::net::api::SocketAddress;

/// Kernel structure layouts and flags shared with the ABI module.
pub use crate::engine::core::internal::linux_abi::{O_CLOEXEC, O_RDONLY, Stat, Timespec};

/// A timer specification for `timerfd_settime` (`struct itimerspec`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Itimerspec {
    /// Period for re-arming the timer; zero for a one-shot timer.
    pub it_interval: Timespec,
    /// First expiration; zero disarms the timer.
    pub it_value: Timespec,
}

/// File system statistics returned by `statfs(2)` (`struct statfs`).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Statfs {
    /// File system type (magic number).
    pub f_type: i64,
    /// Optimal transfer block size.
    pub f_bsize: i64,
    /// Total data blocks.
    pub f_blocks: u64,
    /// Free blocks.
    pub f_bfree: u64,
    /// Free blocks available to unprivileged users.
    pub f_bavail: u64,
    /// Total inodes.
    pub f_files: u64,
    /// Free inodes.
    pub f_ffree: u64,
    /// File system id.
    pub f_fsid: [i32; 2],
    /// Maximum file name length.
    pub f_namelen: i64,
    /// Fragment size.
    pub f_frsize: i64,
    /// Mount flags.
    pub f_flags: i64,
    /// Padding.
    pub f_spare: [i64; 4],
}

/// File system statistics returned by `statvfs(3)` (`struct statvfs`).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Statvfs {
    /// File system block size.
    pub f_bsize: u64,
    /// Fragment size.
    pub f_frsize: u64,
    /// Size of the file system in `f_frsize` units.
    pub f_blocks: u64,
    /// Free blocks.
    pub f_bfree: u64,
    /// Free blocks available to unprivileged users.
    pub f_bavail: u64,
    /// Total inodes.
    pub f_files: u64,
    /// Free inodes.
    pub f_ffree: u64,
    /// Free inodes available to unprivileged users.
    pub f_favail: u64,
    /// File system id.
    pub f_fsid: u64,
    /// Mount flags.
    pub f_flag: u64,
    /// Maximum file name length.
    pub f_namemax: u64,
    /// Padding.
    pub __f_spare: [i32; 6],
}

/// A CPU affinity set (`cpu_set_t`), for up to 1024 CPUs.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CpuSetT {
    /// One bit per CPU.
    pub bits: [u64; 16],
}

impl CpuSetT {
    /// Number of CPUs the set can hold.
    pub const SETSIZE: usize = 1024;
    /// Adds `cpu` to the set.
    #[inline]
    pub fn set(&mut self, cpu: usize) {
        if cpu < Self::SETSIZE {
            self.bits[cpu / 64] |= 1u64 << (cpu % 64);
        }
    }
    /// Whether `cpu` is in the set.
    #[inline]
    pub fn is_set(&self, cpu: usize) -> bool {
        cpu < Self::SETSIZE && self.bits[cpu / 64] & (1u64 << (cpu % 64)) != 0
    }
    /// Number of CPUs in the set.
    #[inline]
    pub fn count(&self) -> u32 {
        self.bits.iter().map(|w| w.count_ones()).sum()
    }
}

const ENOTSOCK: i32 = 88;
const ENOTCONN: i32 = 107;
#[cfg(not(engine_asan_enabled))]
const PROT_NONE: i32 = 0;
const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_SHARED: i32 = 1;
const MAP_PRIVATE: i32 = 2;
#[cfg(not(engine_asan_enabled))]
const MAP_FIXED: i32 = 0x10;
const MAP_ANONYMOUS: i32 = 0x20;
#[cfg(not(engine_asan_enabled))]
const MAP_NORESERVE: i32 = 0x4000;
#[cfg(not(engine_asan_enabled))]
const MAP_STACK: i32 = 0x20000;
const MAP_FAILED: *mut c_void = !0usize as *mut c_void;
#[cfg(not(engine_asan_enabled))]
const MADV_HUGEPAGE: i32 = 14;

use crate::engine::core::internal::linux_abi::PthreadAttrT;

unsafe extern "C" {
    fn close(fd: i32) -> i32;
    #[link_name = "open"]
    fn libc_open(path: *const c_char, flags: i32, ...) -> i32;
    #[link_name = "socket"]
    fn libc_socket(domain: i32, type_: i32, protocol: i32) -> i32;
    #[link_name = "eventfd"]
    fn libc_eventfd(initval: u32, flags: i32) -> i32;
    fn epoll_create1(flags: i32) -> i32;
    #[link_name = "timerfd_create"]
    fn libc_timerfd_create(clockid: i32, flags: i32) -> i32;
    #[link_name = "timerfd_settime"]
    fn libc_timerfd_settime(
        fd: i32,
        flags: i32,
        new_value: *const Itimerspec,
        old: *mut Itimerspec,
    ) -> i32;
    #[link_name = "dup"]
    fn libc_dup(fd: i32) -> i32;
    fn accept4(fd: i32, addr: *mut c_void, len: *mut u32, flags: i32) -> i32;
    #[link_name = "shutdown"]
    fn libc_shutdown(fd: i32, how: i32) -> i32;
    fn ftruncate(fd: i32, len: i64) -> i32;
    #[link_name = "ioctl"]
    fn libc_ioctl(fd: i32, req: u64, ...) -> i32;
    #[link_name = "setsockopt"]
    fn libc_setsockopt(fd: i32, level: i32, name: i32, val: *const c_void, len: u32) -> i32;
    #[link_name = "getsockopt"]
    fn libc_getsockopt(fd: i32, level: i32, name: i32, val: *mut c_void, len: *mut u32) -> i32;
    fn fstat(fd: i32, buf: *mut Stat) -> i32;
    #[link_name = "read"]
    fn libc_read(fd: i32, buf: *mut c_void, n: usize) -> isize;
    #[link_name = "recv"]
    fn libc_recv(fd: i32, buf: *mut c_void, n: usize, flags: i32) -> isize;
    #[link_name = "recvmsg"]
    fn libc_recvmsg(fd: i32, mh: *mut Msghdr, flags: i32) -> isize;
    #[link_name = "send"]
    fn libc_send(fd: i32, buf: *const c_void, n: usize, flags: i32) -> isize;
    #[link_name = "sendto"]
    fn libc_sendto(
        fd: i32,
        buf: *const c_void,
        n: usize,
        flags: i32,
        addr: *const c_void,
        len: u32,
    ) -> isize;
    #[link_name = "sendmsg"]
    fn libc_sendmsg(fd: i32, mh: *const Msghdr, flags: i32) -> isize;
    #[link_name = "bind"]
    fn libc_bind(fd: i32, addr: *const c_void, len: u32) -> i32;
    #[link_name = "connect"]
    fn libc_connect(fd: i32, addr: *const c_void, len: u32) -> i32;
    fn getsockname(fd: i32, addr: *mut c_void, len: *mut u32) -> i32;
    fn getpeername(fd: i32, addr: *mut c_void, len: *mut u32) -> i32;
    #[link_name = "listen"]
    fn libc_listen(fd: i32, backlog: i32) -> i32;
    #[link_name = "write"]
    fn libc_write(fd: i32, buf: *const c_void, n: usize) -> isize;
    #[link_name = "writev"]
    fn libc_writev(fd: i32, iov: *const Iovec, n: i32) -> isize;
    #[link_name = "pread"]
    fn libc_pread(fd: i32, buf: *mut c_void, n: usize, off: i64) -> isize;
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64)
    -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> i32;
    #[cfg(not(engine_asan_enabled))]
    fn madvise(addr: *mut c_void, len: usize, advice: i32) -> i32;
    fn posix_spawn_file_actions_addclose(a: *mut c_void, fd: i32) -> i32;
    fn posix_spawn_file_actions_adddup2(a: *mut c_void, fd: i32, newfd: i32) -> i32;
    fn mkstemp(templat: *mut c_char) -> i32;
    fn unlink(path: *const c_char) -> i32;
    fn inotify_init1(flags: i32) -> i32;
    fn readlink(path: *const c_char, buf: *mut c_char, size: usize) -> isize;
    fn pthread_attr_init(a: *mut PthreadAttrT) -> i32;
    #[cfg(not(engine_asan_enabled))]
    fn pthread_attr_setstack(a: *mut PthreadAttrT, stack: *mut c_void, size: usize) -> i32;
    fn pthread_attr_setaffinity_np(a: *mut PthreadAttrT, size: usize, cs: *const CpuSetT) -> i32;
    fn pthread_create(
        t: *mut usize,
        a: *const PthreadAttrT,
        f: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> i32;
    fn pthread_join(t: usize, ret: *mut *mut c_void) -> i32;
    fn pthread_setaffinity_np(t: usize, size: usize, cs: *const CpuSetT) -> i32;
    fn pthread_getaffinity_np(t: usize, size: usize, cs: *mut CpuSetT) -> i32;
}

/// Unmaps an `mmap` area of a known size.
#[derive(Clone, Copy, Debug)]
pub struct MmapDeleter {
    /// Length of the mapping in bytes.
    pub _size: usize,
}

impl MmapDeleter {
    /// Unmaps `ptr`.
    ///
    /// # Safety
    /// `ptr` is a mapping of `_size` bytes that nothing uses any more.
    #[inline]
    pub unsafe fn call(&self, ptr: *mut c_void) {
        // SAFETY: guaranteed by the caller.
        unsafe { munmap(ptr, self._size) };
    }
}

/// An owned memory mapping, unmapped on drop.
pub struct MmapArea {
    p: *mut u8,
    d: MmapDeleter,
}

impl MmapArea {
    /// The empty area.
    pub const fn null() -> Self {
        MmapArea { p: std::ptr::null_mut(), d: MmapDeleter { _size: 0 } }
    }
    /// The start address of the mapping (null for an empty area).
    #[inline]
    pub fn get(&self) -> *mut u8 {
        self.p
    }
    /// Gives up ownership: returns the start address and leaves the mapping in place.
    #[inline]
    pub fn release(mut self) -> *mut u8 {
        std::mem::replace(&mut self.p, std::ptr::null_mut())
    }
}

impl Drop for MmapArea {
    fn drop(&mut self) {
        if !self.p.is_null() {
            // SAFETY: the area owns the mapping, of the deleter's size.
            unsafe { self.d.call(self.p.cast()) };
        }
    }
}

/// An owned file descriptor, closed on drop.
#[derive(Debug)]
pub struct FileDesc {
    _fd: i32,
}

impl Drop for FileDesc {
    #[inline]
    fn drop(&mut self) {
        if self._fd != -1 {
            // SAFETY: closing the descriptor we own.
            unsafe { close(self._fd) };
        }
    }
}

impl FileDesc {
    /// Takes ownership of `fd`.
    #[inline]
    const fn new(fd: i32) -> Self {
        FileDesc { _fd: fd }
    }

    /// Replaces the owned descriptor with `x`'s, closing the previous one.
    ///
    /// # Errors
    ///
    /// Returns the error of `close` on the previous descriptor.
    pub fn assign(&mut self, mut x: FileDesc) -> Result<(), ExceptionPtr> {
        std::mem::swap(&mut self._fd, &mut x._fd);
        if x._fd != -1 {
            x.close()?;
        }
        Ok(())
    }

    /// Closes the descriptor now.
    ///
    /// # Errors
    ///
    /// Returns the error of `close(2)`.
    ///
    /// Aborts the process if the descriptor was already closed.
    pub fn close(&mut self) -> Result<(), ExceptionPtr> {
        engine_assert!(self._fd != -1);
        // SAFETY: closing the descriptor we own.
        let r = unsafe { close(self._fd) };
        throw_system_error_on(r == -1, "close")?;
        self._fd = -1;
        Ok(())
    }

    /// The raw descriptor (`-1` once closed).
    #[inline]
    pub fn get(&self) -> i32 {
        self._fd
    }

    /// Takes ownership of an existing descriptor.
    #[inline]
    pub fn from_fd(fd: i32) -> FileDesc {
        FileDesc::new(fd)
    }

    /// Opens `name` with `open(2)` `flags` and `mode`.
    ///
    /// # Errors
    ///
    /// Returns the system error of `open`.
    pub fn open(name: &str, flags: i32, mode: u32) -> Result<FileDesc, ExceptionPtr> {
        let c = std::ffi::CString::new(name).unwrap_or_default();
        // SAFETY: NUL-terminated path.
        let fd = unsafe { libc_open(c.as_ptr(), flags, mode) };
        throw_system_error_on(fd == -1, "open")?;
        Ok(FileDesc::new(fd))
    }

    /// Creates a socket (`socket(2)`).
    ///
    /// # Errors
    ///
    /// Returns the system error of `socket`.
    pub fn socket(family: i32, type_: i32, protocol: i32) -> Result<FileDesc, ExceptionPtr> {
        // SAFETY: plain system call.
        let fd = unsafe { libc_socket(family, type_, protocol) };
        throw_system_error_on(fd == -1, "socket")?;
        Ok(FileDesc::new(fd))
    }

    /// Creates an eventfd with initial value `initval`.
    ///
    /// # Errors
    ///
    /// Returns the system error of `eventfd`.
    pub fn eventfd(initval: u32, flags: i32) -> Result<FileDesc, ExceptionPtr> {
        // SAFETY: plain system call.
        let fd = unsafe { libc_eventfd(initval, flags) };
        throw_system_error_on(fd == -1, "eventfd")?;
        Ok(FileDesc::new(fd))
    }

    /// Creates an epoll instance (`epoll_create1`).
    ///
    /// # Errors
    ///
    /// Returns the system error of `epoll_create1`.
    pub fn epoll_create(flags: i32) -> Result<FileDesc, ExceptionPtr> {
        // SAFETY: plain system call.
        let fd = unsafe { epoll_create1(flags) };
        throw_system_error_on(fd == -1, "epoll_create1")?;
        Ok(FileDesc::new(fd))
    }

    /// Creates a timerfd on clock `clockid`.
    ///
    /// # Errors
    ///
    /// Returns the system error of `timerfd_create`.
    pub fn timerfd_create(clockid: i32, flags: i32) -> Result<FileDesc, ExceptionPtr> {
        // SAFETY: plain system call.
        let fd = unsafe { libc_timerfd_create(clockid, flags) };
        throw_system_error_on(fd == -1, "timerfd_create")?;
        Ok(FileDesc::new(fd))
    }

    /// Creates an anonymous temporary file in `directory`: the file is created with `mkstemp`
    /// and unlinked at once, so it disappears when the descriptor is closed.
    ///
    /// # Errors
    ///
    /// Returns the system error of `mkstemp` or `unlink` (in the latter case the file stays
    /// on disk).
    pub fn temporary(mut directory: Sstring) -> Result<FileDesc, ExceptionPtr> {
        // FIXME: add O_TMPFILE support one day
        directory.push_str("/XXXXXX");
        let mut templat: Vec<c_char> = directory.as_str().bytes().map(|b| b as c_char).collect();
        templat.push(0);
        // SAFETY: `templat` is a NUL-terminated, writable template.
        let fd = unsafe { mkstemp(templat.as_mut_ptr()) };
        throw_system_error_on(fd == -1, "")?;
        // SAFETY: as above.
        let r = unsafe { unlink(templat.as_ptr()) };
        throw_system_error_on(r == -1, "")?; // leaks created file, but what can we do?
        Ok(FileDesc::new(fd))
    }

    /// Duplicates the descriptor (`dup(2)`).
    ///
    /// # Errors
    ///
    /// Returns the system error of `dup`.
    pub fn dup(&self) -> Result<FileDesc, ExceptionPtr> {
        // SAFETY: plain system call.
        let fd = unsafe { libc_dup(self.get()) };
        throw_system_error_on(fd == -1, "dup")?;
        Ok(FileDesc::new(fd))
    }

    /// Accepts a connection, storing the peer address in `sa`.
    ///
    /// # Errors
    ///
    /// Returns the system error of `accept4`, including `EAGAIN` (see
    /// [`try_accept`](Self::try_accept)).
    pub fn accept(&self, sa: &mut SocketAddress, flags: i32) -> Result<FileDesc, ExceptionPtr> {
        // SAFETY: `sa` provides storage and its length.
        let ret = unsafe {
            accept4(
                self._fd,
                sa.as_posix_sockaddr_mut() as *mut _ as *mut c_void,
                &mut sa.addr_length,
                flags,
            )
        };
        throw_system_error_on(ret == -1, "accept4")?;
        Ok(FileDesc::new(ret))
    }

    /// Creates an inotify instance (`inotify_init1`).
    ///
    /// # Errors
    ///
    /// Returns the system error of `inotify_init1`.
    pub fn inotify_init(flags: i32) -> Result<FileDesc, ExceptionPtr> {
        // SAFETY: plain system call.
        let fd = unsafe { inotify_init1(flags) };
        throw_system_error_on(fd == -1, "could not create inotify instance")?;
        Ok(FileDesc::new(fd))
    }

    /// Accepts a connection, storing the peer address in `sa`; returns `Ok(None)` if no
    /// connection is available to be accepted.
    ///
    /// # Errors
    ///
    /// Returns any other system error of `accept4`.
    pub fn try_accept(
        &self,
        sa: &mut SocketAddress,
        flags: i32,
    ) -> Result<Option<FileDesc>, ExceptionPtr> {
        // SAFETY: as in accept().
        let ret = unsafe {
            accept4(
                self._fd,
                sa.as_posix_sockaddr_mut() as *mut _ as *mut c_void,
                &mut sa.addr_length,
                flags,
            )
        };
        if ret == -1 && errno() == EAGAIN {
            return Ok(None);
        }
        throw_system_error_on(ret == -1, "accept4")?;
        Ok(Some(FileDesc::new(ret)))
    }

    /// Shuts down part of a full-duplex connection (`SHUT_RD`, `SHUT_WR` or `SHUT_RDWR`). A
    /// socket that is not connected is not an error.
    ///
    /// # Errors
    ///
    /// Returns any other system error of `shutdown`.
    pub fn shutdown(&self, how: i32) -> Result<(), ExceptionPtr> {
        // SAFETY: plain system call.
        let ret = unsafe { libc_shutdown(self._fd, how) };
        if ret == -1 && errno() != ENOTCONN {
            throw_system_error_on(ret == -1, "shutdown")?;
        }
        Ok(())
    }

    /// Truncates or extends the file to `size` bytes.
    ///
    /// # Errors
    ///
    /// Returns the system error of `ftruncate`.
    pub fn truncate(&self, size: usize) -> Result<(), ExceptionPtr> {
        // SAFETY: plain system call.
        let ret = unsafe { ftruncate(self._fd, size as i64) };
        throw_system_error_on(ret != 0, "ftruncate")
    }

    /// Issues `ioctl` `request` with the address of `data`, which the kernel reads and writes.
    ///
    /// # Errors
    ///
    /// Returns the system error of `ioctl`.
    ///
    /// # Safety
    /// `request` reads and writes at most a `X` at `data` (the kernel's meaning of the request).
    pub unsafe fn ioctl<X>(&self, request: u64, data: &mut X) -> Result<i32, ExceptionPtr> {
        // SAFETY: `data` is a live, exclusively borrowed `X` for the whole call; the request
        // determines how the kernel reads and writes it.
        let r = unsafe { libc_ioctl(self._fd, request, data as *mut X as usize) };
        throw_system_error_on(r == -1, "ioctl")?;
        Ok(r)
    }

    /// Issues `ioctl` `request` with an integer argument `value`.
    ///
    /// # Errors
    ///
    /// Returns the system error of `ioctl`.
    pub fn ioctl_int(&self, request: u64, value: i32) -> Result<i32, ExceptionPtr> {
        // SAFETY: plain ioctl with an integer argument.
        let r = unsafe { libc_ioctl(self._fd, request, value) };
        throw_system_error_on(r == -1, "ioctl")?;
        Ok(r)
    }

    /// Issues `ioctl` `request` with the address of `data`, like [`ioctl`](Self::ioctl).
    /// Takes `&self`: the call does not change the descriptor.
    ///
    /// # Errors
    ///
    /// Returns the system error of `ioctl`.
    ///
    /// # Safety
    /// `request` must make the kernel access no more than the `size_of::<X>()` bytes of
    /// `data`, and any bytes it writes must leave a valid `X` (plain data).
    pub unsafe fn ioctl_data<X>(&self, request: u64, data: &mut X) -> Result<i32, ExceptionPtr> {
        // SAFETY: guaranteed by the caller.
        let r = unsafe { libc_ioctl(self._fd, request, data as *mut X) };
        throw_system_error_on(r == -1, "ioctl")?;
        Ok(r)
    }

    /// Sets an integer socket option.
    ///
    /// # Errors
    ///
    /// Returns the system error of `setsockopt`.
    pub fn setsockopt_int(&self, level: i32, optname: i32, data: i32) -> Result<i32, ExceptionPtr> {
        // SAFETY: `data` is a live int.
        let r = unsafe {
            libc_setsockopt(self._fd, level, optname, &data as *const i32 as *const c_void, 4)
        };
        throw_system_error_on(r == -1, "setsockopt")?;
        Ok(r)
    }

    /// Sets a socket option to the bytes of `data`. Takes `&self`: the call does not change
    /// the descriptor.
    ///
    /// # Errors
    ///
    /// Returns the system error of `setsockopt`.
    pub fn setsockopt<X: Copy>(
        &self,
        level: i32,
        optname: i32,
        data: X,
    ) -> Result<i32, ExceptionPtr> {
        // SAFETY: `data` is a live value of `size_of::<X>()` bytes for the call.
        let r = unsafe {
            libc_setsockopt(
                self._fd,
                level,
                optname,
                &data as *const X as *const c_void,
                size_of::<X>() as u32,
            )
        };
        throw_system_error_on(r == -1, "setsockopt")?;
        Ok(r)
    }

    /// Reads a socket option into a zeroed `Data` that the kernel fills. Takes `&self`: the
    /// call does not change the descriptor.
    ///
    /// # Errors
    ///
    /// Returns the system error of `getsockopt`.
    ///
    /// # Safety
    /// `Data` must be plain data for which every bit pattern is a valid value (the kernel
    /// writes its bytes).
    pub unsafe fn getsockopt<Data: Copy>(
        &self,
        level: i32,
        optname: i32,
    ) -> Result<Data, ExceptionPtr> {
        // SAFETY: guaranteed by the caller: all-zero bytes are a valid `Data`.
        let mut data: Data = unsafe { std::mem::zeroed() };
        let mut len = size_of::<Data>() as u32;
        // SAFETY: `data` is a live `Data` of `len` writable bytes.
        let r = unsafe {
            libc_getsockopt(
                self._fd,
                level,
                optname,
                &mut data as *mut Data as *mut c_void,
                &mut len,
            )
        };
        throw_system_error_on(r == -1, "getsockopt")?;
        Ok(data)
    }

    /// Sets a string socket option (for example `SO_BINDTODEVICE`).
    ///
    /// # Errors
    ///
    /// Returns the system error of `setsockopt`.
    pub fn setsockopt_str(
        &self,
        level: i32,
        optname: i32,
        data: &str,
    ) -> Result<i32, ExceptionPtr> {
        let c = std::ffi::CString::new(data).unwrap_or_default();
        let len = c.as_bytes_with_nul().len();
        // SAFETY: `c` is a NUL-terminated string of `len` bytes.
        let r = unsafe { libc_setsockopt(self._fd, level, optname, c.as_ptr().cast(), len as u32) };
        throw_system_error_on(r == -1, "setsockopt")?;
        Ok(r)
    }

    /// Sets a socket option from `len` raw bytes at `data`.
    ///
    /// # Errors
    ///
    /// Returns the system error of `setsockopt`.
    ///
    /// # Safety
    ///
    /// `data` must point to `len` readable bytes.
    pub unsafe fn setsockopt_raw(
        &self,
        level: i32,
        optname: i32,
        data: *const c_void,
        len: u32,
    ) -> Result<i32, ExceptionPtr> {
        // SAFETY: forwarded from the caller.
        let r = unsafe { libc_setsockopt(self._fd, level, optname, data, len) };
        throw_system_error_on(r == -1, "setsockopt")?;
        Ok(r)
    }

    /// Reads an integer socket option.
    ///
    /// # Errors
    ///
    /// Returns the system error of `getsockopt`.
    pub fn getsockopt_int(&self, level: i32, optname: i32) -> Result<i32, ExceptionPtr> {
        let mut data: i32 = 0;
        let mut len: u32 = 4;
        // SAFETY: `data` is a live int of `len` bytes.
        let r = unsafe {
            libc_getsockopt(
                self._fd,
                level,
                optname,
                &mut data as *mut i32 as *mut c_void,
                &mut len,
            )
        };
        throw_system_error_on(r == -1, "getsockopt")?;
        Ok(data)
    }

    /// Reads a socket option into `data`; returns the call's result.
    ///
    /// # Errors
    ///
    /// Returns the system error of `getsockopt`.
    pub fn getsockopt_buf(
        &self,
        level: i32,
        optname: i32,
        data: &mut [u8],
    ) -> Result<i32, ExceptionPtr> {
        let mut len = data.len() as u32;
        // SAFETY: `data` has `len` writable bytes.
        let r = unsafe {
            libc_getsockopt(self._fd, level, optname, data.as_mut_ptr().cast(), &mut len)
        };
        throw_system_error_on(r == -1, "getsockopt")?;
        Ok(r)
    }

    /// The file size, from `fstat`.
    ///
    /// # Errors
    ///
    /// Returns the system error of `fstat`.
    pub fn size(&self) -> Result<usize, ExceptionPtr> {
        // SAFETY: `Stat` is plain data; fstat fills it.
        let mut buf: Stat = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        let r = unsafe { fstat(self._fd, &mut buf) };
        throw_system_error_on(r == -1, "fstat")?;
        Ok(buf.st_size as usize)
    }

    /// Reads up to `len` bytes into `buffer`; returns `Ok(None)` if the read would block.
    ///
    /// # Errors
    ///
    /// Returns any other system error of `read`.
    #[inline]
    ///
    /// # Safety
    /// `buffer` is valid for writes of `len` bytes.
    pub unsafe fn read(
        &self,
        buffer: *mut c_void,
        len: usize,
    ) -> Result<Option<usize>, ExceptionPtr> {
        // SAFETY: `buffer` holds `len` writable bytes (caller contract).
        let r = unsafe { libc_read(self._fd, buffer, len) };
        if r == -1 && errno() == EAGAIN {
            return Ok(None);
        }
        throw_system_error_on(r == -1, "read")?;
        Ok(Some(r as usize))
    }

    /// Receives up to `len` bytes into `buffer`; returns `Ok(None)` if the call would block.
    ///
    /// # Errors
    ///
    /// Returns any other system error of `recv`.
    #[inline]
    ///
    /// # Safety
    /// `buffer` is valid for writes of `len` bytes.
    pub unsafe fn recv(
        &self,
        buffer: *mut c_void,
        len: usize,
        flags: i32,
    ) -> Result<Option<isize>, ExceptionPtr> {
        // SAFETY: as in read().
        let r = unsafe { libc_recv(self._fd, buffer, len, flags) };
        if r == -1 && errno() == EAGAIN {
            return Ok(None);
        }
        throw_system_error_on(r == -1, "recv")?;
        Ok(Some(r))
    }

    /// Receives a message into the buffers described by `mh`; returns `Ok(None)` if the call
    /// would block.
    ///
    /// # Errors
    ///
    /// Returns any other system error of `recvmsg`.
    #[inline]
    ///
    /// # Safety
    /// The iovecs, name and control buffers `mh` points to are valid for the writes they describe.
    pub unsafe fn recvmsg(
        &self,
        mh: &mut Msghdr,
        flags: i32,
    ) -> Result<Option<usize>, ExceptionPtr> {
        // SAFETY: `mh` describes caller-owned buffers.
        let r = unsafe { libc_recvmsg(self._fd, mh, flags) };
        if r == -1 && errno() == EAGAIN {
            return Ok(None);
        }
        throw_system_error_on(r == -1, "recvmsg")?;
        Ok(Some(r as usize))
    }

    /// Sends `len` bytes from `buffer`; returns `Ok(None)` if the call would block.
    ///
    /// # Errors
    ///
    /// Returns any other system error of `send`.
    #[inline]
    ///
    /// # Safety
    /// `buffer` is valid for reads of `len` bytes.
    pub unsafe fn send(
        &self,
        buffer: *const c_void,
        len: usize,
        flags: i32,
    ) -> Result<Option<usize>, ExceptionPtr> {
        // SAFETY: `buffer` holds `len` readable bytes.
        let r = unsafe { libc_send(self._fd, buffer, len, flags) };
        if r == -1 && errno() == EAGAIN {
            return Ok(None);
        }
        throw_system_error_on(r == -1, "send")?;
        Ok(Some(r as usize))
    }

    /// Sends `len` bytes from `buf` to `addr`; returns `Ok(None)` if the call would block.
    ///
    /// # Errors
    ///
    /// Returns any other system error of `sendto`.
    #[inline]
    ///
    /// # Safety
    /// `buf` is valid for reads of `len` bytes.
    pub unsafe fn sendto(
        &self,
        addr: &SocketAddress,
        buf: *const c_void,
        len: usize,
        flags: i32,
    ) -> Result<Option<usize>, ExceptionPtr> {
        // SAFETY: `buf` holds `len` readable bytes; `addr` is a valid address.
        let r = unsafe {
            libc_sendto(
                self._fd,
                buf,
                len,
                flags,
                addr.as_posix_sockaddr() as *const _ as *const c_void,
                addr.length(),
            )
        };
        if r == -1 && errno() == EAGAIN {
            return Ok(None);
        }
        throw_system_error_on(r == -1, "sendto")?;
        Ok(Some(r as usize))
    }

    /// Sends the message described by `msg`; returns `Ok(None)` if the call would block.
    ///
    /// # Errors
    ///
    /// Returns any other system error of `sendmsg`.
    #[inline]
    ///
    /// # Safety
    /// The iovecs, name and control buffers `msg` points to are valid for the reads they describe.
    pub unsafe fn sendmsg(&self, msg: &Msghdr, flags: i32) -> Result<Option<usize>, ExceptionPtr> {
        // SAFETY: `msg` describes caller-owned buffers.
        let r = unsafe { libc_sendmsg(self._fd, msg, flags) };
        if r == -1 && errno() == EAGAIN {
            return Ok(None);
        }
        throw_system_error_on(r == -1, "sendmsg")?;
        Ok(Some(r as usize))
    }

    /// Binds the socket to the address `sa` of length `sl`.
    ///
    /// # Errors
    ///
    /// Returns the system error of `bind`.
    ///
    /// # Safety
    /// `sa` is a socket address of at least `sl` bytes.
    pub unsafe fn bind<A>(&self, sa: &A, sl: u32) -> Result<(), ExceptionPtr> {
        // SAFETY: `sa` is a sockaddr of `sl` bytes.
        let r = unsafe { libc_bind(self._fd, sa as *const A as *const c_void, sl) };
        throw_system_error_on(r == -1, "bind")
    }

    /// Connects the socket to `sa` of length `sl`. On a non-blocking socket, `EINPROGRESS` is
    /// not an error.
    ///
    /// # Errors
    ///
    /// Returns any other system error of `connect`.
    ///
    /// # Safety
    /// `sa` is a socket address of at least `sl` bytes.
    pub unsafe fn connect<A>(&self, sa: &A, sl: u32) -> Result<(), ExceptionPtr> {
        // SAFETY: `sa` is a sockaddr of `sl` bytes.
        let r = unsafe { libc_connect(self._fd, sa as *const A as *const c_void, sl) };
        if r == -1 && errno() == EINPROGRESS {
            return Ok(());
        }
        throw_system_error_on(r == -1, "connect")
    }

    /// The local address of the socket (`getsockname`).
    ///
    /// # Errors
    ///
    /// Returns the system error of `getsockname`.
    pub fn get_address(&self) -> Result<SocketAddress, ExceptionPtr> {
        let mut addr = SocketAddress::default();
        // SAFETY: `addr` provides storage and its length.
        let r = unsafe {
            getsockname(
                self._fd,
                addr.as_posix_sockaddr_mut() as *mut _ as *mut c_void,
                &mut addr.addr_length,
            )
        };
        throw_system_error_on(r == -1, "getsockname")?;
        Ok(addr)
    }

    /// The peer address of the socket (`getpeername`).
    ///
    /// # Errors
    ///
    /// Returns the system error of `getpeername`.
    pub fn get_remote_address(&self) -> Result<SocketAddress, ExceptionPtr> {
        let mut addr = SocketAddress::default();
        // SAFETY: as above.
        let r = unsafe {
            getpeername(
                self._fd,
                addr.as_posix_sockaddr_mut() as *mut _ as *mut c_void,
                &mut addr.addr_length,
            )
        };
        throw_system_error_on(r == -1, "getpeername")?;
        Ok(addr)
    }

    /// Marks the socket as listening with the given `backlog`.
    ///
    /// # Errors
    ///
    /// Returns the system error of `listen`.
    pub fn listen(&self, backlog: i32) -> Result<(), ExceptionPtr> {
        // SAFETY: plain system call.
        let fd = unsafe { libc_listen(self._fd, backlog) };
        throw_system_error_on(fd == -1, "listen")
    }

    /// Writes up to `len` bytes from `buf`; returns `Ok(None)` if the write would block.
    ///
    /// # Errors
    ///
    /// Returns any other system error of `write`.
    #[inline]
    ///
    /// # Safety
    /// `buf` is valid for reads of `len` bytes.
    pub unsafe fn write(
        &self,
        buf: *const c_void,
        len: usize,
    ) -> Result<Option<usize>, ExceptionPtr> {
        // SAFETY: `buf` holds `len` readable bytes.
        let r = unsafe { libc_write(self._fd, buf, len) };
        if r == -1 && errno() == EAGAIN {
            return Ok(None);
        }
        throw_system_error_on(r == -1, "write")?;
        Ok(Some(r as usize))
    }

    /// Writes the buffers of `iov`; returns `Ok(None)` if the write would block.
    ///
    /// # Errors
    ///
    /// Returns any other system error of `writev`.
    #[inline]
    ///
    /// # Safety
    /// each iovec of `iov` is valid for reads of its length.
    pub unsafe fn writev(&self, iov: &[Iovec]) -> Result<Option<usize>, ExceptionPtr> {
        // SAFETY: `iov` describes caller-owned buffers.
        let r = unsafe { libc_writev(self._fd, iov.as_ptr(), iov.len() as i32) };
        if r == -1 && errno() == EAGAIN {
            return Ok(None);
        }
        throw_system_error_on(r == -1, "writev")?;
        Ok(Some(r as usize))
    }

    /// Reads up to `len` bytes at offset `off` into `buf` (blocking).
    ///
    /// # Errors
    ///
    /// Returns the system error of `pread`.
    ///
    /// # Safety
    /// `buf` is valid for writes of `len` bytes.
    pub unsafe fn pread(
        &self,
        buf: *mut c_void,
        len: usize,
        off: i64,
    ) -> Result<usize, ExceptionPtr> {
        // SAFETY: `buf` holds `len` writable bytes.
        let r = unsafe { libc_pread(self._fd, buf, len, off) };
        throw_system_error_on(r == -1, "pread")?;
        Ok(r as usize)
    }

    /// Arms or disarms the timerfd with `its`.
    ///
    /// # Errors
    ///
    /// Returns the system error of `timerfd_settime`.
    #[inline]
    pub fn timerfd_settime(&self, flags: i32, its: &Itimerspec) -> Result<(), ExceptionPtr> {
        // SAFETY: `its` is a valid itimerspec.
        let r = unsafe { libc_timerfd_settime(self._fd, flags, its, std::ptr::null_mut()) };
        throw_system_error_on(r == -1, "timerfd_settime")
    }

    /// Maps `size` bytes of the file at `offset` with protection `prot` and `flags`, at `addr`
    /// (or where the kernel chooses when null).
    ///
    /// # Errors
    ///
    /// Returns the system error of `mmap`.
    ///
    /// # Safety
    ///
    /// With `MAP_FIXED` in `flags`, `addr .. addr + size` must be memory the caller owns and
    /// whose contents may be discarded: the new mapping replaces whatever is there.
    pub unsafe fn map(
        &self,
        size: usize,
        prot: i32,
        flags: i32,
        offset: usize,
        addr: *mut c_void,
    ) -> Result<MmapArea, ExceptionPtr> {
        // SAFETY: mapping our descriptor; a MAP_FIXED `addr` replaces only memory the caller
        // owns and may discard (by the caller).
        let x = unsafe { mmap(addr, size, prot, flags, self._fd, offset as i64) };
        throw_system_error_on(x == MAP_FAILED, "mmap")?;
        Ok(MmapArea { p: x.cast(), d: MmapDeleter { _size: size } })
    }

    /// Maps `size` bytes at `offset` shared and read-write.
    ///
    /// # Errors
    ///
    /// Returns the system error of `mmap`.
    pub fn map_shared_rw(&self, size: usize, offset: usize) -> Result<MmapArea, ExceptionPtr> {
        // SAFETY: no MAP_FIXED, so the kernel chooses where the mapping goes.
        unsafe { self.map(size, PROT_READ | PROT_WRITE, MAP_SHARED, offset, std::ptr::null_mut()) }
    }

    /// Maps `size` bytes at `offset` shared and read-only.
    ///
    /// # Errors
    ///
    /// Returns the system error of `mmap`.
    pub fn map_shared_ro(&self, size: usize, offset: usize) -> Result<MmapArea, ExceptionPtr> {
        // SAFETY: no MAP_FIXED, so the kernel chooses where the mapping goes.
        unsafe { self.map(size, PROT_READ, MAP_SHARED, offset, std::ptr::null_mut()) }
    }

    /// Maps `size` bytes at `offset` private (copy-on-write) and read-write.
    ///
    /// # Errors
    ///
    /// Returns the system error of `mmap`.
    pub fn map_private_rw(&self, size: usize, offset: usize) -> Result<MmapArea, ExceptionPtr> {
        // SAFETY: no MAP_FIXED, so the kernel chooses where the mapping goes.
        unsafe { self.map(size, PROT_READ | PROT_WRITE, MAP_PRIVATE, offset, std::ptr::null_mut()) }
    }

    /// Maps `size` bytes at `offset` private and read-only.
    ///
    /// # Errors
    ///
    /// Returns the system error of `mmap`.
    pub fn map_private_ro(&self, size: usize, offset: usize) -> Result<MmapArea, ExceptionPtr> {
        // SAFETY: no MAP_FIXED, so the kernel chooses where the mapping goes.
        unsafe { self.map(size, PROT_READ, MAP_PRIVATE, offset, std::ptr::null_mut()) }
    }

    /// Adds "close this descriptor" to the spawn file actions `actions`.
    ///
    /// # Errors
    ///
    /// Returns the error of `posix_spawn_file_actions_addclose`.
    ///
    /// # Safety
    /// `actions` is an initialized `posix_spawn_file_actions_t`.
    pub unsafe fn spawn_actions_add_close<A>(&self, actions: &mut A) -> Result<(), ExceptionPtr> {
        // SAFETY: `actions` is an initialized posix_spawn_file_actions_t.
        let r = unsafe {
            posix_spawn_file_actions_addclose(actions as *mut A as *mut c_void, self._fd)
        };
        throw_pthread_error(r)
    }

    /// Adds "duplicate this descriptor onto `newfd`" to the spawn file actions `actions`.
    ///
    /// # Errors
    ///
    /// Returns the error of `posix_spawn_file_actions_adddup2`.
    ///
    /// # Safety
    /// `actions` is an initialized `posix_spawn_file_actions_t`.
    pub unsafe fn spawn_actions_add_dup2<A>(
        &self,
        actions: &mut A,
        newfd: i32,
    ) -> Result<(), ExceptionPtr> {
        // SAFETY: as above.
        let r = unsafe {
            posix_spawn_file_actions_adddup2(actions as *mut A as *mut c_void, self._fd, newfd)
        };
        throw_pthread_error(r)
    }

    /// A description of what the descriptor refers to (the target of `/proc/self/fd/N`), or
    /// `error(errno)` if it cannot be read. For diagnostics; never fails.
    pub fn fdinfo(&self) -> Sstring {
        let _section =
            crate::engine::util::critical_alloc_section::memory::ScopedCriticalAllocSection::new();
        let path =
            std::ffi::CString::new(format!("/proc/self/fd/{}", self._fd)).unwrap_or_default();
        let mut buf = [0u8; 64];
        // SAFETY: `buf` has 64 writable bytes.
        let ret = unsafe { readlink(path.as_ptr(), buf.as_mut_ptr().cast::<c_char>(), buf.len()) };
        if ret > 0 {
            Sstring::from(String::from_utf8_lossy(&buf[..ret as usize]).into_owned())
        } else {
            Sstring::from(format!("error({})", errno()))
        }
    }
}

/// Socket shutdown masks and time conversions for timers.
#[allow(clippy::module_inception)]
pub mod posix {
    use super::*;

    /// Mask bit: the receive side is shut down.
    pub const RCV_SHUTDOWN: u32 = 0x1;
    /// Mask bit: the send side is shut down.
    pub const SND_SHUTDOWN: u32 = 0x2;
    /// The shutdown mask ([`RCV_SHUTDOWN`] | [`SND_SHUTDOWN`] bits) for a `shutdown(2)` `how`.
    #[inline]
    pub const fn shutdown_mask(how: i32) -> u32 {
        (how + 1) as u32
    }

    /// Converts a duration to a `timespec`.
    #[inline]
    pub fn to_timespec(d: Duration) -> Timespec {
        let ns = d.as_nanos() as i64;
        Timespec { tv_sec: ns / 1_000_000_000, tv_nsec: ns % 1_000_000_000 }
    }

    /// Converts a relative first expiration `base` and a re-arm period `interval` to an
    /// `itimerspec`.
    #[inline]
    pub fn to_relative_itimerspec(base: Duration, interval: Duration) -> Itimerspec {
        Itimerspec { it_interval: to_timespec(interval), it_value: to_timespec(base) }
    }

    /// Converts an absolute first expiration (`base_since_epoch`, measured on the timer's
    /// clock) and a re-arm period `interval` to an `itimerspec`.
    #[inline]
    pub fn to_absolute_itimerspec(base_since_epoch: Duration, interval: Duration) -> Itimerspec {
        to_relative_itimerspec(base_since_epoch, interval)
    }
}

/// Re-exports of the most used [`posix`] items.
pub use posix::{RCV_SHUTDOWN, SND_SHUTDOWN, shutdown_mask, to_relative_itimerspec, to_timespec};

/// Shut down the receive side.
pub const SHUT_RD: i32 = 0;
/// Shut down the send side.
pub const SHUT_WR: i32 = 1;
/// Shut down both sides.
pub const SHUT_RDWR: i32 = 2;

/// Thread attribute: the stack size.
#[derive(Clone, Copy, Debug, Default)]
pub struct StackSize {
    /// Stack size in bytes; zero selects the default (2 MiB).
    pub size: usize,
}

/// Attributes of a [`PosixThread`]: stack size and CPU affinity.
#[derive(Clone, Copy, Default)]
pub struct PosixThreadAttr {
    _stack_size: StackSize,
    _affinity: Option<CpuSetT>,
}

impl PosixThreadAttr {
    /// Sets the stack size.
    pub fn set_stack_size(mut self, ss: StackSize) -> Self {
        self._stack_size = ss;
        self
    }
    /// Sets the CPU affinity.
    pub fn set_affinity(mut self, affinity: CpuSetT) -> Self {
        self._affinity = Some(affinity);
        self
    }
}

type ThreadFunc = Box<dyn FnOnce() + Send>;

/// A pthread with a 2 MiB-aligned stack backed by transparent huge pages and surrounded by
/// guard areas.
///
/// The thread must be [`join`](Self::join)ed before the handle is dropped.
pub struct PosixThread {
    // The function lives in its own allocation, because the handle can move while the thread
    // runs.
    _func: *mut Option<ThreadFunc>,
    _pthread: usize,
    _valid: bool,
    _stack: MmapArea,
}

// SAFETY: the handle only joins the thread; the function box is owned by the thread until join.
unsafe impl Send for PosixThread {}

extern "C" fn start_routine(arg: *mut c_void) -> *mut c_void {
    let pfunc = arg.cast::<Option<ThreadFunc>>();
    // SAFETY: `arg` is the function slot the PosixThread keeps alive until join.
    if let Some(f) = unsafe { (*pfunc).take() } {
        f();
    }
    std::ptr::null_mut()
}

/// Rounds `p` up to a multiple of `align` (a power of two).
#[inline]
#[cfg(not(engine_asan_enabled))]
fn align_up_ptr(p: *mut u8, align: usize) -> *mut u8 {
    let a = p as usize;
    ((a + align - 1) & !(align - 1)) as *mut u8
}

/// Creates an anonymous mapping of `length` bytes with protection `prot` and `flags`, at
/// `addr` (or where the kernel chooses when null).
///
/// # Errors
///
/// Returns the system error of `mmap`.
///
/// # Safety
///
/// With `MAP_FIXED` in `flags`, `addr .. addr + length` must be memory the caller owns and
/// whose contents may be discarded: the new mapping replaces whatever is there.
pub unsafe fn mmap_anonymous(
    addr: *mut c_void,
    length: usize,
    prot: i32,
    flags: i32,
) -> Result<MmapArea, ExceptionPtr> {
    // SAFETY: anonymous mapping; a MAP_FIXED `addr` replaces only memory the caller owns and
    // may discard (by the caller).
    let ret = unsafe { mmap(addr, length, prot, flags | MAP_ANONYMOUS, -1, 0) };
    throw_system_error_on(ret == MAP_FAILED, "")?;
    Ok(MmapArea { p: ret.cast(), d: MmapDeleter { _size: length } })
}

impl PosixThread {
    /// Starts a thread running `func`, with default attributes.
    ///
    /// # Errors
    ///
    /// Returns the error of the stack mapping or of `pthread_create`.
    pub fn new(func: ThreadFunc) -> Result<PosixThread, ExceptionPtr> {
        Self::with_attr(PosixThreadAttr::default(), func)
    }

    /// Starts a thread running `func` with attributes `a`.
    ///
    /// The stack is reserved with a guard area, aligned to 2 MiB and advised for huge pages
    /// (unless built for AddressSanitizer, which uses the default pthread stack).
    ///
    /// # Errors
    ///
    /// Returns the error of the stack mapping or of a pthread call.
    pub fn with_attr(a: PosixThreadAttr, func: ThreadFunc) -> Result<PosixThread, ExceptionPtr> {
        let fslot = Box::into_raw(Box::new(Some(func)));
        let mut this =
            PosixThread { _func: fslot, _pthread: 0, _valid: true, _stack: MmapArea::null() };
        let mut pa = PthreadAttrT::zeroed();
        // SAFETY: `pa` is storage for a pthread_attr_t.
        let mut r = unsafe { pthread_attr_init(&mut pa) };
        if r != 0 {
            this._valid = false;
            return Err(ExceptionPtr::from(SystemError::from_errno(r)));
        }
        #[cfg(not(engine_asan_enabled))]
        {
            let mut stack_size = a._stack_size.size;
            if stack_size == 0 {
                stack_size = 2 << 20;
            }
            // allocate guard area as well
            // SAFETY: no MAP_FIXED, so the kernel chooses where the mapping goes.
            this._stack = match unsafe {
                mmap_anonymous(
                    std::ptr::null_mut(),
                    stack_size + (4 << 20),
                    PROT_NONE,
                    MAP_PRIVATE | MAP_NORESERVE,
                )
            } {
                Ok(s) => s,
                Err(e) => {
                    this._valid = false;
                    return Err(e);
                }
            };
            // SAFETY: the guard area is larger than one byte.
            let stack_start = align_up_ptr(unsafe { this._stack.get().add(1) }, 2 << 20);
            // SAFETY: `stack_start .. stack_start + stack_size` lies inside the PROT_NONE
            // reservation just made (it is `stack_size` plus 4 MiB long and the start is
            // aligned up by less than 2 MiB), which this thread object owns and nothing uses.
            let real_stack = match unsafe {
                mmap_anonymous(
                    stack_start.cast(),
                    stack_size,
                    PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_FIXED | MAP_STACK,
                )
            } {
                Ok(s) => s,
                Err(e) => {
                    this._valid = false;
                    return Err(e);
                }
            };
            let _ = real_stack.release(); // protected by @_stack
            // SAFETY: advising the stack mapped above.
            unsafe { madvise(stack_start.cast(), stack_size, MADV_HUGEPAGE) };
            // SAFETY: the stack lives as long as the thread handle.
            r = unsafe { pthread_attr_setstack(&mut pa, stack_start.cast(), stack_size) };
            if r != 0 {
                this._valid = false;
                return Err(ExceptionPtr::from(SystemError::from_errno(r)));
            }
        }
        if let Some(cpuset) = &a._affinity {
            // SAFETY: valid attribute and cpu set.
            unsafe { pthread_attr_setaffinity_np(&mut pa, std::mem::size_of::<CpuSetT>(), cpuset) };
        }
        // SAFETY: `fslot` stays alive until the thread is joined.
        r = unsafe { pthread_create(&mut this._pthread, &pa, start_routine, fslot.cast()) };
        if r != 0 {
            this._valid = false;
            return Err(ExceptionPtr::from(SystemError::from_errno(r)));
        }
        Ok(this)
    }

    /// Waits for the thread to exit.
    ///
    /// Aborts the process if the thread was not started successfully.
    pub fn join(mut self) {
        engine_assert!(self._valid);
        // SAFETY: joining the thread we created.
        unsafe { pthread_join(self._pthread, std::ptr::null_mut()) };
        self._valid = false;
    }
}

impl Drop for PosixThread {
    fn drop(&mut self) {
        engine_assert!(!self._valid);
        // SAFETY: the thread is joined (or never started); the slot is ours again.
        drop(unsafe { Box::from_raw(self._func) });
    }
}

/// Returns the current `errno` as a system error, with `what_arg` as context, if `condition`
/// holds.
///
/// `EBADF` and `ENOTSOCK` abort the process instead when abort-on-EBADF is enabled, to catch
/// descriptor misuse at its source.
///
/// # Errors
///
/// Returns the system error when `condition` is true.
#[inline]
pub fn throw_system_error_on(condition: bool, what_arg: &str) -> Result<(), ExceptionPtr> {
    if condition {
        let e = errno();
        if (e == EBADF || e == ENOTSOCK) && is_abort_on_ebadf_enabled() {
            std::process::abort();
        }
        return Err(ExceptionPtr::from(SystemError::new(e, what_arg)));
    }
    Ok(())
}

/// Turns a negative kernel return value (`-errno`) into a system error.
///
/// # Errors
///
/// Returns [`kernel_error`]`(r)` when `r < 0`.
#[inline]
pub fn throw_kernel_error(r: isize) -> Result<(), ExceptionPtr> {
    if r < 0 {
        return Err(kernel_error(r));
    }
    Ok(())
}

/// The system error for a negative kernel return value `r` (`-errno`). Aborts on `EBADF` and
/// `ENOTSOCK` when abort-on-EBADF is enabled.
#[cold]
pub fn kernel_error(r: isize) -> ExceptionPtr {
    let ec = (-r) as i32;
    if (ec == EBADF || ec == ENOTSOCK) && is_abort_on_ebadf_enabled() {
        std::process::abort();
    }
    ExceptionPtr::from(SystemError::from_errno(ec))
}

/// Turns a non-zero pthread return value (an errno value) into a system error.
///
/// # Errors
///
/// Returns the system error when `r != 0`.
#[inline]
pub fn throw_pthread_error(r: i32) -> Result<(), ExceptionPtr> {
    if r != 0 {
        return Err(ExceptionPtr::from(SystemError::from_errno(r)));
    }
    Ok(())
}

/// A signal set holding only `signo`.
#[inline]
pub fn make_sigset_mask(signo: i32) -> SigsetT {
    let mut set = SigsetT::default();
    // SAFETY: valid signal set.
    unsafe {
        sigemptyset(&mut set);
        sigaddset(&mut set, signo);
    }
    set
}

/// A signal set holding every signal.
#[inline]
pub fn make_full_sigset_mask() -> SigsetT {
    let mut set = SigsetT::default();
    // SAFETY: valid signal set.
    unsafe { sigfillset(&mut set) };
    set
}

/// An empty signal set.
#[inline]
pub fn make_empty_sigset_mask() -> SigsetT {
    let mut set = SigsetT::default();
    // SAFETY: valid signal set.
    unsafe { sigemptyset(&mut set) };
    set
}

/// Pins the calling thread to CPU `cpu_id`.
///
/// Aborts the process if `pthread_setaffinity_np` fails.
#[inline]
pub fn pin_this_thread(cpu_id: u32) {
    let mut cs = CpuSetT::default();
    cs.set(cpu_id as usize);
    // SAFETY: valid cpu set for the calling thread.
    let r = unsafe { pthread_setaffinity_np(pthread_self(), std::mem::size_of::<CpuSetT>(), &cs) };
    engine_assert!(r == 0);
}

/// The CPUs the calling thread may run on.
///
/// Aborts the process if `pthread_getaffinity_np` fails.
pub fn get_current_cpuset() -> BTreeSet<u32> {
    let mut cs = CpuSetT::default();
    // SAFETY: valid cpu set for the calling thread.
    let r =
        unsafe { pthread_getaffinity_np(pthread_self(), std::mem::size_of::<CpuSetT>(), &mut cs) };
    engine_assert!(r == 0);
    let mut ret = BTreeSet::new();
    let nr = cs.count() as usize;
    let mut cpu = 0usize;
    while cpu < CpuSetT::SETSIZE && ret.len() < nr {
        if cs.is_set(cpu) {
            ret.insert(cpu as u32);
        }
        cpu += 1;
    }
    ret
}
