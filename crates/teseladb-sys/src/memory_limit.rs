//! The memory a node may use by default (012 FR-013): 75 % of the memory available to the process,
//! the lowest of the physical memory and the limit of its cgroup (v2 `memory.max`, or v1
//! `memory.limit_in_bytes`), read from the files of `/sys/fs/cgroup` the kernel publishes.

use std::path::Path;

/// Share of the available memory a node takes when `--memory-limit` is not given, in percent.
pub const DEFAULT_SHARE_PCT: u64 = 75;

/// A cgroup v1 reports no limit as a huge number close to `i64::MAX`, rounded to the page.
const V1_NO_LIMIT: u64 = 1 << 62;

/// The limit a cgroup file holds: `None` for `max` (v2), for a v1 value meaning no limit, or for
/// something that is not a number.
pub fn parse_cgroup_limit(text: &str) -> Option<u64> {
    let t = text.trim();
    if t == "max" {
        return None;
    }
    t.parse::<u64>().ok().filter(|&v| v < V1_NO_LIMIT)
}

/// The cgroup v2 path of this process from the text of `/proc/self/cgroup` (`0::<path>`).
pub fn cgroup_v2_path(proc_cgroup: &str) -> Option<&str> {
    proc_cgroup.lines().find_map(|l| l.strip_prefix("0::"))
}

/// The default limit with `physical` bytes of memory and the cgroup's limit `cgroup`.
pub fn default_limit(physical: u64, cgroup: Option<u64>) -> u64 {
    let available = cgroup.map_or(physical, |c| c.min(physical));
    available / 100 * DEFAULT_SHARE_PCT + available % 100 * DEFAULT_SHARE_PCT / 100
}

/// The limit of this process's cgroup under the cgroup file system mounted at `root`, if any.
pub fn cgroup_limit(root: &Path, proc_cgroup: &str) -> Option<u64> {
    let read = |p: &Path| std::fs::read_to_string(p).ok().and_then(|t| parse_cgroup_limit(&t));
    let v2 = cgroup_v2_path(proc_cgroup)
        .map(|rel| root.join(rel.trim_start_matches('/')).join("memory.max"))
        .and_then(|p| read(&p));
    v2.or_else(|| read(&root.join("memory").join("memory.limit_in_bytes")))
}

/// The default limit on this host: three quarters of the lowest of its physical memory and the
/// limit of this process's cgroup (`/proc/self/cgroup` under `/sys/fs/cgroup`, on Linux); `None`
/// if the system does not say how much physical memory there is.
pub fn host_default() -> Option<u64> {
    let physical = crate::host::physical_pages()?.checked_mul(crate::host::page_size()?)?;
    let cgroup = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|t| cgroup_limit(Path::new("/sys/fs/cgroup"), &t));
    Some(default_limit(physical, cgroup))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn cgroup_files_read_as_limits() {
        assert_eq!(parse_cgroup_limit("max\n"), None);
        assert_eq!(parse_cgroup_limit("536870912\n"), Some(536_870_912));
        assert_eq!(parse_cgroup_limit("9223372036854771712"), None, "v1 without a limit");
        assert_eq!(parse_cgroup_limit("lots"), None);
        assert_eq!(
            cgroup_v2_path("0::/system.slice/teseladb.service\n"),
            Some("/system.slice/teseladb.service")
        );
        assert_eq!(cgroup_v2_path("12:memory:/x\n"), None);
    }

    #[test]
    fn the_default_is_three_quarters_of_the_lowest() {
        assert_eq!(default_limit(16 << 30, None), 12 << 30);
        assert_eq!(default_limit(16 << 30, Some(4 << 30)), 3 << 30);
        assert_eq!(default_limit(1 << 30, Some(4 << 30)), 805_306_368);
        assert_eq!(default_limit(101, None), 75);
    }

    #[test]
    fn the_host_default_is_three_quarters_of_its_memory_at_most() {
        let physical = crate::host::physical_pages().unwrap() * crate::host::page_size().unwrap();
        let limit = host_default().unwrap();
        assert!(limit > 0 && limit <= default_limit(physical, None), "{limit} of {physical}");
    }

    #[test]
    fn the_cgroup_limit_comes_from_v2_or_else_v1() {
        let dir = std::env::temp_dir().join(format!("teseladb-cgroup-{}", std::process::id()));
        let v2 = dir.join("app");
        std::fs::create_dir_all(&v2).unwrap();
        std::fs::create_dir_all(dir.join("memory")).unwrap();
        std::fs::write(dir.join("memory/memory.limit_in_bytes"), "1048576\n").unwrap();
        assert_eq!(cgroup_limit(&dir, "0::/app\n"), Some(1_048_576), "no memory.max: v1");
        std::fs::write(v2.join("memory.max"), "2097152\n").unwrap();
        assert_eq!(cgroup_limit(&dir, "0::/app\n"), Some(2_097_152));
        std::fs::write(v2.join("memory.max"), "max\n").unwrap();
        assert_eq!(cgroup_limit(&dir, "0::/app\n"), Some(1_048_576));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(cgroup_limit(&dir, ""), None);
    }
}
