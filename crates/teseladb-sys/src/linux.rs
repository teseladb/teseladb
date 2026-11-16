//! Linux: the host facts of `host.rs`, AF_XDP and network namespaces. Waiting on descriptors and
//! waking threads is the engine's reactor (023 FR-042).

pub mod netns;
pub mod xdp;

use std::io;

// Host facts (`host.rs`).

/// `suseconds_t`.
pub(crate) type Suseconds = i64;
/// `sysconf(3)` names.
pub(crate) const SC_PAGESIZE: i32 = 30;
pub(crate) const SC_PHYS_PAGES: i32 = 85;
pub(crate) const SC_NPROCESSORS_ONLN: i32 = 84;
/// `getrlimit(2)` resource of open files.
pub(crate) const RLIMIT_NOFILE: i32 = 7;
/// `CLOCK_THREAD_CPUTIME_ID`: the CPU time of the calling thread.
pub(crate) const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
/// `ru_maxrss` is in kilobytes on Linux.
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
    Ok(crate::host::Memory {
        resident: resident * page,
        virtual_size: size * page,
        threads,
        pageins: None,
    })
}

/// `NAME` and `VERSION_ID` of `/etc/os-release`, as the reference server shows them in `hostInfo`.
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

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// Linux has no page-ins apart from `getrusage(2)`.
    #[test]
    fn there_are_no_page_ins_apart() {
        assert_eq!(memory().unwrap().pageins, None);
    }

    #[test]
    fn the_utsname_fields_are_the_system_headers_sizes() {
        assert_eq!(std::mem::size_of::<Utsname>(), 6 * UTS_LEN);
        assert_eq!(usec(7), 7);
        assert_eq!(Statvfs::default().sizes(), (0, 0, 0));
    }
}
