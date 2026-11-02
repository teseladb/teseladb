//! Linux: `posix_fallocate`, epoll, eventfd, affinity and io_uring.

pub mod io_uring;

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

pub(crate) const ENOSPC: i32 = 28;

unsafe extern "C" {
    fn posix_fallocate(fd: i32, offset: i64, len: i64) -> i32;
}

pub(crate) fn preallocate(file: &File, len: u64) -> io::Result<()> {
    let len = i64::try_from(len).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: valid descriptor during the call; no pointers. `posix_fallocate` returns the error
    // code instead of using `errno`.
    let rc = unsafe { posix_fallocate(file.as_raw_fd(), 0, len) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(())
}

// ----- epoll, eventfd and affinity -----

use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::Duration;

use crate::poll::{Event, Interest, WAKE_TOKEN, timeout_ms};

const EPOLL_CLOEXEC: i32 = 0o2000000;
const EPOLL_CTL_ADD: i32 = 1;
const EPOLL_CTL_DEL: i32 = 2;
const EPOLL_CTL_MOD: i32 = 3;
const EPOLLIN: u32 = 0x001;
const EPOLLOUT: u32 = 0x004;
const EPOLLERR: u32 = 0x008;
const EPOLLHUP: u32 = 0x010;
const EPOLLRDHUP: u32 = 0x2000;
const EFD_CLOEXEC: i32 = 0o2000000;
const EFD_NONBLOCK: i32 = 0o4000;
const EINTR: i32 = 4;
/// Events per `epoll_wait` call.
const MAX_EVENTS: usize = 256;
/// Bits of `cpu_set_t` (glibc and musl).
const CPU_SETSIZE: usize = 1024;

/// `struct epoll_event`: packed on x86_64 (12 bytes), aligned elsewhere (16 bytes).
#[repr(C)]
#[cfg_attr(target_arch = "x86_64", repr(packed))]
#[derive(Clone, Copy, Default)]
pub(crate) struct EpollEvent {
    events: u32,
    data: u64,
}

unsafe extern "C" {
    fn epoll_create1(flags: i32) -> i32;
    fn epoll_ctl(epfd: i32, op: i32, fd: i32, event: *mut EpollEvent) -> i32;
    fn epoll_wait(epfd: i32, events: *mut EpollEvent, maxevents: i32, timeout: i32) -> i32;
    fn eventfd(initval: u32, flags: i32) -> i32;
    fn sched_setaffinity(pid: i32, cpusetsize: usize, mask: *const u64) -> i32;
}

fn cvt(rc: i32) -> std::io::Result<i32> {
    if rc < 0 { Err(std::io::Error::last_os_error()) } else { Ok(rc) }
}

/// New descriptor returned by a system call.
fn owned(rc: i32) -> std::io::Result<OwnedFd> {
    let fd = cvt(rc)?;
    // SAFETY: the call has just created `fd` and nobody else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// A non-blocking `eventfd` closed on `exec`.
pub(crate) fn new_eventfd() -> std::io::Result<OwnedFd> {
    // SAFETY: no pointers.
    owned(unsafe { eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK) })
}

fn raw(fd: &OwnedFd) -> RawFd {
    use std::os::fd::AsRawFd;
    fd.as_raw_fd()
}

fn ctl(ep: &OwnedFd, op: i32, fd: RawFd, events: u32, token: u64) -> std::io::Result<()> {
    let mut ev = EpollEvent { events, data: token };
    // SAFETY: `ev` lives during the call; `epoll_ctl` does not keep the pointer.
    cvt(unsafe { epoll_ctl(raw(ep), op, fd, &mut ev) }).map(|_| ())
}

fn mask(i: Interest) -> u32 {
    let mut m = 0;
    if i.read {
        m |= EPOLLIN | EPOLLRDHUP;
    }
    if i.write {
        m |= EPOLLOUT;
    }
    m
}

#[derive(Debug)]
pub(crate) struct Poller {
    ep: OwnedFd,
    wake: Arc<std::fs::File>,
}

#[derive(Clone, Debug)]
pub(crate) struct Waker {
    wake: Arc<std::fs::File>,
}

impl Poller {
    pub(crate) fn new() -> std::io::Result<Poller> {
        // SAFETY: no pointers.
        let ep = owned(unsafe { epoll_create1(EPOLL_CLOEXEC) })?;
        // SAFETY: no pointers.
        let efd = owned(unsafe { eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK) })?;
        ctl(&ep, EPOLL_CTL_ADD, raw(&efd), EPOLLIN, WAKE_TOKEN)?;
        Ok(Poller { ep, wake: Arc::new(std::fs::File::from(efd)) })
    }

    pub(crate) fn register(&self, fd: RawFd, token: u64, i: Interest) -> std::io::Result<()> {
        ctl(&self.ep, EPOLL_CTL_ADD, fd, mask(i), token)
    }

    pub(crate) fn modify(&self, fd: RawFd, token: u64, i: Interest) -> std::io::Result<()> {
        ctl(&self.ep, EPOLL_CTL_MOD, fd, mask(i), token)
    }

    pub(crate) fn deregister(&self, fd: RawFd) -> std::io::Result<()> {
        ctl(&self.ep, EPOLL_CTL_DEL, fd, 0, 0)
    }

    pub(crate) fn wait(
        &self,
        out: &mut Vec<Event>,
        timeout: Option<Duration>,
    ) -> std::io::Result<()> {
        let mut evs = [EpollEvent::default(); MAX_EVENTS];
        // SAFETY: `evs` has `MAX_EVENTS` writable entries.
        let n = unsafe {
            epoll_wait(raw(&self.ep), evs.as_mut_ptr(), MAX_EVENTS as i32, timeout_ms(timeout))
        };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            return if e.raw_os_error() == Some(EINTR) { Ok(()) } else { Err(e) };
        }
        for e in &evs[..n as usize] {
            // Copies: fields of a packed struct cannot be taken by reference.
            let (events, token) = (e.events, e.data);
            if token == WAKE_TOKEN {
                // Drain the counter; if another thread already drained it there is nothing to read.
                let mut buf = [0u8; 8];
                let _ = (&*self.wake).read(&mut buf);
                out.push(Event { token, readable: false, writable: false, hangup: false });
                continue;
            }
            out.push(Event {
                token,
                readable: events & EPOLLIN != 0,
                writable: events & EPOLLOUT != 0,
                hangup: events & (EPOLLHUP | EPOLLERR | EPOLLRDHUP) != 0,
            });
        }
        Ok(())
    }

    pub(crate) fn waker(&self) -> Waker {
        Waker { wake: Arc::clone(&self.wake) }
    }
}

impl Waker {
    pub(crate) fn wake(&self) -> std::io::Result<()> {
        match (&*self.wake).write(&1u64.to_ne_bytes()) {
            // Counter at its maximum: a wake-up is already pending.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
            r => r.map(|_| ()),
        }
    }
}

pub(crate) fn pin_to_cpu(cpu: usize) -> std::io::Result<bool> {
    if cpu >= CPU_SETSIZE {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    let mut set = [0u64; CPU_SETSIZE / 64];
    set[cpu / 64] |= 1 << (cpu % 64);
    // SAFETY: `set` is a `cpu_set_t` of `CPU_SETSIZE` bits that lives during the call; pid 0 is
    // the calling thread.
    cvt(unsafe { sched_setaffinity(0, std::mem::size_of_val(&set), set.as_ptr()) })?;
    Ok(true)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    unsafe extern "C" {
        fn pthread_kill(thread: std::os::unix::thread::RawPthread, sig: i32) -> i32;
        fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
    }

    const SIGUSR1: i32 = 10;

    extern "C" fn ignore(_sig: i32) {}

    #[test]
    fn preallocate_rejects_a_size_beyond_i64() {
        let path =
            std::env::temp_dir().join(format!("teseladb-sys-prealloc-{}", std::process::id()));
        let file = File::create(&path).unwrap();
        let err = preallocate(&file, u64::MAX).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn system_call_errors_are_returned() {
        // Waits on something that is not an epoll descriptor.
        let not_an_epoll = OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
        let p =
            Poller { ep: not_an_epoll, wake: Arc::new(std::fs::File::open("/dev/null").unwrap()) };
        let err = p.wait(&mut Vec::new(), Some(Duration::ZERO)).unwrap_err();
        assert_ne!(err.raw_os_error(), Some(EINTR));
        // A wake-up that cannot be written is an error, not a pending wake-up.
        assert!(p.waker().wake().is_err());
    }

    #[test]
    fn a_wake_on_a_full_counter_is_already_pending() {
        let p = Poller::new().unwrap();
        // The largest value an eventfd counter holds: one more would block.
        (&*p.wake).write_all(&(u64::MAX - 1).to_ne_bytes()).unwrap();
        p.waker().wake().unwrap();
        let mut out = Vec::new();
        p.wait(&mut out, Some(Duration::ZERO)).unwrap();
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn a_signal_during_the_wait_returns_without_events() {
        // SAFETY: `ignore` is a valid handler for the whole program and does nothing.
        assert_ne!(unsafe { signal(SIGUSR1, ignore) }, usize::MAX);
        // The signal may arrive before the thread enters `epoll_wait`: then the wait lasts its
        // whole timeout and the round is repeated.
        let interrupted = (0..50).any(|_| {
            let (tx, rx) = std::sync::mpsc::channel();
            let waiter = std::thread::spawn(move || {
                let p = Poller::new().unwrap();
                let mut out = Vec::new();
                tx.send(()).unwrap();
                let start = std::time::Instant::now();
                p.wait(&mut out, Some(Duration::from_secs(2))).unwrap();
                (start.elapsed(), out.len())
            });
            rx.recv().unwrap();
            std::thread::sleep(Duration::from_millis(20));
            use std::os::unix::thread::JoinHandleExt;
            // SAFETY: the thread is alive until `join`; `SIGUSR1` has a handler that does nothing.
            assert_eq!(unsafe { pthread_kill(waiter.as_pthread_t(), SIGUSR1) }, 0);
            let (elapsed, events) = waiter.join().unwrap();
            assert_eq!(events, 0);
            elapsed < Duration::from_secs(1)
        });
        assert!(interrupted, "no wait was interrupted by the signal");
    }

    #[test]
    fn struct_layouts_match_the_system_headers() {
        let expected = if cfg!(target_arch = "x86_64") { 12 } else { 16 };
        assert_eq!(std::mem::size_of::<EpollEvent>(), expected);
        assert_eq!(std::mem::size_of::<[u64; CPU_SETSIZE / 64]>(), 128);
    }
}

// Host facts (`host.rs`).

/// `suseconds_t`.
pub(crate) type Suseconds = i64;
/// `sysconf(3)` names.
pub(crate) const SC_PAGESIZE: i32 = 30;
pub(crate) const SC_PHYS_PAGES: i32 = 85;
pub(crate) const SC_NPROCESSORS_ONLN: i32 = 84;
/// `getrlimit(2)` resource of open files.
pub(crate) const RLIMIT_NOFILE: i32 = 7;
/// `ru_maxrss` is in kilobytes on Linux.
/// `CLOCK_THREAD_CPUTIME_ID`: the CPU time of the calling thread.
pub(crate) const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
pub(crate) const MAXRSS_PER_KB: i64 = 1;

/// Bytes of each field of `struct utsname`.
const UTS_LEN: usize = 65;

#[repr(C)]
pub(crate) struct Utsname {
    pub(crate) sysname: [u8; UTS_LEN],
    pub(crate) nodename: [u8; UTS_LEN],
    pub(crate) release: [u8; UTS_LEN],
    pub(crate) version: [u8; UTS_LEN],
    pub(crate) machine: [u8; UTS_LEN],
    pub(crate) domainname: [u8; UTS_LEN],
}

impl Default for Utsname {
    fn default() -> Utsname {
        let z = [0u8; UTS_LEN];
        Utsname { sysname: z, nodename: z, release: z, version: z, machine: z, domainname: z }
    }
}

#[repr(C)]
#[derive(Default)]
pub(crate) struct Statvfs {
    bsize: u64,
    frsize: u64,
    blocks: u64,
    bfree: u64,
    bavail: u64,
    files: u64,
    ffree: u64,
    favail: u64,
    fsid: u64,
    flag: u64,
    namemax: u64,
    spare: [i32; 6],
}

impl Statvfs {
    /// Blocks, free blocks and the bytes of a block.
    pub(crate) fn sizes(&self) -> (u64, u64, u64) {
        (self.blocks, self.bfree, self.frsize)
    }
}

/// Resident and virtual memory from `/proc/self/statm` (in pages) and threads from
/// `/proc/self/status`.
pub(crate) fn memory() -> io::Result<crate::host::Memory> {
    let statm = std::fs::read_to_string("/proc/self/statm")?;
    let mut pages = statm.split_whitespace().map(|n| n.parse::<u64>().unwrap_or(0));
    let (size, resident) = (pages.next().unwrap_or(0), pages.next().unwrap_or(0));
    let page = crate::host::page_size().unwrap_or(4096);
    let status = std::fs::read_to_string("/proc/self/status")?;
    let threads = status
        .lines()
        .find_map(|l| l.strip_prefix("Threads:"))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0);
    Ok(crate::host::Memory { resident: resident * page, virtual_size: size * page, threads })
}

/// `NAME` and `VERSION_ID` of `/etc/os-release`, as RefDb shows them in `hostInfo`.
pub(crate) fn distribution() -> (String, String) {
    let text = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key))
            .map(|v| v.trim_matches('"').to_owned())
            .unwrap_or_default()
    };
    (field("NAME="), field("VERSION_ID="))
}

/// Microseconds of a `struct timeval`.
pub(crate) fn usec(v: Suseconds) -> i64 {
    v
}
