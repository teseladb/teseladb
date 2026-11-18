//! The engine's options of a node (023 FR-027, FR-014, FR-028, FR-033; `contracts/cli-engine.md`):
//! what `teseladb start` and `teseladb dev` read from their command line and configuration file,
//! translated into the engine's command line. The engine reads no arguments of its own; the node
//! builds them here.
//!
//! | TeselaDB | Engine |
//! |----------|--------|
//! | `--cores <n>` | `--smp n` |
//! | `--cpu-list <cpus>` | `--cpuset`, `--thread-affinity 1`, `--mbind 1` |
//! | `--memory-limit <bytes>` | `--memory` (split evenly between the shards) |
//! | huge pages reserved | `--hugepages <hugetlbfs mount>` |
//! | `--direct-io auto` / `on` / `off` | `--relaxed-dma` / nothing / `--kernel-page-cache 1` |
//! | `--idle-poll-us <µs>` | `--idle-poll-time-us` (by default [`DEFAULT_IDLE_POLL_US`]) |
//! | always | `--network-stack posix`, `--blocked-reactor-notify-ms 25`, `--task-quota-ms 0.5` |
//!
//! The engine's allocator is the global one (D-181) and its reactor backend is the one it picks
//! (io_uring if the kernel allows it, else linux-aio): neither is an option.
//!
//! Everything here is plain logic and builds on every platform; what reads the host lives in
//! [`super::resources`] (Linux).

use std::fmt;
use std::path::{Path, PathBuf};

/// The stall detector's threshold (SC-013): a shard that does not yield for this long is reported.
pub const STALL_NOTIFY_MS: u32 = 25;

/// The environment variable that brings the stall detector's threshold down, in milliseconds, for
/// a diagnosis run: each report names the stack that held its shard (the diagnosis binary of
/// `linux-build.sh --diag` symbolizes it). Not an option of the node.
pub const STALL_NOTIFY_ENV: &str = "TESELADB_STALL_NOTIFY_MS";

/// The stall detector's threshold from the value of [`STALL_NOTIFY_ENV`], if it is a whole
/// number of milliseconds of at least 1; otherwise [`STALL_NOTIFY_MS`].
#[must_use]
pub fn stall_notify_ms(env: Option<&str>) -> u32 {
    env.and_then(|v| v.trim().parse::<u32>().ok()).filter(|&ms| ms >= 1).unwrap_or(STALL_NOTIFY_MS)
}

/// How long an idle shard polls before it sleeps, without `--idle-poll-us`: not at all. The
/// engine's own default, 2 ms on a virtual machine, kept every idle shard of a loaded node on its
/// CPU: on a host shared with other work (one-node load, 17-11-2026, three pairs) a node without
/// it served 18-31 % more operations with 37-40 % less CPU per operation and its writes' p50
/// 20-30 % lower; a sleeping shard is woken by the engine when work comes. A dedicated host
/// that wants the polling back gives the option.
pub const DEFAULT_IDLE_POLL_US: u32 = 0;

/// The engine's task quota, its default: the longest a task runs before the reactor polls.
pub const TASK_QUOTA_MS: &str = "0.5";

/// The node's memory without `--memory-limit`: this share, in percent, of what the host has
/// (012 FR-013).
pub const DEFAULT_MEMORY_PERCENT: u64 = 75;

/// The most CPUs a list can name (the size of an affinity mask).
pub const MAX_CPUS: usize = 1024;

/// `--direct-io` (012 FR-003, 023 FR-033): the mode of the engine's files.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DirectIo {
    /// `O_DIRECT`; where the file system does not take it, the system's cache with a warning.
    #[default]
    Auto,
    /// `O_DIRECT`; the node does not start without it.
    On,
    /// The system's cache.
    Off,
}

impl DirectIo {
    /// The value of `--direct-io`.
    pub fn parse(s: &str) -> Option<DirectIo> {
        match s {
            "auto" => Some(DirectIo::Auto),
            "on" => Some(DirectIo::On),
            "off" => Some(DirectIo::Off),
            _ => None,
        }
    }

    /// Its value as `--direct-io` takes it.
    pub fn as_str(self) -> &'static str {
        match self {
            DirectIo::Auto => "auto",
            DirectIo::On => "on",
            DirectIo::Off => "off",
        }
    }
}

/// Where a node's shards run (`--cpu-list`, 012 FR-006).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Placement {
    /// Not pinned: a node embedded in a test or a tool.
    #[default]
    Unpinned,
    /// Pinned to the CPUs the process may use, in order (`teseladb start` without `--cpu-list`).
    Allowed,
    /// Pinned to these CPUs (`--cpu-list`).
    List(Vec<usize>),
}

/// How a node's engine starts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineConfig {
    /// Shards: the node's cores.
    pub shards: u16,
    /// The CPU of each shard, in shard order (see [`shard_cpus`]); empty: the shards are not
    /// pinned.
    pub cpus: Vec<usize>,
    /// Memory for all the shards, in bytes, split evenly between them; `None`: the engine's
    /// default.
    pub memory: Option<u64>,
    /// The hugetlbfs mount the engine's memory comes from (FR-014); `None`: ordinary pages.
    pub hugepages: Option<PathBuf>,
    /// The mode of the engine's files (`--direct-io`).
    pub direct_io: DirectIo,
    /// How long, in microseconds, an idle shard polls before it sleeps (`--idle-poll-us`);
    /// `None`: [`DEFAULT_IDLE_POLL_US`].
    pub idle_poll_us: Option<u32>,
}

impl EngineConfig {
    /// The engine's command line (its program name first).
    pub fn args(&self) -> Vec<String> {
        let mut a = vec!["teseladb".to_owned(), "--smp".to_owned(), self.shards.max(1).to_string()];
        if self.cpus.is_empty() {
            // Without pinning the engine binds no memory either.
            a.extend(["--thread-affinity".to_owned(), "0".to_owned()]);
        } else {
            let list: Vec<String> = self.cpus.iter().map(ToString::to_string).collect();
            a.extend([
                "--cpuset".to_owned(),
                list.join(","),
                "--thread-affinity".to_owned(),
                "1".to_owned(),
                // Each shard's memory on its CPU's NUMA node.
                "--mbind".to_owned(),
                "1".to_owned(),
            ]);
        }
        if let Some(m) = self.memory {
            a.extend(["--memory".to_owned(), m.to_string()]);
        }
        if let Some(p) = &self.hugepages {
            a.extend(["--hugepages".to_owned(), p.display().to_string()]);
        }
        match self.direct_io {
            DirectIo::Auto => a.push("--relaxed-dma".to_owned()),
            DirectIo::On => {}
            DirectIo::Off => a.extend(["--kernel-page-cache".to_owned(), "1".to_owned()]),
        }
        let idle_poll = self.idle_poll_us.unwrap_or(DEFAULT_IDLE_POLL_US);
        a.extend(["--idle-poll-time-us".to_owned(), idle_poll.to_string()]);
        a.extend([
            "--network-stack".to_owned(),
            "posix".to_owned(),
            "--blocked-reactor-notify-ms".to_owned(),
            stall_notify_ms(std::env::var(STALL_NOTIFY_ENV).ok().as_deref()).to_string(),
            "--task-quota-ms".to_owned(),
            TASK_QUOTA_MS.to_owned(),
            // The node writes its own log; the engine says only what goes wrong.
            "--default-log-level".to_owned(),
            "error".to_owned(),
        ]);
        a
    }

    /// The start line of the node (`contracts/cli-engine.md`): the reactor's backend, the files'
    /// mode, the shards and their CPUs, the engine's memory and whether it is in huge pages.
    pub fn describe(&self, backend: &str) -> EngineLine<'_> {
        EngineLine { cfg: self, backend: backend.to_owned() }
    }
}

/// [`EngineConfig::describe`].
#[derive(Debug)]
pub struct EngineLine<'a> {
    cfg: &'a EngineConfig,
    backend: String,
}

impl fmt::Display for EngineLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = self.cfg;
        write!(f, "engine: backend {}, files {}", self.backend, c.direct_io.as_str())?;
        write!(f, ", shards {}", c.shards.max(1))?;
        if c.cpus.is_empty() {
            write!(f, " not pinned")?;
        } else {
            let list: Vec<String> = c.cpus.iter().map(ToString::to_string).collect();
            write!(f, " on CPUs {}", list.join(","))?;
        }
        match c.memory {
            Some(m) => write!(f, ", memory {m} bytes")?,
            None => write!(f, ", memory the engine's default")?,
        }
        match &c.hugepages {
            Some(p) => write!(f, " in huge pages ({})", p.display()),
            None => write!(f, " in ordinary pages"),
        }
    }
}

/// Parses a CPU list as Linux writes them (`0-3,8,10-11`): the CPUs in the order written, without
/// repeats.
///
/// # Errors
///
/// An item that is not a number or a range, a range that goes down, or a CPU past [`MAX_CPUS`].
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

/// The CPU of each of `shards` shards, given the CPUs the process may use (`allowed`, in
/// increasing order). Without a list the shards take the allowed CPUs in order, and more shards
/// than CPUs leave them unpinned (the engine pins one shard per CPU); a list shorter than the
/// shards, or with a CPU the process may not use, is a usage error, which the engine would
/// otherwise turn into an exit of the process.
///
/// The engine's cpuset is a set: shard `k` runs on the `k`-th lowest of the CPUs chosen, so the
/// result is in increasing order.
///
/// # Errors
///
/// The usage error, with its text.
pub fn shard_cpus(p: &Placement, shards: u16, allowed: &[usize]) -> Result<Vec<usize>, String> {
    let n = usize::from(shards.max(1));
    let mut cpus = match p {
        Placement::Unpinned => return Ok(Vec::new()),
        Placement::Allowed if allowed.len() < n => return Ok(Vec::new()),
        Placement::Allowed => allowed[..n].to_vec(),
        Placement::List(list) => {
            if list.len() < n {
                return Err(format!(
                    "--cpu-list names {} CPUs for {n} cores: it needs one per core",
                    list.len()
                ));
            }
            if let Some(c) = list.iter().find(|c| !allowed.contains(c)) {
                return Err(format!("--cpu-list: the process may not run on CPU {c}"));
            }
            list[..n].to_vec()
        }
    };
    cpus.sort_unstable();
    Ok(cpus)
}

/// The node's memory without `--memory-limit` (FR-028): [`DEFAULT_MEMORY_PERCENT`] of the lower of
/// the physical memory and the cgroup's limit, and never more than the engine grants by default
/// (`engine_grant`, what it leaves after its reserve for the system), so that the engine starts.
pub fn default_memory(physical: u64, cgroup: u64, engine_grant: u64) -> u64 {
    let host = physical.min(cgroup);
    (host / 100 * DEFAULT_MEMORY_PERCENT + host % 100 * DEFAULT_MEMORY_PERCENT / 100)
        .min(engine_grant)
}

/// Huge pages the system keeps for the processes, from `/proc/meminfo`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HugePages {
    /// Free huge pages (`HugePages_Free`).
    pub free: u64,
    /// The size of a huge page in bytes (`Hugepagesize`).
    pub size: u64,
}

impl HugePages {
    /// Reads `/proc/meminfo`'s text; `None` without huge pages.
    pub fn parse(meminfo: &str) -> Option<HugePages> {
        let field = |name: &str| {
            meminfo.lines().find_map(|l| {
                let rest = l.strip_prefix(name)?.trim_start().strip_prefix(':')?;
                rest.split_whitespace().next()?.parse::<u64>().ok()
            })
        };
        let free = field("HugePages_Free")?;
        let size = field("Hugepagesize")?.checked_mul(1024)?;
        (free > 0 && size > 0).then_some(HugePages { free, size })
    }

    /// Bytes of the free huge pages.
    pub fn bytes(&self) -> u64 {
        self.free.saturating_mul(self.size)
    }
}

/// The first hugetlbfs mount in `/proc/mounts`' text.
pub fn hugetlbfs_mount(mounts: &str) -> Option<PathBuf> {
    mounts.lines().find_map(|l| {
        let mut f = l.split_whitespace();
        let (_dev, at, kind) = (f.next()?, f.next()?, f.next()?);
        (kind == "hugetlbfs").then(|| PathBuf::from(unescape_mount(at)))
    })
}

/// A mount point as `/proc/mounts` writes it, with `\040` for a space and the like.
fn unescape_mount(at: &str) -> String {
    let b = at.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let octal = b.get(i + 1..i + 4).filter(|d| d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match (b[i], octal) {
            (b'\\', Some(d)) => {
                out.push(d.iter().fold(0u8, |v, c| v.wrapping_mul(8).wrapping_add(c - b'0')));
                i += 4;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether the engine's `memory` bytes go in huge pages (FR-014): when the system has a
/// hugetlbfs mount and free huge pages for all of it. The reason when they do not, for the log.
pub fn hugepages_for(
    memory: u64,
    pages: Option<HugePages>,
    mount: Option<&Path>,
) -> Result<PathBuf, String> {
    let Some(pages) = pages else { return Err("the system keeps no huge pages".to_owned()) };
    let Some(mount) = mount else { return Err("no hugetlbfs is mounted".to_owned()) };
    if pages.bytes() < memory {
        return Err(format!(
            "{} bytes of free huge pages for {memory} bytes of memory",
            pages.bytes()
        ));
    }
    Ok(mount.to_path_buf())
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn pair(a: &[String], flag: &str) -> Option<String> {
        a.iter().position(|x| x == flag).and_then(|i| a.get(i + 1).cloned())
    }

    #[test]
    fn every_row_of_the_contract_becomes_its_engine_option() {
        let cfg = EngineConfig {
            shards: 3,
            cpus: vec![2, 4, 6],
            memory: Some(3 << 30),
            hugepages: Some(PathBuf::from("/dev/hugepages")),
            direct_io: DirectIo::Auto,
            idle_poll_us: None,
        };
        let a = cfg.args();
        assert_eq!(a[0], "teseladb");
        assert_eq!(pair(&a, "--smp").as_deref(), Some("3"));
        assert_eq!(pair(&a, "--cpuset").as_deref(), Some("2,4,6"));
        assert_eq!(pair(&a, "--thread-affinity").as_deref(), Some("1"));
        assert_eq!(pair(&a, "--mbind").as_deref(), Some("1"));
        assert_eq!(pair(&a, "--memory"), Some((3u64 << 30).to_string()));
        assert_eq!(pair(&a, "--hugepages").as_deref(), Some("/dev/hugepages"));
        assert!(a.contains(&"--relaxed-dma".to_owned()));
        assert!(!a.contains(&"--kernel-page-cache".to_owned()));
        assert_eq!(pair(&a, "--network-stack").as_deref(), Some("posix"));
        assert_eq!(pair(&a, "--blocked-reactor-notify-ms").as_deref(), Some("25"));
        assert_eq!(pair(&a, "--task-quota-ms").as_deref(), Some("0.5"));
        // The engine picks its backend, and an idle shard sleeps: neither is set.
        assert!(!a.iter().any(|x| x == "--reactor-backend" || x == "--poll-mode"));
        assert_eq!(pair(&a, "--idle-poll-time-us").as_deref(), Some("0"));
    }

    #[test]
    fn unpinned_shards_bind_no_memory_and_the_files_follow_direct_io() {
        let on = EngineConfig { shards: 0, direct_io: DirectIo::On, ..EngineConfig::default() };
        let a = on.args();
        assert_eq!(pair(&a, "--smp").as_deref(), Some("1"), "at least one shard");
        assert_eq!(pair(&a, "--thread-affinity").as_deref(), Some("0"));
        assert!(!a.iter().any(|x| x == "--cpuset" || x == "--mbind" || x == "--memory"));
        assert!(!a.iter().any(|x| x == "--relaxed-dma" || x == "--kernel-page-cache"));
        assert!(!a.iter().any(|x| x == "--hugepages"));
        let off = EngineConfig { direct_io: DirectIo::Off, ..on };
        assert_eq!(pair(&off.args(), "--kernel-page-cache").as_deref(), Some("1"));
    }

    #[test]
    fn the_idle_poll_is_the_node_s_default_unless_given() {
        let plain = EngineConfig::default();
        assert_eq!(pair(&plain.args(), "--idle-poll-time-us").as_deref(), Some("0"));
        let zero = EngineConfig { idle_poll_us: Some(0), ..EngineConfig::default() };
        assert_eq!(pair(&zero.args(), "--idle-poll-time-us").as_deref(), Some("0"));
        let some = EngineConfig { idle_poll_us: Some(200), ..EngineConfig::default() };
        assert_eq!(pair(&some.args(), "--idle-poll-time-us").as_deref(), Some("200"));
    }

    #[test]
    fn the_stall_threshold_comes_down_only_for_a_diagnosis() {
        assert_eq!(stall_notify_ms(None), STALL_NOTIFY_MS);
        assert_eq!(stall_notify_ms(Some("2")), 2);
        assert_eq!(stall_notify_ms(Some(" 3 ")), 3);
        // Nothing below 1 ms, and a value that is no number leaves the default.
        assert_eq!(stall_notify_ms(Some("0")), STALL_NOTIFY_MS);
        assert_eq!(stall_notify_ms(Some("x")), STALL_NOTIFY_MS);
    }

    #[test]
    fn direct_io_reads_its_three_values() {
        for d in [DirectIo::Auto, DirectIo::On, DirectIo::Off] {
            assert_eq!(DirectIo::parse(d.as_str()), Some(d));
        }
        assert_eq!(DirectIo::parse("yes"), None);
        assert_eq!(DirectIo::default(), DirectIo::Auto);
    }

    #[test]
    fn the_start_line_names_backend_files_shards_cpus_memory_and_pages() {
        let cfg = EngineConfig {
            shards: 2,
            cpus: vec![0, 1],
            memory: Some(1024),
            hugepages: Some(PathBuf::from("/mnt/huge")),
            direct_io: DirectIo::On,
            idle_poll_us: None,
        };
        assert_eq!(
            cfg.describe("io_uring").to_string(),
            "engine: backend io_uring, files on, shards 2 on CPUs 0,1, memory 1024 bytes in huge \
             pages (/mnt/huge)"
        );
        let plain = EngineConfig { shards: 1, ..EngineConfig::default() };
        assert_eq!(
            plain.describe("linux-aio").to_string(),
            "engine: backend linux-aio, files auto, shards 1 not pinned, memory the engine's \
             default in ordinary pages"
        );
    }

    #[test]
    fn cpu_lists_read_as_linux_writes_them() {
        assert_eq!(parse_cpu_list("0-3,8,10-11"), Ok(vec![0, 1, 2, 3, 8, 10, 11]));
        assert_eq!(parse_cpu_list("9, 8,8,3"), Ok(vec![9, 8, 3]));
        assert!(parse_cpu_list("a").unwrap_err().contains("bad CPU list item"));
        assert!(parse_cpu_list("4-2").unwrap_err().contains("bad CPU range"));
        assert!(parse_cpu_list("1024").unwrap_err().contains("beyond 1023"));
        assert!(parse_cpu_list("").is_err());
    }

    #[test]
    fn shard_k_runs_on_the_kth_cpu_chosen() {
        let allowed = [0, 1, 2, 3, 8, 9];
        assert_eq!(shard_cpus(&Placement::Unpinned, 2, &allowed), Ok(Vec::new()));
        assert_eq!(shard_cpus(&Placement::Allowed, 3, &allowed), Ok(vec![0, 1, 2]));
        assert_eq!(shard_cpus(&Placement::Allowed, 3, &[4, 5]), Ok(Vec::new()), "overprovisioned");
        assert_eq!(shard_cpus(&Placement::Allowed, 0, &[4]), Ok(vec![4]), "at least one");
        let list = Placement::List(vec![9, 8, 3]);
        assert_eq!(shard_cpus(&list, 2, &allowed), Ok(vec![8, 9]));
        let short = shard_cpus(&list, 4, &allowed).unwrap_err();
        assert_eq!(short, "--cpu-list names 3 CPUs for 4 cores: it needs one per core");
        let outside = shard_cpus(&Placement::List(vec![0, 5]), 2, &allowed).unwrap_err();
        assert_eq!(outside, "--cpu-list: the process may not run on CPU 5");
    }

    #[test]
    fn the_default_memory_is_three_quarters_of_the_host_within_the_engine_s_grant() {
        assert_eq!(default_memory(16 << 30, u64::MAX, u64::MAX), 12 << 30);
        assert_eq!(default_memory(16 << 30, 4 << 30, u64::MAX), 3 << 30, "the cgroup's limit");
        assert_eq!(default_memory(4 << 30, u64::MAX, 2 << 30), 2 << 30, "the engine's grant");
        assert_eq!(default_memory(101, u64::MAX, u64::MAX), 75);
    }

    #[test]
    fn huge_pages_come_from_meminfo_and_the_mounts() {
        let meminfo = "MemTotal:       16000000 kB\nHugePages_Total:    1024\n\
                       HugePages_Free:      512\nHugepagesize:       2048 kB\n";
        let pages = HugePages::parse(meminfo).unwrap();
        assert_eq!(pages, HugePages { free: 512, size: 2 << 20 });
        assert_eq!(pages.bytes(), 1 << 30);
        assert_eq!(HugePages::parse("HugePages_Free: 0\nHugepagesize: 2048 kB\n"), None);
        assert_eq!(HugePages::parse("MemTotal: 1 kB\n"), None);
        let mounts =
            "proc /proc proc rw 0 0\nnone /dev/huge\\040pages hugetlbfs rw,pagesize=2M 0 0\n";
        assert_eq!(hugetlbfs_mount(mounts), Some(PathBuf::from("/dev/huge pages")));
        assert_eq!(hugetlbfs_mount("proc /proc proc rw 0 0\n"), None);
        assert_eq!(unescape_mount("a\\134b\\0"), "a\\b\\0");
    }

    #[test]
    fn memory_goes_in_huge_pages_only_when_all_of_it_fits() {
        let pages = Some(HugePages { free: 512, size: 2 << 20 });
        let at = Path::new("/dev/hugepages");
        assert_eq!(hugepages_for(1 << 30, pages, Some(at)), Ok(at.to_path_buf()));
        let short = hugepages_for(2 << 30, pages, Some(at)).unwrap_err();
        assert!(short.contains("1073741824 bytes of free huge pages"), "{short}");
        assert!(hugepages_for(1, None, Some(at)).unwrap_err().contains("no huge pages"));
        assert!(hugepages_for(1, pages, None).unwrap_err().contains("no hugetlbfs"));
    }
}
