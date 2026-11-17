//! Test-only failure injection for system calls, with no hook in the engine: a seccomp filter
//! in user-notification mode sends the chosen system calls of the filtered threads to a
//! supervisor thread, which answers each one either "run it" (`SECCOMP_USER_NOTIF_FLAG_CONTINUE`)
//! or "fail it with this errno" without running it, following the rules armed at that moment.
//!
//! [`install`] must run on the thread that later starts the engine, before it starts: the
//! filter applies to that thread and to every thread it creates afterwards (the other shards,
//! the syscall thread), not to the threads that already exist, among them the supervisor, which
//! [`install`] starts first so that it never waits on itself. A filter cannot be removed, so a
//! program that installs one runs each case in a process of its own. Unarmed calls run as usual
//! (each one costs a round trip to the supervisor).
//!
//! The kernel needs seccomp user notifications with `CONTINUE` (Linux 5.5) and either
//! `CAP_SYS_ADMIN` or `no_new_privs`; [`install`] sets `no_new_privs` for the calling thread.
#![allow(dead_code)]
#![allow(unsafe_code)]

use std::os::raw::{c_int, c_long, c_ulong};
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// System call numbers of the target.
#[cfg(target_arch = "aarch64")]
pub mod nr {
    pub const IO_SETUP: i64 = 0;
    pub const IO_SUBMIT: i64 = 2;
    pub const IO_GETEVENTS: i64 = 4;
    pub const EVENTFD2: i64 = 19;
    pub const EPOLL_CTL: i64 = 21;
    pub const IOCTL: i64 = 29;
    pub const FSTATFS: i64 = 44;
    pub const FTRUNCATE: i64 = 46;
    pub const FALLOCATE: i64 = 47;
    pub const OPENAT: i64 = 56;
    pub const CLOSE: i64 = 57;
    pub const GETDENTS64: i64 = 61;
    pub const LSEEK: i64 = 62;
    pub const READ: i64 = 63;
    pub const WRITE: i64 = 64;
    pub const WRITEV: i64 = 66;
    pub const READV: i64 = 65;
    pub const CLONE3: i64 = 435;
    pub const PREAD64: i64 = 67;
    pub const PWRITE64: i64 = 68;
    pub const FSTAT: i64 = 80;
    pub const FSYNC: i64 = 82;
    pub const FDATASYNC: i64 = 83;
    pub const SOCKET: i64 = 198;
    pub const BIND: i64 = 200;
    pub const LISTEN: i64 = 201;
    pub const ACCEPT: i64 = 202;
    pub const CONNECT: i64 = 203;
    pub const GETSOCKNAME: i64 = 204;
    pub const GETPEERNAME: i64 = 205;
    pub const SENDTO: i64 = 206;
    pub const RECVFROM: i64 = 207;
    pub const SETSOCKOPT: i64 = 208;
    pub const GETSOCKOPT: i64 = 209;
    pub const SHUTDOWN: i64 = 210;
    pub const SENDMSG: i64 = 211;
    pub const RECVMSG: i64 = 212;
    pub const MMAP: i64 = 222;
    pub const MADVISE: i64 = 233;
    pub const ACCEPT4: i64 = 242;
    pub const RENAMEAT2: i64 = 276;
    pub const SECCOMP: i64 = 277;
    pub const STATX: i64 = 291;
    pub const IO_PGETEVENTS: i64 = 292;
    pub const IO_URING_SETUP: i64 = 425;
    pub const IO_URING_ENTER: i64 = 426;
    pub const IO_URING_REGISTER: i64 = 427;
    pub const SCHED_SETSCHEDULER: i64 = 119;
    pub const EPOLL_PWAIT: i64 = 22;
}

/// System call numbers of the target.
#[cfg(target_arch = "x86_64")]
pub mod nr {
    pub const IO_SETUP: i64 = 206;
    pub const IO_SUBMIT: i64 = 209;
    pub const IO_GETEVENTS: i64 = 208;
    pub const EVENTFD2: i64 = 290;
    pub const EPOLL_CTL: i64 = 233;
    pub const IOCTL: i64 = 16;
    pub const FSTATFS: i64 = 138;
    pub const FTRUNCATE: i64 = 77;
    pub const FALLOCATE: i64 = 285;
    pub const OPENAT: i64 = 257;
    pub const CLOSE: i64 = 3;
    pub const GETDENTS64: i64 = 217;
    pub const LSEEK: i64 = 8;
    pub const READ: i64 = 0;
    pub const WRITE: i64 = 1;
    pub const WRITEV: i64 = 20;
    pub const READV: i64 = 19;
    pub const CLONE3: i64 = 435;
    pub const PREAD64: i64 = 17;
    pub const PWRITE64: i64 = 18;
    pub const FSTAT: i64 = 5;
    pub const FSYNC: i64 = 74;
    pub const FDATASYNC: i64 = 75;
    pub const SOCKET: i64 = 41;
    pub const BIND: i64 = 49;
    pub const LISTEN: i64 = 50;
    pub const ACCEPT: i64 = 43;
    pub const CONNECT: i64 = 42;
    pub const GETSOCKNAME: i64 = 51;
    pub const GETPEERNAME: i64 = 52;
    pub const SENDTO: i64 = 44;
    pub const RECVFROM: i64 = 45;
    pub const SETSOCKOPT: i64 = 54;
    pub const GETSOCKOPT: i64 = 55;
    pub const SHUTDOWN: i64 = 48;
    pub const SENDMSG: i64 = 46;
    pub const RECVMSG: i64 = 47;
    pub const MMAP: i64 = 9;
    pub const MADVISE: i64 = 28;
    pub const ACCEPT4: i64 = 288;
    pub const RENAMEAT2: i64 = 316;
    pub const SECCOMP: i64 = 317;
    pub const STATX: i64 = 332;
    pub const IO_PGETEVENTS: i64 = 333;
    pub const IO_URING_SETUP: i64 = 425;
    pub const IO_URING_ENTER: i64 = 426;
    pub const IO_URING_REGISTER: i64 = 427;
    pub const SCHED_SETSCHEDULER: i64 = 144;
    pub const EPOLL_PWAIT: i64 = 281;
}

#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;

/// A predicate over the six arguments of an intercepted call. It runs on the supervisor
/// thread while the calling thread waits in the kernel, so it may read memory the arguments
/// point to. It must not allocate or make system calls the filter intercepts.
pub type Matcher = fn(&[u64; 6]) -> bool;

/// A value matchers may compare against (a descriptor, a context), set by the test.
pub static TARGET: AtomicU64 = AtomicU64::new(u64::MAX);

const SLOTS: usize = 16;

struct Slot {
    nr: AtomicI64,
    matcher: AtomicUsize,
    errno: AtomicI32,
    skip: AtomicU32,
    left: AtomicU32,
    hits: AtomicU32,
    seen: AtomicU32,
}

#[allow(clippy::declare_interior_mutable_const)]
const FREE: Slot = Slot {
    nr: AtomicI64::new(-1),
    matcher: AtomicUsize::new(0),
    errno: AtomicI32::new(0),
    skip: AtomicU32::new(0),
    left: AtomicU32::new(0),
    hits: AtomicU32::new(0),
    seen: AtomicU32::new(0),
};

static RULES: [Slot; SLOTS] = [FREE; SLOTS];

/// An armed rule, see [`arm`].
#[derive(Clone, Copy, Debug)]
pub struct Rule(usize);

/// Every matching call fails, without limit.
pub const ALWAYS: u32 = u32::MAX;

/// Makes calls of `nr` for which `matcher` holds (all, without one) fail with `errno`: the
/// first `skip` matching calls run, the next `count` fail, the later ones run again.
pub fn arm(nr: i64, matcher: Option<Matcher>, errno: i32, skip: u32, count: u32) -> Rule {
    for (i, s) in RULES.iter().enumerate() {
        if s.nr.load(Ordering::Acquire) == -1 {
            s.matcher.store(matcher.map_or(0, |m| m as usize), Ordering::Relaxed);
            s.errno.store(errno, Ordering::Relaxed);
            s.skip.store(skip, Ordering::Relaxed);
            s.left.store(count, Ordering::Relaxed);
            s.hits.store(0, Ordering::Relaxed);
            s.seen.store(0, Ordering::Relaxed);
            s.nr.store(nr, Ordering::Release);
            return Rule(i);
        }
    }
    panic!("no free fault rule");
}

impl Rule {
    /// The calls this rule failed.
    pub fn hits(&self) -> u32 {
        RULES[self.0].hits.load(Ordering::Acquire)
    }

    /// The matching calls seen, failed or not.
    pub fn seen(&self) -> u32 {
        RULES[self.0].seen.load(Ordering::Acquire)
    }

    /// Stops the rule; its slot may be armed again.
    pub fn disarm(self) -> u32 {
        let s = &RULES[self.0];
        s.left.store(0, Ordering::Release);
        let hits = s.hits.load(Ordering::Acquire);
        s.nr.store(-1, Ordering::Release);
        hits
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SeccompData {
    nr: c_int,
    arch: u32,
    instruction_pointer: u64,
    args: [u64; 6],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SeccompNotif {
    id: u64,
    pid: u32,
    flags: u32,
    data: SeccompData,
}

#[repr(C)]
struct SeccompNotifResp {
    id: u64,
    val: i64,
    error: i32,
    flags: u32,
}

#[repr(C)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

unsafe extern "C" {
    fn syscall(num: c_long, ...) -> c_long;
    fn prctl(option: c_int, ...) -> c_int;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn __errno_location() -> *mut c_int;
}

const PR_SET_NO_NEW_PRIVS: c_int = 38;
const SECCOMP_SET_MODE_FILTER: c_long = 1;
const SECCOMP_FILTER_FLAG_NEW_LISTENER: c_long = 1 << 3;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;
const SECCOMP_USER_NOTIF_FLAG_CONTINUE: u32 = 1;
const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JEQ_K: u16 = 0x15;
const BPF_RET_K: u16 = 0x06;
// _IOWR('!', 0, struct seccomp_notif) and _IOWR('!', 1, struct seccomp_notif_resp).
const NOTIF_RECV: c_ulong =
    0xC000_0000 | ((std::mem::size_of::<SeccompNotif>() as c_ulong) << 16) | 0x2100;
const NOTIF_SEND: c_ulong =
    0xC000_0000 | ((std::mem::size_of::<SeccompNotifResp>() as c_ulong) << 16) | 0x2101;
const EINTR: c_int = 4;
const ENOENT: c_int = 2;

static LISTENER: AtomicI32 = AtomicI32::new(-1);

fn errno() -> c_int {
    // SAFETY: the calling thread's errno.
    unsafe { *__errno_location() }
}

/// The supervisor's answer to one call: `Some(errno)` to fail it.
fn decide(n: &SeccompNotif) -> Option<i32> {
    for s in RULES.iter() {
        if s.nr.load(Ordering::Acquire) != n.data.nr as i64 {
            continue;
        }
        let m = s.matcher.load(Ordering::Relaxed);
        if m != 0 {
            // SAFETY: only `arm` stores into `matcher`, and it stores a `Matcher`.
            let f: Matcher = unsafe { std::mem::transmute::<usize, Matcher>(m) };
            if !f(&n.data.args) {
                continue;
            }
        }
        s.seen.fetch_add(1, Ordering::AcqRel);
        if s.skip.load(Ordering::Acquire) > 0 {
            s.skip.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        let left = s.left.load(Ordering::Acquire);
        if left == 0 {
            // Spent: a later rule for the same call may still apply.
            continue;
        }
        if left != ALWAYS {
            s.left.fetch_sub(1, Ordering::AcqRel);
        }
        s.hits.fetch_add(1, Ordering::AcqRel);
        return Some(s.errno.load(Ordering::Relaxed));
    }
    None
}

fn supervise() {
    let fd = loop {
        let fd = LISTENER.load(Ordering::Acquire);
        if fd >= 0 {
            break fd;
        }
        std::thread::park();
    };
    loop {
        // SAFETY: all-zero bytes is the value the kernel requires on receive.
        let mut n: SeccompNotif = unsafe { std::mem::zeroed() };
        // SAFETY: `n` is the buffer the request takes.
        if unsafe { ioctl(fd, NOTIF_RECV, &mut n as *mut SeccompNotif) } != 0 {
            let e = errno();
            if e == EINTR || e == ENOENT {
                continue;
            }
            return;
        }
        let resp = match decide(&n) {
            Some(e) => SeccompNotifResp { id: n.id, val: 0, error: -e, flags: 0 },
            None => SeccompNotifResp {
                id: n.id,
                val: 0,
                error: 0,
                flags: SECCOMP_USER_NOTIF_FLAG_CONTINUE,
            },
        };
        // SAFETY: `resp` is the buffer the request takes. A call interrupted meanwhile
        // (ENOENT) is sent again by the kernel; nothing to do.
        unsafe { ioctl(fd, NOTIF_SEND, &resp as *const SeccompNotifResp) };
    }
}

/// Sends the calls `syscalls` of this thread, and of the threads it creates from now on, to
/// the supervisor. Panics when the kernel refuses the filter.
pub fn install(syscalls: &[i64]) {
    assert!(LISTENER.load(Ordering::Acquire) < 0, "a fault filter is already installed");
    let sup = std::thread::Builder::new()
        .name("fault-supervisor".into())
        .spawn(supervise)
        .expect("the supervisor thread");
    let n = syscalls.len();
    assert!(n < 200);
    let mut prog = vec![
        SockFilter { code: BPF_LD_W_ABS, jt: 0, jf: 0, k: 4 },
        SockFilter { code: BPF_JEQ_K, jt: 0, jf: (n + 1) as u8, k: AUDIT_ARCH },
        SockFilter { code: BPF_LD_W_ABS, jt: 0, jf: 0, k: 0 },
    ];
    for (i, &s) in syscalls.iter().enumerate() {
        prog.push(SockFilter { code: BPF_JEQ_K, jt: (n - i) as u8, jf: 0, k: s as u32 });
    }
    prog.push(SockFilter { code: BPF_RET_K, jt: 0, jf: 0, k: SECCOMP_RET_ALLOW });
    prog.push(SockFilter { code: BPF_RET_K, jt: 0, jf: 0, k: SECCOMP_RET_USER_NOTIF });
    let fprog = SockFprog { len: prog.len() as u16, filter: prog.as_ptr() };
    // SAFETY: plain prctl and seccomp calls; `fprog` points to a live program.
    let fd = unsafe {
        assert_eq!(
            prctl(PR_SET_NO_NEW_PRIVS, 1 as c_ulong, 0 as c_ulong, 0 as c_ulong, 0 as c_ulong),
            0
        );
        syscall(
            nr::SECCOMP as c_long,
            SECCOMP_SET_MODE_FILTER,
            SECCOMP_FILTER_FLAG_NEW_LISTENER,
            &fprog as *const SockFprog,
        )
    };
    assert!(fd >= 0, "seccomp listener: errno {}", errno());
    LISTENER.store(fd as i32, Ordering::Release);
    sup.thread().unpark();
}

/// Reads a `T` at address `p` of this process (the arguments of an intercepted call point
/// into the caller's memory, which is ours); null gives `None`.
///
/// # Safety
/// `p` is null or points to a readable `T`.
pub unsafe fn peek<T: Copy>(p: u64) -> Option<T> {
    if p == 0 {
        return None;
    }
    // SAFETY: the caller's contract.
    Some(unsafe { std::ptr::read_unaligned(p as *const T) })
}
