//! Command-line options of the multicore layer: shard count, CPU set, memory, huge pages, I/O
//! groups and the allocator choice.
//!
//! [`SmpOptions`] is an option group. It is built in two steps, like every option group of the
//! engine: [`SmpOptions::new`] creates the values unlinked, and [`SmpOptions::link`] registers them,
//! in place, under a parent group once the struct is pinned at its final address.

use std::pin::Pin;

use crate::engine::core::resource::Cpuset;
#[cfg(not(feature = "hwloc"))]
use crate::engine::util::program_options::Unused;
use crate::engine::util::program_options::{OptionGroup, Value};

/// Which allocator the application uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemoryAllocator {
    /// The engine's own allocator, optimized for the shard-per-core design: each shard allocates
    /// from its own memory without locks. Recommended for most applications.
    #[default]
    Engine,
    /// The standard allocator from libc.
    /// Useful for small tools built on the engine that want to minimize their footprint on the
    /// system they run on.
    Standard,
}

/// Configuration of the multicore layer: how many shards to start, where, and with how much memory.
pub struct SmpOptions {
    /// The option group this struct extends.
    pub group: OptionGroup,
    /// Number of threads (default: one per CPU).
    pub smp: Value<u32>,
    /// CPUs to use (in cpuset(7) format; default: all)).
    pub cpuset: Value<Cpuset>,
    /// Memory to use, in bytes (ex: 4G) (default: all).
    pub memory: Value<String>,
    /// Memory left to the OS and other processes when `memory` is not given; the engine does not
    /// use it.
    pub reserve_memory: Value<String>,
    /// Path to accessible hugetlbfs mount (typically /dev/hugepages/something).
    pub hugepages: Value<String>,
    /// Lock all memory (prevents swapping).
    pub lock_memory: Value<bool>,
    /// Pin threads to their cpus (disable for overprovisioning).
    ///
    /// Default: `true`.
    pub thread_affinity: Value<bool>,
    /// Number of I/O groups.
    ///
    /// Each I/O group is responsible for a fraction of the I/O requests. Defaults to the number of
    /// NUMA nodes. Unused when the crate is built without the `hwloc` feature.
    pub num_io_groups: Value<u32>,
    /// Path to a YAML file describing the characteristics of the I/O Subsystem.
    pub io_properties_file: Value<String>,
    /// A YAML string describing the characteristics of the I/O Subsystem.
    pub io_properties: Value<String>,
    /// Bind each shard's memory to its NUMA node with `mbind(2)`.
    ///
    /// Default: `true`.
    pub mbind: Value<bool>,
    /// Enable the workaround for the scalability problem of the glibc unwinder, which takes a global
    /// lock while it looks up unwind tables (see [`exception_hacks`](crate::engine::core::exception_hacks)).
    ///
    /// Default: `true`. Unused on builds without that workaround.
    pub enable_glibc_exception_scaling_workaround: Value<bool>,
    /// If some CPUs are found not to have any local NUMA nodes, allow assigning
    /// them to remote ones. Unused when the crate is built without the `hwloc` feature.
    pub allow_cpus_in_remote_numa_nodes: Value<bool>,
    /// Memory allocator to use.
    ///
    /// The following options only have an effect with [`MemoryAllocator::Engine`]:
    /// `memory`, `reserve_memory`, `hugepages`, `mbind`, and the reactor options `heapprof`,
    /// `abort_on_engine_bad_alloc` and `dump_memory_diagnostics_on_alloc_failure_kind`.
    ///
    /// Unused on builds without the engine allocator; the options above are then ignored too.
    pub memory_allocator: MemoryAllocator,
    /// Additional memory, per shard, left to the OS and other processes on top of the default or
    /// `reserve_memory` value; the engine does not use it. Internal: set by code, not by a flag.
    pub reserve_additional_memory_per_shard: usize,
}

impl SmpOptions {
    /// Creates the options with their names, defaults and help texts, not yet linked to a group
    /// (see [`SmpOptions::link`]).
    pub fn new() -> Self {
        Self {
            group: OptionGroup::new("SMP options"),
            smp: Value::new("smp", None, "number of threads (default: one per CPU)"),
            cpuset: Value::new(
                "cpuset",
                None,
                "CPUs to use (in cpuset(7) list format (ex: 0,1-3,7); default: all))",
            ),
            memory: Value::new("memory", None, "memory to use, in bytes (ex: 4G) (default: all)"),
            reserve_memory: Value::new(
                "reserve-memory",
                None,
                "memory reserved to OS (if --memory not specified)",
            ),
            hugepages: Value::new(
                "hugepages",
                None,
                "path to accessible hugetlbfs mount (typically /dev/hugepages/something)",
            ),
            lock_memory: Value::new("lock-memory", None, "lock all memory (prevents swapping)"),
            thread_affinity: Value::new(
                "thread-affinity",
                Some(true),
                "pin threads to their cpus (disable for overprovisioning)",
            ),
            #[cfg(feature = "hwloc")]
            num_io_groups: Value::new(
                "num-io-groups",
                None,
                "Number of IO groups. Each IO group will be responsible for a fraction of the IO requests. Defaults to the number of NUMA nodes",
            ),
            #[cfg(not(feature = "hwloc"))]
            num_io_groups: Value::new_unused("num-io-groups", Unused),
            io_properties_file: Value::new(
                "io-properties-file",
                None,
                "path to a YAML file describing the characteristics of the I/O Subsystem",
            ),
            io_properties: Value::new(
                "io-properties",
                None,
                "a YAML string describing the characteristics of the I/O Subsystem",
            ),
            mbind: Value::new("mbind", Some(true), "enable mbind"),
            enable_glibc_exception_scaling_workaround: Value::new(
                "enable-glibc-exception-scaling-workaround",
                Some(true),
                "enable workaround for the scalability problem of the glibc unwinder",
            ),
            #[cfg(feature = "hwloc")]
            allow_cpus_in_remote_numa_nodes: Value::new(
                "allow-cpus-in-remote-numa-nodes",
                Some(true),
                "if some CPUs are found not to have any local NUMA nodes, allow assigning them to remote ones",
            ),
            #[cfg(not(feature = "hwloc"))]
            allow_cpus_in_remote_numa_nodes: Value::new_unused(
                "allow-cpus-in-remote-numa-nodes",
                Unused,
            ),
            memory_allocator: MemoryAllocator::Engine,
            reserve_additional_memory_per_shard: 0,
        }
    }

    /// Links the group under `parent_group` and registers every value with it, in place, in the
    /// order the fields are declared (which is the order `--help` lists them).
    pub fn link(self: Pin<&mut Self>, parent_group: Option<Pin<&mut OptionGroup>>) {
        crate::link_option_group!(
            self,
            parent_group,
            group,
            smp,
            cpuset,
            memory,
            reserve_memory,
            hugepages,
            lock_memory,
            thread_affinity,
            num_io_groups,
            io_properties_file,
            io_properties,
            mbind,
            enable_glibc_exception_scaling_workaround,
            allow_cpus_in_remote_numa_nodes
        );
    }
}

impl Default for SmpOptions {
    fn default() -> Self {
        Self::new()
    }
}
