//! What the operating system says of the host and of this process, for the diagnostic commands
//! (`hostInfo`, `serverStatus`, `dbStats` of 006): POSIX calls common to Linux and macOS here,
//! the rest in each system's module.

use std::ffi::CStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::check_rc;
use crate::os;

/// The kernel's names (`uname(2)`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Uname {
    /// Operating system (`Linux`, `Darwin`).
    pub sysname: String,
    /// Kernel release.
    pub release: String,
    /// Kernel version string.
    pub version: String,
    /// Machine (`aarch64`, `x86_64`, `arm64`).
    pub machine: String,
}

/// The process's use of the machine (`getrusage(2)`), in microseconds and kilobytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// CPU time in user mode.
    pub user_us: i64,
    /// CPU time in system mode.
    pub system_us: i64,
    /// Largest resident set, in kilobytes.
    pub max_rss_kb: i64,
    /// Page faults served without I/O.
    pub minor_faults: i64,
    /// Page faults that needed I/O.
    pub major_faults: i64,
    /// Blocks read.
    pub in_blocks: i64,
    /// Blocks written.
    pub out_blocks: i64,
    /// Voluntary context switches.
    pub voluntary_switches: i64,
    /// Forced context switches.
    pub involuntary_switches: i64,
}

/// The process's memory and threads now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Memory {
    /// Resident bytes.
    pub resident: u64,
    /// Virtual bytes.
    pub virtual_size: u64,
    /// Threads.
    pub threads: u64,
    /// The task's page-ins, which macOS counts apart from `getrusage(2)`; `None` on Linux.
    pub pageins: Option<i32>,
}

const RUSAGE_SELF: i32 = 0;

#[repr(C)]
struct Timeval {
    sec: i64,
    // `suseconds_t`: 64 bits on Linux, 32 bits padded to 64 on macOS; the value fits in both.
    usec: os::Suseconds,
}

#[repr(C)]
struct Rusage {
    utime: Timeval,
    stime: Timeval,
    maxrss: i64,
    ixrss: i64,
    idrss: i64,
    isrss: i64,
    minflt: i64,
    majflt: i64,
    nswap: i64,
    inblock: i64,
    oublock: i64,
    msgsnd: i64,
    msgrcv: i64,
    nsignals: i64,
    nvcsw: i64,
    nivcsw: i64,
}

#[repr(C)]
struct Rlimit {
    cur: u64,
    max: u64,
}

unsafe extern "C" {
    fn gethostname(name: *mut u8, len: usize) -> i32;
    fn getrusage(who: i32, usage: *mut Rusage) -> i32;
    fn clock_gettime(clock: i32, tp: *mut ThreadTime) -> i32;
    fn getrlimit(resource: i32, rlim: *mut Rlimit) -> i32;
    fn sysconf(name: i32) -> i64;
    fn uname(buf: *mut os::Utsname) -> i32;
    fn statvfs(path: *const u8, buf: *mut os::Statvfs) -> i32;
}

/// Longest host name the call may return, with its terminating zero.
const HOSTNAME_MAX: usize = 256;

/// The host's name.
pub fn hostname() -> io::Result<String> {
    let mut buf = [0u8; HOSTNAME_MAX];
    // SAFETY: `buf` is valid for writes of its length; the call writes a terminated string there.
    check_rc(unsafe { gethostname(buf.as_mut_ptr(), buf.len() - 1) })?;
    Ok(text(&buf))
}

/// The text of a zero-terminated buffer.
fn text(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// The kernel's names.
pub fn kernel() -> io::Result<Uname> {
    let mut u = os::Utsname::default();
    // SAFETY: `u` is a `struct utsname` with this system's layout, valid for the whole call.
    check_rc(unsafe { uname(&mut u) })?;
    Ok(Uname {
        sysname: text(&u.sysname),
        release: text(&u.release),
        version: text(&u.version),
        machine: text(&u.machine),
    })
}

/// A value of `sysconf(3)`, `None` when the system has no limit or does not know it.
fn conf(name: i32) -> Option<u64> {
    // SAFETY: `sysconf` only reads its argument.
    let v = unsafe { sysconf(name) };
    u64::try_from(v).ok()
}

/// Bytes of a memory page.
pub fn page_size() -> Option<u64> {
    conf(os::SC_PAGESIZE)
}

/// Pages of physical memory.
pub fn physical_pages() -> Option<u64> {
    conf(os::SC_PHYS_PAGES)
}

/// Processors online.
pub fn online_cpus() -> Option<u64> {
    conf(os::SC_NPROCESSORS_ONLN)
}

/// Most files the process may open (`RLIMIT_NOFILE`, soft limit).
pub fn max_open_files() -> io::Result<u64> {
    let mut r = Rlimit { cur: 0, max: 0 };
    // SAFETY: `r` is a `struct rlimit` valid for writes during the call.
    check_rc(unsafe { getrlimit(os::RLIMIT_NOFILE, &mut r) })?;
    Ok(r.cur)
}

/// `struct timespec` on the 64-bit hosts TeselaDB runs on.
#[repr(C)]
struct ThreadTime {
    sec: i64,
    nsec: i64,
}

/// The CPU time the calling thread has used, in nanoseconds (`clock_gettime(2)` with
/// `CLOCK_THREAD_CPUTIME_ID`).
pub fn thread_cpu_nanos() -> io::Result<u64> {
    let mut t = ThreadTime { sec: 0, nsec: 0 };
    // SAFETY: `t` is a `struct timespec` valid for writes during the call.
    check_rc(unsafe { clock_gettime(os::CLOCK_THREAD_CPUTIME_ID, &mut t) })?;
    let nanos = i128::from(t.sec) * 1_000_000_000 + i128::from(t.nsec);
    Ok(u64::try_from(nanos).unwrap_or(0))
}

/// The process's use of the machine since it started.
pub fn usage() -> io::Result<Usage> {
    // SAFETY: an all-zero `struct rusage` is a valid value of plain integers.
    let mut r: Rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `r` is a `struct rusage` valid for writes during the call.
    check_rc(unsafe { getrusage(RUSAGE_SELF, &mut r) })?;
    let us = |t: &Timeval| t.sec * 1_000_000 + os::usec(t.usec);
    Ok(Usage {
        user_us: us(&r.utime),
        system_us: us(&r.stime),
        max_rss_kb: r.maxrss / os::MAXRSS_PER_KB,
        minor_faults: r.minflt,
        major_faults: r.majflt,
        in_blocks: r.inblock,
        out_blocks: r.oublock,
        voluntary_switches: r.nvcsw,
        involuntary_switches: r.nivcsw,
    })
}

/// The process's memory and threads now.
pub fn memory() -> io::Result<Memory> {
    os::memory()
}

/// Bytes the process's allocator holds for live allocations (012 FR-016): what `malloc` handed
/// out and nobody freed yet. `None` where the system does not say (glibc before 2.33).
pub fn allocated() -> Option<u64> {
    os::allocated()
}

/// Caps the arenas of glibc's allocator at `arenas` (`mallopt(M_ARENA_MAX)`, 012 FR-022), to be
/// called before the threads that allocate start: by default glibc makes up to eight per CPU.
/// `None` where the C library has no such setting (musl, macOS), else whether it took the value.
pub fn limit_allocator_arenas(arenas: usize) -> Option<bool> {
    os::limit_arenas(arenas)
}

/// Hands back to the system the free memory the allocator keeps (`malloc_trim(0)` with glibc,
/// `malloc_zone_pressure_relief` on macOS; 012 FR-022). `None` where the C library cannot, else
/// whether any memory went back.
pub fn release_free_memory() -> Option<bool> {
    os::release_free_memory()
}

/// Bytes used and total of the file system that holds `path`.
pub fn fs_usage(path: &Path) -> io::Result<(u64, u64)> {
    let mut c = path.as_os_str().as_bytes().to_vec();
    if c.contains(&0) {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    c.push(0);
    let name = CStr::from_bytes_with_nul(&c).map_err(|_| io::ErrorKind::InvalidInput)?;
    let mut s = os::Statvfs::default();
    // SAFETY: `name` is a terminated path and `s` a `struct statvfs` of this system's layout,
    // both valid during the call.
    check_rc(unsafe { statvfs(name.as_ptr().cast(), &mut s) })?;
    let (blocks, free, unit) = s.sizes();
    Ok((blocks.saturating_sub(free).saturating_mul(unit), blocks.saturating_mul(unit)))
}

/// The operating system's distribution name and version (`os` of `hostInfo`).
pub fn distribution() -> (String, String) {
    os::distribution()
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_host_answers_with_plausible_values() {
        assert!(!hostname().unwrap().is_empty());
        let k = kernel().unwrap();
        assert!(!k.sysname.is_empty() && !k.release.is_empty() && !k.machine.is_empty());
        let page = page_size().unwrap();
        assert!(page.is_power_of_two() && page >= 4096);
        assert!(physical_pages().unwrap() * page >= 1 << 28);
        assert!(online_cpus().unwrap() >= 1);
        assert!(max_open_files().unwrap() >= 16);
        let (name, version) = distribution();
        assert!(!name.is_empty() && !version.is_empty());
    }

    #[test]
    fn the_thread_clock_counts_only_work() {
        let before = thread_cpu_nanos().unwrap();
        let mut x = 0u64;
        for i in 0..2_000_000u64 {
            x = x.wrapping_add(std::hint::black_box(i * i));
        }
        assert_ne!(x, 1);
        let worked = thread_cpu_nanos().unwrap() - before;
        assert!(worked > 0);
        // Sleeping takes no CPU time of the thread.
        let before = thread_cpu_nanos().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(thread_cpu_nanos().unwrap() - before < 20_000_000);
    }

    #[test]
    fn the_process_reports_its_use() {
        // Some CPU time to measure.
        let mut x = 0u64;
        for i in 0..2_000_000u64 {
            x = x.wrapping_add(i * i);
        }
        assert_ne!(x, 1);
        let u = usage().unwrap();
        assert!(u.user_us + u.system_us > 0);
        assert!(u.max_rss_kb > 0);
        let m = memory().unwrap();
        assert!(m.resident > 0 && m.virtual_size >= m.resident && m.threads >= 1);
    }

    #[test]
    fn file_system_sizes_and_bad_paths() {
        let (used, total) = fs_usage(&std::env::temp_dir()).unwrap();
        assert!(total > 0 && used <= total);
        assert!(fs_usage(Path::new("/no/such/dir/at/all")).is_err());
        let bad = std::ffi::OsStr::from_bytes(b"a\0b");
        assert_eq!(fs_usage(Path::new(bad)).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(text(b"abc"), "abc");
        assert_eq!(text(b"ab\0c"), "ab");
    }

    #[test]
    fn the_allocator_takes_an_arena_cap_and_gives_back_free_memory_where_its_library_can() {
        let glibc = cfg!(all(target_os = "linux", target_env = "gnu"));
        assert_eq!(limit_allocator_arenas(4), glibc.then_some(true));
        assert_eq!(limit_allocator_arenas(0), glibc.then_some(false), "glibc refuses no arena");
        // Memory freed by another thread stays in its arena until it is handed back.
        let freed =
            std::thread::spawn(|| drop((0..64).map(|_| vec![7u8; 1 << 20]).collect::<Vec<_>>()));
        freed.join().unwrap();
        let back = release_free_memory();
        assert_eq!(back.is_some(), glibc || cfg!(target_os = "macos"), "{back:?}");
    }

    #[test]
    fn the_allocator_counts_what_is_held() {
        let Some(before) = allocated() else { return };
        let held: Vec<u8> = vec![1; 64 << 20];
        let after = allocated().expect("the allocator answered once");
        assert!(after >= before + (32 << 20), "{before} then {after}");
        drop(held);
    }
}
