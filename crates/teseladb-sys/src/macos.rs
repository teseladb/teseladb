//! macOS: `fcntl(F_PREALLOCATE)` + `ftruncate`.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

pub(crate) const ENOSPC: i32 = 28;

const F_PREALLOCATE: i32 = 42;
const F_ALLOCATECONTIG: u32 = 0x2;
const F_ALLOCATEALL: u32 = 0x4;
const F_PEOFPOSMODE: i32 = 3;

/// `fstore_t` from `<sys/fcntl.h>`.
#[repr(C)]
struct FStore {
    flags: u32,
    posmode: i32,
    offset: i64,
    length: i64,
    bytesalloc: i64,
}

unsafe extern "C" {
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
}

pub(crate) fn preallocate(file: &File, len: u64) -> io::Result<()> {
    let current = file.metadata()?.len();
    if len <= current {
        return Ok(());
    }
    let extra =
        i64::try_from(len - current).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut st = FStore {
        flags: F_ALLOCATECONTIG | F_ALLOCATEALL,
        posmode: F_PEOFPOSMODE,
        offset: 0,
        length: extra,
        bytesalloc: 0,
    };
    // SAFETY: `st` is a valid `fstore_t` that lives during the call; the descriptor is valid.
    let mut rc = unsafe { fcntl(file.as_raw_fd(), F_PREALLOCATE, &mut st as *mut FStore) };
    if rc == -1 {
        // No contiguous space: retry without requiring contiguity.
        st.flags = F_ALLOCATEALL;
        // SAFETY: same as above.
        rc = unsafe { fcntl(file.as_raw_fd(), F_PREALLOCATE, &mut st as *mut FStore) };
    }
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    file.set_len(len)
}

// ----- durability -----

/// `F_FULLFSYNC` of `<sys/fcntl.h>`: flush the file to the drive and then the drive's cache.
const F_FULLFSYNC: i32 = 51;

unsafe extern "C" {
    fn fsync(fd: i32) -> i32;
}

/// `fsync(2)`: the file's data and metadata go to the drive, which may still hold them in its
/// cache.
pub(crate) fn push_to_drive(file: &File) -> io::Result<()> {
    // SAFETY: the descriptor is valid while `file` lives; `fsync` touches no memory of the caller.
    if unsafe { fsync(file.as_raw_fd()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Sends what the file has written to the drive without asking it to flush its cache: on macOS,
/// [`push_to_drive`].
pub(crate) fn write_behind(file: &File) -> io::Result<()> {
    push_to_drive(file)
}

/// `QOS_CLASS_USER_INTERACTIVE` of `<sys/qos.h>`.
const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

unsafe extern "C" {
    fn pthread_set_qos_class_self_np(class: u32, relative_priority: i32) -> i32;
}

pub(crate) fn favor_current_thread() -> io::Result<()> {
    // SAFETY: it changes the calling thread's class and touches no memory of the caller.
    let rc = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(())
}

/// [`push_to_drive`] by descriptor: `fd` is open in another thread that waits for this push.
pub(crate) fn push_descriptor(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fsync` takes a descriptor and touches no memory of the caller; a descriptor that
    // is not open fails with `EBADF`, which is returned.
    if unsafe { fsync(fd) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `fcntl(F_FULLFSYNC)`: the file goes to the drive and the drive writes its whole cache to
/// permanent storage, so everything any file sent to that drive before is durable.
pub(crate) fn flush_drive(file: &File) -> io::Result<()> {
    // SAFETY: the descriptor is valid while `file` lives; `F_FULLFSYNC` takes no argument.
    if unsafe { fcntl(file.as_raw_fd(), F_FULLFSYNC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ----- kqueue -----

use std::os::fd::{AsFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::Duration;

use crate::poll::{Event, Interest, WAKE_TOKEN};

const EVFILT_READ: i16 = -1;
const EVFILT_WRITE: i16 = -2;
const EVFILT_USER: i16 = -10;
const EV_ADD: u16 = 0x0001;
const EV_DELETE: u16 = 0x0002;
const EV_CLEAR: u16 = 0x0020;
const EV_RECEIPT: u16 = 0x0040;
const EV_ERROR: u16 = 0x4000;
const EV_EOF: u16 = 0x8000;
const NOTE_TRIGGER: u32 = 0x0100_0000;
const ENOENT: i64 = 2;
const EINTR: i32 = 4;
/// Events per `kevent` call.
const MAX_EVENTS: usize = 256;

/// `struct kevent` from `<sys/event.h>` (64-bit).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct KEvent {
    ident: usize,
    filter: i16,
    flags: u16,
    fflags: u32,
    data: isize,
    udata: usize,
}

/// `struct timespec`.
#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

unsafe extern "C" {
    fn kqueue() -> i32;
    fn kevent(
        kq: i32,
        changelist: *const KEvent,
        nchanges: i32,
        eventlist: *mut KEvent,
        nevents: i32,
        timeout: *const Timespec,
    ) -> i32;
}

fn change(ident: usize, filter: i16, flags: u16, fflags: u32, udata: usize) -> KEvent {
    KEvent { ident, filter, flags, fflags, data: 0, udata }
}

/// Applies changes with `EV_RECEIPT` (each one comes back with its result in `data`). Returns the
/// per-change errors, in order (0 = success).
fn apply(kq: RawFd, changes: &mut [KEvent]) -> std::io::Result<Vec<i64>> {
    for c in changes.iter_mut() {
        c.flags |= EV_RECEIPT;
    }
    let n = changes.len() as i32;
    let zero = Timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `changes` is a valid buffer of `n` `kevent`s used as input and output (with
    // `EV_RECEIPT` the kernel writes one result per change); `zero` lives during the call.
    let rc = unsafe { kevent(kq, changes.as_ptr(), n, changes.as_mut_ptr(), n, &zero) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(changes[..rc as usize]
        .iter()
        // With `EV_RECEIPT` every returned entry carries `EV_ERROR` and its result in `data`.
        .map(|c| c.data as i64)
        .collect())
}

fn first_error(errs: &[i64], ignore: i64) -> std::io::Result<()> {
    match errs.iter().find(|&&e| e != 0 && e != ignore) {
        Some(&e) => Err(std::io::Error::from_raw_os_error(e as i32)),
        None => Ok(()),
    }
}

#[derive(Debug)]
pub(crate) struct Poller {
    kq: Arc<OwnedFd>,
}

#[derive(Clone, Debug)]
pub(crate) struct Waker {
    kq: Arc<OwnedFd>,
}

/// Takes ownership of a descriptor a system call has just returned, or of its error if negative.
fn owned(fd: RawFd) -> std::io::Result<OwnedFd> {
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the system call has just returned this descriptor and nobody else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn raw(fd: &OwnedFd) -> RawFd {
    use std::os::fd::AsRawFd;
    fd.as_fd().as_raw_fd()
}

impl Poller {
    pub(crate) fn new() -> std::io::Result<Poller> {
        // SAFETY: no arguments.
        let kq = Arc::new(owned(unsafe { kqueue() })?);
        let mut c = [change(0, EVFILT_USER, EV_ADD | EV_CLEAR, 0, 0)];
        first_error(&apply(raw(&kq), &mut c)?, 0)?;
        Ok(Poller { kq })
    }

    pub(crate) fn register(&self, fd: RawFd, token: u64, i: Interest) -> std::io::Result<()> {
        self.modify(fd, token, i)
    }

    pub(crate) fn modify(&self, fd: RawFd, token: u64, i: Interest) -> std::io::Result<()> {
        if fd < 0 {
            return Err(std::io::Error::from_raw_os_error(9));
        }
        let id = fd as usize;
        let t = token as usize;
        let flag = |on: bool| if on { EV_ADD } else { EV_DELETE };
        let mut c = [
            change(id, EVFILT_READ, flag(i.read), 0, t),
            change(id, EVFILT_WRITE, flag(i.write), 0, t),
        ];
        // Removing a filter that was not there is not an error.
        first_error(&apply(raw(&self.kq), &mut c)?, ENOENT)
    }

    pub(crate) fn deregister(&self, fd: RawFd) -> std::io::Result<()> {
        let id = fd as usize;
        let mut c =
            [change(id, EVFILT_READ, EV_DELETE, 0, 0), change(id, EVFILT_WRITE, EV_DELETE, 0, 0)];
        let errs = apply(raw(&self.kq), &mut c)?;
        if errs.iter().all(|&e| e == ENOENT) {
            return Err(std::io::Error::from_raw_os_error(ENOENT as i32));
        }
        first_error(&errs, ENOENT)
    }

    pub(crate) fn wait(
        &self,
        out: &mut Vec<Event>,
        timeout: Option<Duration>,
    ) -> std::io::Result<()> {
        let mut evs = [KEvent::default(); MAX_EVENTS];
        let ts = timeout.map(|d| Timespec {
            tv_sec: d.as_secs().min(i64::MAX as u64) as i64,
            tv_nsec: i64::from(d.subsec_nanos()),
        });
        let tp = ts.as_ref().map_or(std::ptr::null(), |t| t as *const Timespec);
        // SAFETY: `evs` has `MAX_EVENTS` writable entries; no changes (null list with 0);
        // `tp` is null or points to `ts`, which lives during the call.
        let n = unsafe {
            kevent(raw(&self.kq), std::ptr::null(), 0, evs.as_mut_ptr(), MAX_EVENTS as i32, tp)
        };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            return if e.raw_os_error() == Some(EINTR) { Ok(()) } else { Err(e) };
        }
        for e in &evs[..n as usize] {
            if e.filter == EVFILT_USER {
                out.push(Event {
                    token: WAKE_TOKEN,
                    readable: false,
                    writable: false,
                    hangup: false,
                });
                continue;
            }
            out.push(Event {
                token: e.udata as u64,
                readable: e.filter == EVFILT_READ,
                writable: e.filter == EVFILT_WRITE,
                hangup: e.flags & (EV_EOF | EV_ERROR) != 0,
            });
        }
        Ok(())
    }

    pub(crate) fn waker(&self) -> Waker {
        Waker { kq: Arc::clone(&self.kq) }
    }
}

impl Waker {
    pub(crate) fn wake(&self) -> std::io::Result<()> {
        let mut c = [change(0, EVFILT_USER, 0, NOTE_TRIGGER, 0)];
        first_error(&apply(raw(&self.kq), &mut c)?, 0)
    }
}

pub(crate) fn pin_to_cpu(_cpu: usize) -> std::io::Result<bool> {
    // macOS cannot pin threads to CPUs (it only has affinity hints, which are not used).
    Ok(false)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    unsafe extern "C" {
        fn pthread_self() -> usize;
        fn pthread_get_qos_class_np(thread: usize, class: *mut u32, relative: *mut i32) -> i32;
    }

    /// The favored thread runs in the highest class; another thread keeps its own.
    #[test]
    fn a_favored_thread_runs_in_the_highest_class() {
        let class_of_self = || {
            let (mut class, mut relative) = (0u32, 0i32);
            // SAFETY: both pointers are to locals that live through the call.
            let rc = unsafe { pthread_get_qos_class_np(pthread_self(), &mut class, &mut relative) };
            assert_eq!(rc, 0);
            class
        };
        let other = std::thread::spawn(class_of_self).join().unwrap();
        assert_ne!(other, QOS_CLASS_USER_INTERACTIVE);
        let favored = std::thread::spawn(move || {
            favor_current_thread().unwrap();
            class_of_self()
        })
        .join()
        .unwrap();
        assert_eq!(favored, QOS_CLASS_USER_INTERACTIVE);
    }

    /// macOS counts the task's page-ins apart, and `extra_info` shows them (RefDb's
    /// `processinfo_osx.cpp`).
    #[test]
    fn the_task_counts_its_page_ins() {
        let pageins = memory().unwrap().pageins;
        assert!(pageins.is_some_and(|n| n >= 0), "{pageins:?}");
    }

    unsafe extern "C" {
        fn pthread_kill(thread: std::os::unix::thread::RawPthread, sig: i32) -> i32;
        fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
    }

    const SIGUSR1: i32 = 30;

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
        // A negative descriptor is the error of the call that returned it.
        assert!(owned(-1).is_err());
        // Changes and waits on something that is not a kqueue.
        let mut c = [change(0, EVFILT_USER, EV_ADD, 0, 0)];
        assert!(apply(-1, &mut c).is_err());
        let not_a_kqueue = File::open("/dev/null").unwrap();
        let p = Poller { kq: Arc::new(OwnedFd::from(not_a_kqueue)) };
        let err = p.wait(&mut Vec::new(), Some(Duration::ZERO)).unwrap_err();
        assert_ne!(err.raw_os_error(), Some(EINTR));
    }

    #[test]
    fn a_signal_during_the_wait_returns_without_events() {
        // SAFETY: `ignore` is a valid handler for the whole program and does nothing.
        assert_ne!(unsafe { signal(SIGUSR1, ignore) }, usize::MAX);
        // The signal may arrive before the thread enters `kevent`: then the wait lasts its whole
        // timeout and the round is repeated.
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
    fn a_file_goes_to_the_drive_and_the_drive_flushes_its_cache() {
        let path =
            std::env::temp_dir().join(format!("teseladb-sys-durable-{}", std::process::id()));
        let mut file = File::create(&path).unwrap();
        std::io::Write::write_all(&mut file, b"durable").unwrap();
        push_to_drive(&file).unwrap();
        flush_drive(&file).unwrap();
        std::fs::remove_file(&path).unwrap();
        // A descriptor that is not a file of a drive is an error of each call.
        let pipe = std::process::Command::new("true").stdout(std::process::Stdio::piped()).spawn();
        let out = pipe.unwrap().stdout.take().unwrap();
        let not_a_drive = File::from(std::os::fd::OwnedFd::from(out));
        assert!(flush_drive(&not_a_drive).is_err());
    }

    #[test]
    fn struct_layouts_match_the_system_headers() {
        assert_eq!(std::mem::size_of::<KEvent>(), 32);
        assert_eq!(std::mem::align_of::<KEvent>(), 8);
        assert_eq!(std::mem::size_of::<Timespec>(), 16);
        assert_eq!(std::mem::size_of::<FStore>(), 32);
    }
}

// Host facts (`host.rs`).

/// `suseconds_t`.
pub(crate) type Suseconds = i32;
/// `sysconf(3)` names.
pub(crate) const SC_PAGESIZE: i32 = 29;
pub(crate) const SC_PHYS_PAGES: i32 = 200;
pub(crate) const SC_NPROCESSORS_ONLN: i32 = 58;
/// `getrlimit(2)` resource of open files.
pub(crate) const RLIMIT_NOFILE: i32 = 8;
/// `ru_maxrss` is in bytes on macOS.
/// `CLOCK_THREAD_CPUTIME_ID`: the CPU time of the calling thread.
pub(crate) const CLOCK_THREAD_CPUTIME_ID: i32 = 16;
pub(crate) const MAXRSS_PER_KB: i64 = 1024;

/// Bytes of each field of `struct utsname`.
const UTS_LEN: usize = 256;

#[repr(C)]
pub(crate) struct Utsname {
    pub(crate) sysname: [u8; UTS_LEN],
    pub(crate) nodename: [u8; UTS_LEN],
    pub(crate) release: [u8; UTS_LEN],
    pub(crate) version: [u8; UTS_LEN],
    pub(crate) machine: [u8; UTS_LEN],
}

impl Default for Utsname {
    fn default() -> Utsname {
        let z = [0u8; UTS_LEN];
        Utsname { sysname: z, nodename: z, release: z, version: z, machine: z }
    }
}

#[repr(C)]
#[derive(Default)]
pub(crate) struct Statvfs {
    bsize: u64,
    frsize: u64,
    blocks: u32,
    bfree: u32,
    bavail: u32,
    files: u32,
    ffree: u32,
    favail: u32,
    fsid: u64,
    flag: u64,
    namemax: u64,
}

impl Statvfs {
    /// Blocks, free blocks and the bytes of a block.
    pub(crate) fn sizes(&self) -> (u64, u64, u64) {
        (u64::from(self.blocks), u64::from(self.bfree), self.frsize)
    }
}

/// `struct proc_taskinfo` of `<sys/proc_info.h>`.
#[repr(C)]
#[derive(Default)]
struct TaskInfo {
    virtual_size: u64,
    resident_size: u64,
    total_user: u64,
    total_system: u64,
    threads_user: u64,
    threads_system: u64,
    policy: i32,
    faults: i32,
    pageins: i32,
    cow_faults: i32,
    messages_sent: i32,
    messages_received: i32,
    syscalls_mach: i32,
    syscalls_unix: i32,
    csw: i32,
    threadnum: i32,
    numrunning: i32,
    priority: i32,
}

/// `proc_pidinfo` flavour of `struct proc_taskinfo`.
const PROC_PIDTASKINFO: i32 = 4;

unsafe extern "C" {
    fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut TaskInfo, size: i32) -> i32;
}

pub(crate) fn memory() -> io::Result<crate::host::Memory> {
    let mut t = TaskInfo::default();
    let size = i32::try_from(std::mem::size_of::<TaskInfo>()).unwrap_or(i32::MAX);
    let pid = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
    // SAFETY: `t` is a `struct proc_taskinfo` valid for writes of `size` bytes during the call.
    let n = unsafe { proc_pidinfo(pid, PROC_PIDTASKINFO, 0, &mut t, size) };
    if n != size {
        return Err(io::Error::last_os_error());
    }
    Ok(crate::host::Memory {
        resident: t.resident_size,
        virtual_size: t.virtual_size,
        threads: u64::try_from(t.threadnum).unwrap_or(0),
        pageins: Some(t.pageins),
    })
}

/// `malloc_statistics_t` of `<malloc/malloc.h>`.
#[repr(C)]
#[derive(Default)]
struct MallocStatistics {
    blocks_in_use: u32,
    size_in_use: usize,
    max_size_in_use: usize,
    size_allocated: usize,
}

unsafe extern "C" {
    fn malloc_zone_statistics(zone: *mut std::ffi::c_void, stats: *mut MallocStatistics);
    fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
}

/// The malloc of macOS has no arenas to cap.
pub(crate) fn limit_arenas(_arenas: usize) -> Option<bool> {
    None
}

/// `malloc_zone_pressure_relief` over every zone, with no goal: the free pages go back.
pub(crate) fn release_free_memory() -> Option<bool> {
    // SAFETY: a null zone asks every zone; no pointers are passed.
    Some(unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) } > 0)
}

/// Bytes in use in every malloc zone of the process.
pub(crate) fn allocated() -> Option<u64> {
    let mut s = MallocStatistics::default();
    // SAFETY: a null zone asks for every zone together; `s` is a `malloc_statistics_t` valid for
    // writes during the call.
    unsafe { malloc_zone_statistics(std::ptr::null_mut(), &mut s) };
    u64::try_from(s.size_in_use).ok()
}

/// RefDb's names of macOS in `hostInfo`: `Mac OS X` and the kernel release.
pub(crate) fn distribution() -> (String, String) {
    let release = crate::host::kernel().map(|k| k.release).unwrap_or_default();
    ("Mac OS X".to_owned(), release)
}

/// Microseconds of a `struct timeval`.
pub(crate) fn usec(v: Suseconds) -> i64 {
    i64::from(v)
}
