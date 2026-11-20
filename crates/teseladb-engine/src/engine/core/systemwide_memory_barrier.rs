//! Process-wide memory barriers.
//!
//! A sleeping reactor publishes "I am asleep" with a plain store and other shards read it
//! with plain loads; to make that cheap on the fast path, the sleeping side issues a barrier
//! that forces every thread of the process to execute a full memory barrier. The preferred
//! mechanism is the Linux `membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED)` system call; when it
//! is unavailable, a fallback dirties and then `madvise(MADV_DONTNEED)`s a private page, which
//! makes the kernel send a TLB-shootdown IPI to every CPU running the process.

use std::ffi::c_void;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::engine::core::reactor_backend::{engine_assert, errno};

use crate::engine::core::internal::linux_abi::nr::MEMBARRIER as SYS_MEMBARRIER;
const MEMBARRIER_CMD_QUERY: i32 = 0;
const MEMBARRIER_CMD_PRIVATE_EXPEDITED: i32 = 1 << 3;
const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: i32 = 1 << 4;
const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_PRIVATE: i32 = 2;
const MAP_ANONYMOUS: i32 = 0x20;
const MAP_FAILED: *mut c_void = !0usize as *mut c_void;
const MADV_DONTNEED: i32 = 4;
const EPERM: i32 = 1;

unsafe extern "C" {
    fn syscall(num: i64, ...) -> i64;
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64)
    -> *mut c_void;
    fn munlock(addr: *const c_void, len: usize) -> i32;
    fn madvise(addr: *mut c_void, len: usize, advice: i32) -> i32;
    fn getpagesize() -> i32;
}

static HAS_NATIVE_MEMBARRIER: LazyLock<bool> = LazyLock::new(|| {
    // SAFETY: plain membarrier system calls.
    let r = unsafe { syscall(SYS_MEMBARRIER, MEMBARRIER_CMD_QUERY, 0) };
    if r == -1 {
        return false;
    }
    let needed =
        (MEMBARRIER_CMD_PRIVATE_EXPEDITED | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED) as i64;
    if (r & needed) != needed {
        return false;
    }
    // SAFETY: as above.
    unsafe { syscall(SYS_MEMBARRIER, MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED, 0) };
    true
});

/// Process-wide barriers issued since the process started, by either way.
static ISSUED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many process-wide memory barriers the process has issued. A reactor that goes to sleep
/// issues none: each interrupts every thread of the process.
#[must_use]
pub fn barriers_issued() -> u64 {
    ISSUED.load(Ordering::Relaxed)
}

#[inline]
fn try_native_membarrier() -> bool {
    if *HAS_NATIVE_MEMBARRIER {
        ISSUED.fetch_add(1, Ordering::Relaxed);
        // SAFETY: plain membarrier system call.
        unsafe { syscall(SYS_MEMBARRIER, MEMBARRIER_CMD_PRIVATE_EXPEDITED, 0) };
        return true;
    }
    false
}

thread_local! {
    static MEM: *mut u8 = {
        // SAFETY: anonymous private mapping of one page.
        let mem = unsafe { mmap(std::ptr::null_mut(), getpagesize() as usize, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) };
        engine_assert!(mem != MAP_FAILED);
        // If the user specified --lock-memory, then madvise() below will fail
        // with EINVAL, so we unlock here:
        // SAFETY: unlocking the page mapped above.
        let r = unsafe { munlock(mem, getpagesize() as usize) };
        // munlock may fail on old kernels if we don't have permission. That's not
        // a problem, since if we don't have permission to unlock, we didn't have
        // permissions to lock.
        engine_assert!(r == 0 || errno() == EPERM);
        mem.cast::<u8>()
    };
}

/// Causes every thread of the process to execute a full memory barrier.
///
/// Uses `membarrier` when the kernel supports it, otherwise the page-eviction fallback
/// described in the module docs. This is a system call with a process-wide cost: call it on
/// slow paths only (for example before a reactor goes to sleep).
///
/// # Panics
///
/// Panics if the fallback's `madvise` call fails.
pub fn systemwide_memory_barrier() {
    if try_native_membarrier() {
        return;
    }

    ISSUED.fetch_add(1, Ordering::Relaxed);
    // FIXME: use sys_membarrier() when available
    let mem = MEM.with(|m| *m);
    // Force page into memory to make madvise() have real work to do
    // SAFETY: `mem` is this thread's private page.
    unsafe { std::ptr::write_volatile(mem, 3) };
    // Evict page to force kernel to send IPI to all threads, with
    // a side effect of executing a memory barrier on those threads
    // FIXME: does this work on ARM?
    // SAFETY: as above.
    let r2 = unsafe { madvise(mem.cast(), getpagesize() as usize, MADV_DONTNEED) };
    engine_assert!(r2 == 0);
}

#[repr(C, align(64))]
struct AlignedFlag {
    flag: AtomicBool,
}

impl AlignedFlag {
    #[inline]
    fn try_lock(&self) -> bool {
        !self.flag.swap(true, Ordering::Relaxed)
    }
    #[inline]
    fn unlock(&self) {
        self.flag.store(false, Ordering::Relaxed);
    }
}

static MEMBARRIER_LOCK: AlignedFlag = AlignedFlag { flag: AtomicBool::new(false) };

/// Attempts a process-wide memory barrier without blocking.
///
/// Returns `true` if the barrier was executed and `false` if another thread is issuing one
/// right now (calling the kernel would make this thread wait on a kernel mutex) or, on
/// aarch64, if `membarrier` is unavailable (the page-eviction fallback is not reliable there;
/// a warning is logged once). A caller that gets `false` must not rely on the barrier and
/// should keep polling instead of sleeping.
pub fn try_systemwide_memory_barrier() -> bool {
    // In 944d5fe50f3f, Linux started serializing membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED)
    // calls. This means that if all reactors enter sleep mode at the same time, they will sleep
    // on a kernel mutex while doing so. While they wait on the mutex, they cannot be woken.
    //
    // To fix this, only we serialize membarrier calls ourselves, but instead of sleeping, we just
    // return to the reactor poll loop. If an event is ready, we will wake up immediately.
    if !MEMBARRIER_LOCK.try_lock() {
        return false;
    }
    let _unlock = crate::engine::util::defer::defer(|| MEMBARRIER_LOCK.unlock());
    if try_native_membarrier() {
        return true;
    }
    #[cfg(target_arch = "aarch64")]
    {
        // Some (not all) ARM processors can broadcast TLB invalidations using the
        // TLBI instruction. On those, the mprotect trick won't work.
        static WARN_ONCE: std::sync::Once = std::sync::Once::new();
        WARN_ONCE.call_once(|| {
            crate::engine::core::reactor::engine_logger().warn(crate::engine::util::log::FormatInfo::new("membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED) is not available, reactor will not sleep when idle. Upgrade to Linux 4.14 or later"), format_args!(
                "membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED) is not available, reactor will not sleep when idle. Upgrade to Linux 4.14 or later"
            ));
        });
        false
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        systemwide_memory_barrier();
        true
    }
}
