//! Limits imposed by the Linux control group of the process.
//!
//! The resource allocator reads these at start-up so that the number of shards and the memory given
//! to them respect the `cpuset` and memory controllers of the cgroup (v1 or v2) the process runs in.
//! The functions are implemented in [`resource::cgroup`](crate::engine::core::resource::cgroup) and
//! re-exported here.

/// A set of CPU numbers, kept sorted.
pub type Cpuset = std::collections::BTreeSet<u32>;

/// Cgroup queries: [`cpu_set`] (the CPUs the cgroup allows, if any limit is set), [`memory_limit`]
/// (the memory limit in bytes), and the generic readers [`read_setting_as`] (parse the first line of
/// a cgroup file) and [`read_setting_v1v2_as`] (the same, choosing the v1 or v2 file that exists).
pub use crate::engine::core::resource::cgroup::{
    cpu_set, memory_limit, read_setting_as, read_setting_v1v2_as,
};
