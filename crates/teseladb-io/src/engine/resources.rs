//! What the engine sees of the host, for the options of a node's engine (023 FR-027, FR-028,
//! FR-014): the CPUs the process may use, the memory the engine would grant and the cgroup's
//! limit, the huge pages the system keeps and the reactor backend the engine picks. Each is read
//! with the engine's own functions where it has them; [`engine_config`] puts them together with
//! the node's options ([`super::options`]).

use std::path::PathBuf;

use teseladb::engine::core::cgroup;
use teseladb::engine::core::posix::get_current_cpuset;
use teseladb::engine::core::reactor_backend::ReactorBackendSelector;
use teseladb::engine::core::resource::{Configuration, calculate_memory};

use super::options::{
    DirectIo, EngineConfig, HugePages, Placement, default_memory, hugepages_for, hugetlbfs_mount,
    shard_cpus,
};

/// Where the system says how many huge pages it keeps.
const MEMINFO: &str = "/proc/meminfo";
/// Where the system lists its mounts.
const MOUNTS: &str = "/proc/mounts";

/// The CPUs the process may run on (its affinity, which a cgroup's cpuset narrows), in increasing
/// order, as the engine reads them.
pub fn allowed_cpus() -> Vec<usize> {
    get_current_cpuset().into_iter().filter_map(|c| usize::try_from(c).ok()).collect()
}

/// The physical memory of the host, in bytes.
fn physical_memory() -> u64 {
    let page = teseladb_sys::host::page_size().unwrap_or(0);
    teseladb_sys::host::physical_pages().unwrap_or(0).saturating_mul(page)
}

/// What the engine gives `shards` shards without a `--memory`: the memory available to the
/// process less the engine's reserve for the system.
fn engine_grant(available: u64, shards: u16) -> u64 {
    let c = Configuration { cpus: usize::from(shards.max(1)), ..Configuration::default() };
    let available = usize::try_from(available).unwrap_or(usize::MAX);
    calculate_memory(&c, available, 1.0).map_or(available as u64, |m| m as u64)
}

/// The memory of a node of `shards` shards (FR-028): `limit` (`--memory-limit`), or
/// three quarters of the lower of the physical memory and the cgroup's limit, within what the
/// engine grants.
pub fn node_memory(limit: Option<u64>, shards: u16) -> u64 {
    if let Some(l) = limit {
        return l;
    }
    let physical = physical_memory();
    let cgroup = u64::try_from(cgroup::memory_limit()).unwrap_or(u64::MAX);
    default_memory(physical, cgroup, engine_grant(physical.min(cgroup), shards))
}

/// The hugetlbfs mount the engine's `memory` bytes go to (FR-014), or why they stay in ordinary
/// pages.
///
/// # Errors
///
/// The reason, for the node's log.
pub fn hugepages(memory: u64) -> Result<PathBuf, String> {
    let pages = std::fs::read_to_string(MEMINFO).ok().and_then(|t| HugePages::parse(&t));
    let mount = std::fs::read_to_string(MOUNTS).ok().and_then(|t| hugetlbfs_mount(&t));
    hugepages_for(memory, pages, mount.as_deref())
}

/// The reactor backend the engine picks (FR-033): io_uring if the kernel allows it, else
/// linux-aio.
pub fn backend_name() -> String {
    ReactorBackendSelector::default_backend()
        .map_or_else(|_| "unknown".to_owned(), |b| b.name().clone())
}

/// The engine of a node: `shards` shards placed as `placement` says, `limit` bytes of memory (or
/// the default, [`node_memory`]) in huge pages when the system has them, and files in `direct_io`
/// mode. The second value says why the memory is not in huge pages, when it is not.
///
/// # Errors
///
/// A `--cpu-list` that does not fit the shards or names a CPU the process may not use.
pub fn engine_config(
    shards: u16,
    placement: &Placement,
    limit: Option<u64>,
    direct_io: DirectIo,
) -> Result<(EngineConfig, Option<String>), String> {
    let cpus = shard_cpus(placement, shards, &allowed_cpus())?;
    let memory = node_memory(limit, shards);
    let (hugepages, why_not) = match hugepages(memory) {
        Ok(p) => (Some(p), None),
        Err(why) => (None, Some(why)),
    };
    let cfg = EngineConfig {
        shards: shards.max(1),
        cpus,
        memory: Some(memory),
        hugepages,
        direct_io,
        idle_poll_us: None,
        start_limit: None,
    };
    Ok((cfg, why_not))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_process_may_use_at_least_one_cpu_in_increasing_order() {
        let cpus = allowed_cpus();
        assert!(!cpus.is_empty());
        assert!(cpus.windows(2).all(|w| w[0] < w[1]), "{cpus:?}");
    }

    #[test]
    fn a_limit_is_the_node_s_memory_and_without_one_the_host_s_three_quarters() {
        assert_eq!(node_memory(Some(123 << 20), 4), 123 << 20);
        let m = node_memory(None, 1);
        assert!(m > 0, "{m}");
        assert!(m <= physical_memory() / 4 * 3 + 1, "{m}");
    }

    #[test]
    fn the_engine_grants_less_than_what_is_available() {
        let available = 64u64 << 30;
        let g = engine_grant(available, 2);
        assert!(g > 0 && g < available, "{g}");
    }

    #[test]
    fn the_backend_is_one_the_engine_knows() {
        let b = backend_name();
        assert!(["io_uring", "linux-aio", "epoll"].contains(&b.as_str()), "{b}");
    }

    #[test]
    fn the_engine_of_a_node_puts_its_options_together() {
        let (cfg, why_not) = engine_config(1, &Placement::Unpinned, Some(256 << 20), DirectIo::Off)
            .expect("an unpinned node always fits");
        assert_eq!(cfg.shards, 1);
        assert!(cfg.cpus.is_empty());
        assert_eq!(cfg.memory, Some(256 << 20));
        assert_eq!(cfg.direct_io, DirectIo::Off);
        assert_eq!(cfg.hugepages.is_none(), why_not.is_some());
        let first = allowed_cpus()[0];
        let (pinned, _) =
            engine_config(1, &Placement::List(vec![first]), Some(1 << 20), DirectIo::Auto).unwrap();
        assert_eq!(pinned.cpus, vec![first]);
        let err =
            engine_config(2, &Placement::List(vec![first]), None, DirectIo::Auto).unwrap_err();
        assert!(err.contains("needs one per core"), "{err}");
    }
}
