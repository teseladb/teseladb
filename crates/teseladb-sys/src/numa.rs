//! CPUs and NUMA nodes (012 FR-006): which CPUs the process may use (its affinity, which a
//! cgroup's cpuset narrows), the NUMA node of each CPU, the `--cpu-list` syntax, and binding a
//! thread's future allocations to the node of the CPU it runs on.

use std::io;
use std::path::Path;

/// Parses a CPU list as Linux writes them (`0-3,8,10-11`): the CPUs in the order written,
/// without repeats.
pub fn parse_cpu_list(s: &str) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for part in s.split(',').map(str::trim) {
        let (lo, hi) = match part.split_once('-') {
            Some((a, b)) => (a.trim(), b.trim()),
            None => (part, part),
        };
        let num = |x: &str| x.parse::<usize>().map_err(|_| format!("bad CPU list item {part:?}"));
        let (lo, hi) = (num(lo)?, num(hi)?);
        if lo > hi {
            return Err(format!("bad CPU range {part:?}"));
        }
        if hi >= MAX_CPUS {
            return Err(format!("CPU {hi} beyond {}", MAX_CPUS - 1));
        }
        for c in lo..=hi {
            if !out.contains(&c) {
                out.push(c);
            }
        }
    }
    Ok(out)
}

/// CPUs an affinity mask can name (`CPU_SETSIZE`).
pub const MAX_CPUS: usize = 1024;

/// The CPUs this process may run on, in increasing order: its affinity mask on Linux (a cpuset
/// narrows it), every online CPU on macOS.
pub fn allowed_cpus() -> io::Result<Vec<usize>> {
    os_allowed_cpus()
}

/// The NUMA node of `cpu`, read from `sys` (`/sys` on the host); `None` without NUMA information.
pub fn cpu_node(sys: &Path, cpu: usize) -> Option<usize> {
    let dir = sys.join(format!("devices/system/cpu/cpu{cpu}"));
    std::fs::read_dir(dir).ok()?.find_map(|e| {
        let name = e.ok()?.file_name();
        name.to_str()?.strip_prefix("node")?.parse().ok()
    })
}

/// Binds the calling thread's future allocations to the NUMA node it runs on (`set_mempolicy`
/// with `MPOL_LOCAL`). `Ok(false)` where there is no NUMA (a kernel without it, macOS).
pub fn bind_memory_local() -> io::Result<bool> {
    os_bind_memory_local()
}

#[cfg(target_os = "linux")]
mod os {
    use std::io;

    use crate::numa::MAX_CPUS;

    #[cfg(target_arch = "x86_64")]
    const SYS_SET_MEMPOLICY: i64 = 238;
    #[cfg(not(target_arch = "x86_64"))]
    const SYS_SET_MEMPOLICY: i64 = 237;
    const MPOL_LOCAL: i64 = 4;
    const ENOSYS: i32 = 38;

    unsafe extern "C" {
        fn sched_getaffinity(pid: i32, size: usize, mask: *mut u64) -> i32;
        fn syscall(num: i64, ...) -> i64;
    }

    pub(crate) fn allowed_cpus() -> io::Result<Vec<usize>> {
        let mut set = [0u64; MAX_CPUS / 64];
        // SAFETY: `set` is a `cpu_set_t` of `MAX_CPUS` bits that lives during the call; pid 0 is
        // the calling thread.
        let rc = unsafe { sched_getaffinity(0, size_of_val(&set), set.as_mut_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((0..MAX_CPUS).filter(|c| set[c / 64] & (1 << (c % 64)) != 0).collect())
    }

    pub(crate) fn bind_memory_local() -> io::Result<bool> {
        // SAFETY: MPOL_LOCAL takes no node mask (null, 0 bits); no pointers.
        let rc = unsafe { syscall(SYS_SET_MEMPOLICY, MPOL_LOCAL, std::ptr::null::<u64>(), 0u64) };
        if rc == 0 {
            return Ok(true);
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(ENOSYS) { Ok(false) } else { Err(e) }
    }
}

#[cfg(target_os = "linux")]
use os::{allowed_cpus as os_allowed_cpus, bind_memory_local as os_bind_memory_local};

#[cfg(not(target_os = "linux"))]
fn os_allowed_cpus() -> io::Result<Vec<usize>> {
    let n = crate::host::online_cpus().unwrap_or(1).max(1) as usize;
    Ok((0..n).collect())
}

#[cfg(not(target_os = "linux"))]
fn os_bind_memory_local() -> io::Result<bool> {
    Ok(false)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn cpu_lists_parse_like_linux_writes_them() {
        assert_eq!(parse_cpu_list("0-3,8,10-11").unwrap(), vec![0, 1, 2, 3, 8, 10, 11]);
        assert_eq!(parse_cpu_list("5, 1 ,1-2").unwrap(), vec![5, 1, 2]);
        assert!(parse_cpu_list("3-1").unwrap_err().contains("range"));
        assert!(parse_cpu_list("a").unwrap_err().contains("item"));
        assert!(parse_cpu_list("").is_err());
        assert!(parse_cpu_list("0-1024").unwrap_err().contains("beyond"));
    }

    #[test]
    fn the_process_may_run_somewhere() {
        let cpus = allowed_cpus().unwrap();
        assert!(!cpus.is_empty());
        assert!(cpus.windows(2).all(|w| w[0] < w[1]));
        // Binding twice gives the same answer: there is NUMA or there is not.
        assert_eq!(bind_memory_local().unwrap(), bind_memory_local().unwrap());
    }

    #[test]
    fn nodes_come_from_the_cpu_directories() {
        let sys = std::env::temp_dir().join(format!("teseladb-numa-{}", std::process::id()));
        std::fs::create_dir_all(sys.join("devices/system/cpu/cpu3/node1")).unwrap();
        std::fs::create_dir_all(sys.join("devices/system/cpu/cpu3/cache")).unwrap();
        std::fs::create_dir_all(sys.join("devices/system/cpu/cpu4/cache")).unwrap();
        assert_eq!(cpu_node(&sys, 3), Some(1));
        assert_eq!(cpu_node(&sys, 4), None);
        assert_eq!(cpu_node(&sys, 9), None);
        std::fs::remove_dir_all(&sys).unwrap();
    }
}
