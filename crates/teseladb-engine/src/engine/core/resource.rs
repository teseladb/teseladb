//! Assignment of CPUs, memory and I/O queues to the shards at start-up.
//!
//! [`allocate`] takes a [`Configuration`] (requested CPUs and memory, I/O queue settings) and
//! returns [`Resources`]: for every shard, its CPU and the memory it gets on each NUMA node, plus
//! the [`IoQueueTopology`] that maps shards to I/O groups. Limits of the Linux control group
//! ([`cgroup`]) and the kernel's own reservation are respected; [`parse_cpuset`] parses CPU lists
//! in `cpuset(7)` format.
//!
//! With the `hwloc` feature the real topology (NUMA nodes, cores, caches) is read through hwloc
//! 2.x, shards are spread over it, and memory is split per NUMA node. Without it, all CPUs are
//! treated as one node and every shard is its own I/O group member of a single group.

use std::collections::{BTreeSet, HashMap};
use std::hash::{BuildHasherDefault, Hasher};

/// A hasher that returns the integer (or pointer) key itself.
///
/// The iteration order of the maps in this module decides how orphan shards are spread over I/O
/// coordinators and orphan CPUs over NUMA nodes. With the identity hash that order is the same on
/// every run (a randomly seeded hash would change it at each start), and no hashing is paid.
#[derive(Debug, Default, Clone, Copy)]
pub struct IdentityHasher(u64);

impl Hasher for IdentityHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 << 8) | u64::from(b);
        }
    }

    #[inline]
    fn write_u32(&mut self, n: u32) {
        self.0 = u64::from(n);
    }

    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.0 = n;
    }

    #[inline]
    fn write_usize(&mut self, n: usize) {
        self.0 = n as u64;
    }
}

/// Hash map keyed by an integer or a pointer, with the identity hash (deterministic order).
pub type UnorderedMap<K, V> = HashMap<K, V, BuildHasherDefault<IdentityHasher>>;
use std::str::FromStr;

use crate::engine::core::future::{ExceptionPtr, RuntimeError, make_exception_ptr};
use crate::engine::core::reactor::engine_logger;
use crate::engine::core::sstring::Sstring;
use crate::{log_debug, log_warn};

/// The `cpu_set_t` CPU mask of `sched_setaffinity(2)`, defined in `posix`.
pub use crate::engine::core::posix::CpuSetT;

/// Number of CPUs a [`CpuSetT`] can hold (`CPU_SETSIZE`).
pub const CPU_SETSIZE: usize = CpuSetT::SETSIZE;

use crate::engine::core::io_queue::{IoGroup, IoQueue};

/// A set of CPU numbers, kept sorted.
pub type Cpuset = BTreeSet<u32>;

/// Ownership of the hwloc topology.
pub mod hwloc {
    /// The topology holder.
    pub mod internal {
        #[cfg(feature = "hwloc")]
        use super::super::hwloc_ffi::*;

        /// Owns an hwloc topology, loaded lazily on first use and destroyed on drop.
        #[cfg(feature = "hwloc")]
        pub struct TopologyHolder {
            _topology: HwlocTopologyT,
        }

        #[cfg(feature = "hwloc")]
        impl Default for TopologyHolder {
            fn default() -> Self {
                Self { _topology: std::ptr::null_mut() }
            }
        }

        #[cfg(feature = "hwloc")]
        impl TopologyHolder {
            /// Returns a holder with no topology loaded yet.
            pub fn new() -> Self {
                Self::default()
            }

            /// Returns whether a topology is loaded.
            pub fn is_some(&self) -> bool {
                !self._topology.is_null()
            }

            /// Takes the topology out of `o`, leaving `o` empty.
            pub fn take_from(o: &mut TopologyHolder) -> Self {
                Self { _topology: std::mem::replace(&mut o._topology, std::ptr::null_mut()) }
            }

            /// Swaps the topologies of `self` and `o` (`o` then destroys the previous one).
            pub fn assign_from(&mut self, o: &mut TopologyHolder) {
                if !std::ptr::eq(self, o) {
                    std::mem::swap(&mut self._topology, &mut o._topology);
                }
            }

            /// Initializes and loads the topology of the running machine.
            pub fn init_and_load(&mut self) {
                // SAFETY: hwloc_topology_init writes a fresh topology into the out pointer.
                unsafe { hwloc_topology_init(&mut self._topology) };
                // After hwloc_topology_init succeeds the pointer is non-null, and Drop must call
                // hwloc_topology_destroy.

                // SAFETY: the topology was just initialized.
                unsafe { hwloc_topology_load(self._topology) };
            }

            /// Returns the topology, loading it on first use.
            pub fn get(&mut self) -> HwlocTopologyT {
                if self._topology.is_null() {
                    self.init_and_load();
                }
                self._topology
            }
        }

        #[cfg(feature = "hwloc")]
        impl Drop for TopologyHolder {
            fn drop(&mut self) {
                if !self._topology.is_null() {
                    // SAFETY: the topology was initialized by hwloc_topology_init and is owned here.
                    unsafe { hwloc_topology_destroy(self._topology) };
                }
            }
        }

        /// Without the `hwloc` feature there is no topology to hold.
        #[cfg(not(feature = "hwloc"))]
        #[derive(Debug, Default)]
        pub struct TopologyHolder {}
    }
}

/// What [`allocate`] is asked for.
#[derive(Default)]
pub struct Configuration {
    /// Memory to give the shards in total; `None` for everything usable.
    pub total_memory: Option<usize>,
    /// Memory left to the OS when `total_memory` is not given; `None` for the default
    /// (at least 1.5 GiB or 7% of the machine).
    pub reserve_memory: Option<usize>,
    /// Extra memory left to the OS per shard.
    pub reserve_additional_memory_per_shard: usize,
    /// Number of shards.
    pub cpus: usize,
    /// The CPUs the shards run on.
    pub cpu_set: Cpuset,
    /// Whether CPUs with no local NUMA node may be assigned to remote nodes.
    pub assign_orphan_cpus: bool,
    /// The CPUs that coordinate I/O queues, if given explicitly.
    pub io_queues: Vec<u32>,
    /// Number of I/O groups; 0 for one per NUMA node.
    pub num_io_groups: u32,
    /// The machine topology, loaded when needed.
    pub topology: hwloc::internal::TopologyHolder,
}

/// A share of memory on one NUMA node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Memory {
    /// Size in bytes.
    pub bytes: usize,
    /// NUMA node id.
    pub nodeid: u32,
}

/// Mapping of shards to I/O groups, and the I/O queues and groups themselves once created.
///
/// The reactor threads share it while they start: the vectors are
/// sized before, `shard_to_group` and `shards_in_group` are only read, each shard writes
/// only its own slot of `queues`, and `groups` is touched only with `lock` held. The slots and
/// the groups are therefore cells, so that no thread needs a `&mut` to the shared topology.
pub struct IoQueueTopology {
    /// Each shard's I/O queue, written once by that shard.
    pub queues:
        Vec<std::cell::UnsafeCell<Option<crate::engine::core::shared_ptr::SharedPtr<IoQueue>>>>,
    /// The I/O group of each shard.
    pub shard_to_group: Vec<u32>,
    /// Number of shards in each group.
    pub shards_in_group: Vec<u32>,
    /// The I/O groups, created by the first shard of each that starts. Guarded by `lock`.
    pub groups: std::cell::UnsafeCell<Vec<Option<std::sync::Arc<IoGroup>>>>,
    /// Guards `groups`.
    pub lock: crate::engine::util::spinlock::util::Spinlock,
}

// SAFETY: see the type: the shared parts are read-only, each slot of `queues` is written by
// its shard only, and `groups` only under `lock`; the queues and groups themselves are moved
// to the shard that uses them.
unsafe impl Sync for IoQueueTopology {}

/// One shard's CPU and memory.
#[derive(Debug, Clone, Default)]
pub struct Cpu {
    /// The CPU the shard runs on.
    pub cpu_id: u32,
    /// The shard's memory, per NUMA node.
    pub mem: Vec<Memory>,
}

/// The result of [`allocate`].
#[derive(Default)]
pub struct Resources {
    /// One entry per shard, in shard order.
    pub cpus: Vec<Cpu>,
    /// The I/O queue topology of each device id (0 for the default).
    pub ioq_topology: UnorderedMap<u32, IoQueueTopology>,
    /// The CPUs of each NUMA node id.
    pub numa_node_id_to_cpuset: UnorderedMap<u32, Cpuset>,
}

/// Parses the whole of `s` as a `T`; fails with a runtime error naming the input otherwise.
fn lexical_cast<T: FromStr>(s: &str) -> Result<T, ExceptionPtr> {
    s.parse::<T>()
        .map_err(|_| make_exception_ptr(RuntimeError::new(format!("cannot parse value: {}", s))))
}

/// Matches `(\d+-)?(\d+)(,(\d+-)?(\d+))*` against the whole string.
fn matches_cpuset_regex(value: &str) -> bool {
    fn digits(b: &[u8], mut i: usize) -> Option<usize> {
        let s = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i > s { Some(i) } else { None }
    }
    // One item: an optional "N-" then "N".
    fn item(b: &[u8], i: usize) -> Option<usize> {
        let j = digits(b, i)?;
        if j < b.len()
            && b[j] == b'-'
            && let Some(k) = digits(b, j + 1)
        {
            return Some(k);
        }
        Some(j)
    }
    let b = value.as_bytes();
    let mut i = match item(b, 0) {
        Some(i) => i,
        None => return false,
    };
    while i < b.len() {
        if b[i] != b',' {
            return false;
        }
        i = match item(b, i + 1) {
            Some(j) => j,
            None => return false,
        };
    }
    true
}

/// Parses a CPU list in `cpuset(7)` list format, such as `"0,1-3,7"`.
///
/// Returns `None` if the text does not have that form or a range is reversed (`"3-1"`), so that
/// option validation can report it.
///
/// # Errors
/// Fails if a number does not fit in a `u32`.
pub fn parse_cpuset(value: String) -> Result<Option<Cpuset>, ExceptionPtr> {
    if matches_cpuset_regex(&value) {
        let ranges: Vec<&str> = value.split(',').collect();
        let mut ret = Cpuset::new();
        for range in ranges {
            let mut beg = range;
            let mut end = range;
            if let Some(dash) = range.find('-') {
                beg = &range[..dash];
                end = &range[dash + 1..];
            }
            let b: u32 = lexical_cast(beg)?;
            let e: u32 = lexical_cast(end)?;

            if b > e {
                return Ok(None);
            }

            for i in b..=e {
                ret.insert(i);
            }
        }
        return Ok(Some(ret));
    }
    Ok(None)
}

/// Reading of Linux control group settings, v1 or v2 (re-exported by
/// [`core::cgroup`](crate::engine::core::cgroup)).
pub mod cgroup {
    use super::*;
    use crate::engine::util::read_first_line::read_first_line;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    /// Returns the CPUs the process's cpuset controller allows, or `None` (with a warning) if they
    /// cannot be read or parsed.
    pub fn cpu_set() -> Option<Cpuset> {
        let cpuset = read_setting_v1v2_as::<String>(
            "cpuset/cpuset.cpus".to_string(),
            "cpuset.cpus.effective".to_string(),
        );
        if let Some(cpuset) = cpuset {
            return super::parse_cpuset(cpuset).ok().flatten();
        }

        log_warn!(engine_logger(), "Unable to parse cgroup's cpuset. Ignoring.");
        None
    }

    /// Returns the memory limit of the process's cgroup in bytes, or `usize::MAX` if there is
    /// none or it cannot be read.
    pub fn memory_limit() -> usize {
        read_setting_v1v2_as::<usize>(
            "memory/memory.limit_in_bytes".to_string(),
            "memory.max".to_string(),
        )
        .unwrap_or(usize::MAX)
    }

    /// Parses the first line of the file at `path` as a `T`; `None` (with a warning) if it cannot
    /// be read or parsed.
    pub fn read_setting_as<T: FromStr>(path: String) -> Option<T> {
        let r = (|| -> Result<T, ExceptionPtr> {
            let line = read_first_line(Path::new(&path))?;
            lexical_cast::<T>(line.as_str())
        })();
        match r {
            Ok(v) => return Some(v),
            Err(_) => {
                log_warn!(engine_logger(), "Couldn't read cgroup file {}.", path);
            }
        }

        None
    }

    /// Returns the cgroup directory of the process on a cgroups-v2-only system, or `None` on a v1
    /// or hybrid one.
    ///
    /// On v2, `/proc/self/cgroup` reads `"0::<cgroup-dir-path>"`; hybrid configurations are not
    /// supported.
    ///
    /// # Errors
    /// Fails if `/proc/self/cgroup` cannot be read or is empty.
    fn cgroup2_path_my_pid() -> Result<Option<PathBuf>, ExceptionPtr> {
        let cline: Sstring = match read_first_line(Path::new("/proc/self/cgroup")) {
            Ok(l) => l,
            Err(_) => {
                // '/proc/self/cgroup' must be there. If not - there is an issue
                // with the system configuration.
                return Err(make_exception_ptr(RuntimeError::new(
                    "no cgroup data for our process".to_string(),
                )));
            }
        };

        // for a V2-only system, we expect exactly one line:
        // 0::<abs-path-to-cgroup>
        let s = cline.as_str();
        match s.as_bytes().first() {
            Some(b'0') => {}
            Some(_) => {
                // This is either a v1 system, or system configured with a hybrid of v1 & v2.
                // We do not support such combinations of v1 and v2 at this point.
                log_debug!(engine_logger(), "Not a cgroups-v2-only system");
                return Ok(None);
            }
            None => {
                return Err(make_exception_ptr(RuntimeError::new(
                    "out of range: the cgroup line is empty".to_string(),
                )));
            }
        }

        // the path is guaranteed to start with '0::/'
        Ok(Some(PathBuf::from(format!("/sys/fs/cgroup/{}", s.get(4..).unwrap_or("")))))
    }

    /// Walks the cgroups v2 hierarchy bottom-up, from the process's cgroup up to `/sys/fs/cgroup`,
    /// and returns the first path where `filename` exists.
    fn locate_lowest_cgroup2(mut lowest_subdir: PathBuf, filename: String) -> Option<PathBuf> {
        // locate the lowest subgroup containing the named file (i.e.
        // handles the requested control by itself)
        loop {
            //  does the cgroup settings file exist?
            let set_path = lowest_subdir.join(&filename);
            if set_path.exists() {
                return Some(set_path);
            }

            lowest_subdir = match lowest_subdir.parent() {
                Some(p) => p.to_path_buf(),
                None => PathBuf::new(),
            };
            if lowest_subdir == Path::new("/sys/fs") || lowest_subdir.as_os_str().is_empty() {
                break;
            }
        }

        None
    }

    // On v2 systems, the leaf cgroup that controls this process; computed once.
    static CG2_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

    /// Reads a setting from the cgroups v2 file `cg2_fname` or, on v1 systems, from
    /// `/sys/fs/cgroup/<cg1_path>`.
    ///
    /// On v2 the lowest cgroup of the process's hierarchy that has the file is used, and the value
    /// `"max"` means no limit. Returns `None` (with a warning) when the value is absent, unlimited,
    /// unreadable or malformed.
    pub fn read_setting_v1v2_as<T: FromStr>(cg1_path: String, cg2_fname: String) -> Option<T> {
        // A failure to read /proc/self/cgroup counts as "not v2".
        let cg2_path = CG2_PATH.get_or_init(|| cgroup2_path_my_pid().ok().flatten());

        if let Some(cg2_path) = cg2_path {
            // this is a v2 system
            let line = match locate_lowest_cgroup2(cg2_path.clone(), cg2_fname.clone())
                .ok_or(())
                .and_then(|p| read_first_line(&p).map_err(|_| ()))
            {
                Ok(l) => l,
                Err(()) => {
                    log_warn!(engine_logger(), "Could not read cgroups v2 file ({}).", cg2_fname);
                    return None;
                }
            };
            if line.as_str() != "max" {
                match lexical_cast::<T>(line.as_str()) {
                    Ok(v) => return Some(v),
                    Err(_) => {
                        log_warn!(
                            engine_logger(),
                            "Malformed cgroups file ({}) contents.",
                            cg2_fname
                        );
                    }
                }
            }
            return None;
        }

        // try cgroups v1:
        let r = read_first_line(&Path::new("/sys/fs/cgroup").join(&cg1_path))
            .and_then(|line| lexical_cast::<T>(line.as_str()));
        match r {
            Ok(v) => return Some(v),
            Err(_) => {
                log_warn!(engine_logger(), "Could not parse cgroups v1 file ({}).", cg1_path);
            }
        }

        None
    }
}

unsafe extern "C" {
    fn sysconf(name: std::ffi::c_int) -> std::ffi::c_long;
}

#[cfg(target_os = "linux")]
const _SC_PAGESIZE: std::ffi::c_int = 30;
#[cfg(target_os = "linux")]
const _SC_PHYS_PAGES: std::ffi::c_int = 85;
#[cfg(target_os = "linux")]
const _SC_NPROCESSORS_ONLN: std::ffi::c_int = 84;
#[cfg(target_os = "macos")]
const _SC_PAGESIZE: std::ffi::c_int = 29;
#[cfg(target_os = "macos")]
const _SC_PHYS_PAGES: std::ffi::c_int = 200;
#[cfg(target_os = "macos")]
const _SC_NPROCESSORS_ONLN: std::ffi::c_int = 58;

/// Returns the physical memory of the machine in bytes, from `sysconf(3)`.
fn get_machine_memory_from_sysconf() -> u64 {
    // SAFETY: sysconf has no preconditions.
    unsafe { sysconf(_SC_PAGESIZE) as u64 * sysconf(_SC_PHYS_PAGES) as usize as u64 }
}

/// Returns the memory the kernel keeps free (`vm.min_free_kbytes`) in bytes, or 0 if unknown.
fn kernel_memory_reservation() -> usize {
    match crate::engine::util::read_first_line::read_first_line_as::<usize>(std::path::Path::new(
        "/proc/sys/vm/min_free_kbytes",
    )) {
        Ok(v) => v * 1024,
        Err(_) => 0,
    }
}

/// Returns the memory to give the shards out of `available_memory`.
///
/// The kernel's reservation and the OS reserve (`c.reserve_memory`, by default the larger of
/// 1.5 GiB and 7% of memory, times `panic_factor`, plus the per-shard extra) are subtracted; at
/// least 500 MB is always granted. With `c.total_memory`, that amount (minus the per-shard extra)
/// is returned instead, if it fits.
///
/// # Errors
/// Fails if `c.total_memory` is smaller than the per-shard reserve or larger than the memory
/// available.
pub fn calculate_memory(
    c: &Configuration,
    mut available_memory: usize,
    panic_factor: f32,
) -> Result<usize, ExceptionPtr> {
    let kernel_reservation = kernel_memory_reservation();
    if kernel_reservation >= 200_000_000 {
        // The standard setting is sqrt(mem)*128. This is 128MB at 1TB RAM. With 64kB pages and transparent hugepages,
        // the kernel increases this significantly, wasting memory.
        log_warn!(
            engine_logger(),
            "Kernel memory reservation (/proc/sys/vm/min_free_kbytes) unexpectedly high ({}), check your configuration",
            kernel_reservation
        );
    }
    available_memory = available_memory.wrapping_sub(kernel_reservation);
    let default_reserve_memory = ((1536usize * 1024 * 1024)
        .max((0.07 * available_memory as f64) as usize) as f64
        * panic_factor as f64) as usize;
    let mut reserve = c.reserve_memory.unwrap_or(default_reserve_memory);
    let reserve_additional = c.reserve_additional_memory_per_shard * c.cpus;
    reserve += reserve_additional;
    let min_memory: usize = 500_000_000;
    if available_memory >= reserve + min_memory {
        available_memory -= reserve;
    } else {
        // Allow starting up even in low memory configurations (e.g. 2GB boot2docker VM)
        available_memory = min_memory;
    }
    let Some(total_memory) = c.total_memory else {
        return Ok(available_memory);
    };
    if total_memory < reserve_additional {
        return Err(make_exception_ptr(RuntimeError::new(format!(
            "insufficient total memory: reserve {} total {}",
            reserve_additional, total_memory
        ))));
    }
    let needed_memory = total_memory - reserve_additional;
    if needed_memory > available_memory {
        return Err(make_exception_ptr(RuntimeError::new(format!(
            "insufficient physical memory: needed {} available {}",
            needed_memory, available_memory
        ))));
    }
    Ok(needed_memory)
}

impl IoQueueTopology {
    /// Returns an empty topology.
    pub fn new() -> Self {
        Self {
            queues: Vec::new(),
            shard_to_group: Vec::new(),
            shards_in_group: Vec::new(),
            groups: std::cell::UnsafeCell::new(Vec::new()),
            lock: crate::engine::util::spinlock::util::Spinlock::new(),
        }
    }

    /// Takes the contents of `o`, leaving it empty; the lock is new.
    pub fn take_from(o: &mut IoQueueTopology) -> Self {
        Self {
            queues: std::mem::take(&mut o.queues),
            shard_to_group: std::mem::take(&mut o.shard_to_group),
            shards_in_group: std::mem::take(&mut o.shards_in_group),
            groups: std::mem::take(&mut o.groups),
            lock: crate::engine::util::spinlock::util::Spinlock::new(), // unused until now, so just initialize
        }
    }
}

impl Default for IoQueueTopology {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "hwloc")]
pub(crate) mod hwloc_ffi {
    //! hwloc 2.x bindings: the exported functions, plus Rust versions of the inline helpers of
    //! hwloc's headers, which have no symbol to link against.
    use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void};

    pub type HwlocUint64T = u64;
    pub type HwlocBitmapT = *mut c_void;
    pub type HwlocConstBitmapT = *const c_void;
    pub type HwlocCpusetT = HwlocBitmapT;
    pub type HwlocNodesetT = HwlocBitmapT;
    pub type HwlocTopologyT = *mut c_void;
    pub type HwlocObjTypeT = c_int;
    pub type HwlocObjT = *mut HwlocObj;

    pub const HWLOC_OBJ_MACHINE: HwlocObjTypeT = 0;
    #[allow(dead_code)] // part of the object type numbering; not queried by this module
    pub const HWLOC_OBJ_PACKAGE: HwlocObjTypeT = 1;
    #[allow(dead_code)] // part of the object type numbering; not queried by this module
    pub const HWLOC_OBJ_CORE: HwlocObjTypeT = 2;
    pub const HWLOC_OBJ_PU: HwlocObjTypeT = 3;
    pub const HWLOC_OBJ_L1CACHE: HwlocObjTypeT = 4;
    pub const HWLOC_OBJ_L2CACHE: HwlocObjTypeT = 5;
    pub const HWLOC_OBJ_L3CACHE: HwlocObjTypeT = 6;
    pub const HWLOC_OBJ_NUMANODE: HwlocObjTypeT = 13;

    pub const HWLOC_TYPE_DEPTH_UNKNOWN: c_int = -1;
    pub const HWLOC_TYPE_DEPTH_MULTIPLE: c_int = -2;

    pub const HWLOC_RESTRICT_FLAG_ADAPT_MISC: c_ulong = 1 << 1;
    pub const HWLOC_RESTRICT_FLAG_ADAPT_IO: c_ulong = 1 << 2;

    pub const HWLOC_DISTRIB_FLAG_REVERSE: c_ulong = 1 << 0;

    /// `struct hwloc_obj` of hwloc 2.x.
    #[repr(C)]
    pub struct HwlocObj {
        pub type_: HwlocObjTypeT,
        pub subtype: *mut c_char,
        pub os_index: c_uint,
        pub name: *mut c_char,
        pub total_memory: HwlocUint64T,
        pub attr: *mut c_void,
        pub depth: c_int,
        pub logical_index: c_uint,
        pub next_cousin: HwlocObjT,
        pub prev_cousin: HwlocObjT,
        pub parent: HwlocObjT,
        pub sibling_rank: c_uint,
        pub next_sibling: HwlocObjT,
        pub prev_sibling: HwlocObjT,
        pub arity: c_uint,
        pub children: *mut HwlocObjT,
        pub first_child: HwlocObjT,
        pub last_child: HwlocObjT,
        pub symmetric_subtree: c_int,
        pub memory_arity: c_uint,
        pub memory_first_child: HwlocObjT,
        pub io_arity: c_uint,
        pub io_first_child: HwlocObjT,
        pub misc_arity: c_uint,
        pub misc_first_child: HwlocObjT,
        pub cpuset: HwlocCpusetT,
        pub complete_cpuset: HwlocCpusetT,
        pub nodeset: HwlocNodesetT,
        pub complete_nodeset: HwlocNodesetT,
        pub infos: *mut c_void,
        pub infos_count: c_uint,
        pub userdata: *mut c_void,
        pub gp_index: HwlocUint64T,
    }

    #[link(name = "hwloc")]
    unsafe extern "C" {
        pub fn hwloc_topology_init(topologyp: *mut HwlocTopologyT) -> c_int;
        pub fn hwloc_topology_load(topology: HwlocTopologyT) -> c_int;
        pub fn hwloc_topology_destroy(topology: HwlocTopologyT);
        pub fn hwloc_topology_restrict(
            topology: HwlocTopologyT,
            set: HwlocConstBitmapT,
            flags: c_ulong,
        ) -> c_int;
        pub fn hwloc_get_type_depth(topology: HwlocTopologyT, type_: HwlocObjTypeT) -> c_int;
        pub fn hwloc_get_depth_type(topology: HwlocTopologyT, depth: c_int) -> HwlocObjTypeT;
        pub fn hwloc_get_nbobjs_by_depth(topology: HwlocTopologyT, depth: c_int) -> c_uint;
        pub fn hwloc_get_obj_by_depth(
            topology: HwlocTopologyT,
            depth: c_int,
            idx: c_uint,
        ) -> HwlocObjT;
        pub fn hwloc_compare_types(type1: HwlocObjTypeT, type2: HwlocObjTypeT) -> c_int;
        pub fn hwloc_obj_type_is_normal(type_: HwlocObjTypeT) -> c_int;
        pub fn hwloc_obj_type_string(type_: HwlocObjTypeT) -> *const c_char;
        pub fn hwloc_bitmap_alloc() -> HwlocBitmapT;
        pub fn hwloc_bitmap_free(bitmap: HwlocBitmapT);
        pub fn hwloc_bitmap_dup(bitmap: HwlocConstBitmapT) -> HwlocBitmapT;
        pub fn hwloc_bitmap_set(bitmap: HwlocBitmapT, id: c_uint) -> c_int;
        pub fn hwloc_bitmap_first(bitmap: HwlocConstBitmapT) -> c_int;
        pub fn hwloc_bitmap_next(bitmap: HwlocConstBitmapT, prev: c_int) -> c_int;
        pub fn hwloc_bitmap_weight(bitmap: HwlocConstBitmapT) -> c_int;
        pub fn hwloc_bitmap_intersects(a: HwlocConstBitmapT, b: HwlocConstBitmapT) -> c_int;
        pub fn hwloc_bitmap_or(
            res: HwlocBitmapT,
            a: HwlocConstBitmapT,
            b: HwlocConstBitmapT,
        ) -> c_int;
    }

    /// `hwloc_get_root_obj`
    pub unsafe fn hwloc_get_root_obj(topology: HwlocTopologyT) -> HwlocObjT {
        // SAFETY: forwarded contract: `topology` is loaded.
        unsafe { hwloc_get_obj_by_depth(topology, 0, 0) }
    }

    /// `hwloc_get_next_obj_by_depth`
    pub unsafe fn hwloc_get_next_obj_by_depth(
        topology: HwlocTopologyT,
        depth: c_int,
        prev: HwlocObjT,
    ) -> HwlocObjT {
        // SAFETY: forwarded contract: `prev` is null or an object of `topology`.
        unsafe {
            if prev.is_null() {
                return hwloc_get_obj_by_depth(topology, depth, 0);
            }
            if (*prev).depth != depth {
                return std::ptr::null_mut();
            }
            (*prev).next_cousin
        }
    }

    /// `hwloc_get_nbobjs_by_type`
    pub unsafe fn hwloc_get_nbobjs_by_type(
        topology: HwlocTopologyT,
        type_: HwlocObjTypeT,
    ) -> c_int {
        // SAFETY: forwarded contract.
        unsafe {
            let depth = hwloc_get_type_depth(topology, type_);
            if depth == HWLOC_TYPE_DEPTH_UNKNOWN {
                return 0;
            }
            if depth == HWLOC_TYPE_DEPTH_MULTIPLE {
                return -1; /* objects at several depths: their counts are not aggregated */
            }
            hwloc_get_nbobjs_by_depth(topology, depth) as c_int
        }
    }

    /// `hwloc_get_next_obj_by_type`
    pub unsafe fn hwloc_get_next_obj_by_type(
        topology: HwlocTopologyT,
        type_: HwlocObjTypeT,
        prev: HwlocObjT,
    ) -> HwlocObjT {
        // SAFETY: forwarded contract.
        unsafe {
            let depth = hwloc_get_type_depth(topology, type_);
            if depth == HWLOC_TYPE_DEPTH_UNKNOWN || depth == HWLOC_TYPE_DEPTH_MULTIPLE {
                return std::ptr::null_mut();
            }
            hwloc_get_next_obj_by_depth(topology, depth, prev)
        }
    }

    /// `hwloc_get_type_or_above_depth`
    pub unsafe fn hwloc_get_type_or_above_depth(
        topology: HwlocTopologyT,
        type_: HwlocObjTypeT,
    ) -> c_int {
        // SAFETY: forwarded contract.
        unsafe {
            let mut depth = hwloc_get_type_depth(topology, type_);

            if depth != HWLOC_TYPE_DEPTH_UNKNOWN {
                return depth;
            }

            /* find the highest existing level with type order >= */
            depth = hwloc_get_type_depth(topology, HWLOC_OBJ_PU);
            loop {
                if hwloc_compare_types(hwloc_get_depth_type(topology, depth), type_) < 0 {
                    return depth + 1;
                }
                depth -= 1;
            }
        }
    }

    /// `hwloc_get_type_depth` for a type known to exist.
    pub unsafe fn hwloc_get_type_depth_checked(
        topology: HwlocTopologyT,
        type_: HwlocObjTypeT,
    ) -> c_int {
        // SAFETY: forwarded contract.
        unsafe { hwloc_get_type_depth(topology, type_) }
    }

    /// `hwloc_get_pu_obj_by_os_index`
    pub unsafe fn hwloc_get_pu_obj_by_os_index(
        topology: HwlocTopologyT,
        os_index: c_uint,
    ) -> HwlocObjT {
        // SAFETY: forwarded contract.
        unsafe {
            let mut obj: HwlocObjT = std::ptr::null_mut();
            loop {
                obj = hwloc_get_next_obj_by_type(topology, HWLOC_OBJ_PU, obj);
                if obj.is_null() {
                    return std::ptr::null_mut();
                }
                if (*obj).os_index == os_index {
                    return obj;
                }
            }
        }
    }

    /// `hwloc_distrib`
    // `topology` is unused by the algorithm but kept so the signature matches the hwloc API.
    #[allow(clippy::only_used_in_recursion)]
    pub unsafe fn hwloc_distrib(
        topology: HwlocTopologyT,
        roots: *mut HwlocObjT,
        n_roots: c_uint,
        set: *mut HwlocCpusetT,
        n: c_uint,
        until: c_int,
        flags: c_ulong,
    ) -> c_int {
        // SAFETY: `roots` holds n_roots objects and `set` has room for n cpusets.
        unsafe {
            let mut cpusetp = set;

            if flags & !HWLOC_DISTRIB_FLAG_REVERSE != 0 {
                return -1;
            }

            let mut tot_weight: c_uint = 0;
            for i in 0..n_roots {
                tot_weight += hwloc_bitmap_weight((**roots.add(i as usize)).cpuset) as c_uint;
            }

            let mut given: c_uint = 0;
            let mut givenweight: c_uint = 0;
            for i in 0..n_roots {
                let idx = if flags & HWLOC_DISTRIB_FLAG_REVERSE != 0 { n_roots - 1 - i } else { i };
                let mut root = *roots.add(idx as usize);
                let cpuset = (*root).cpuset;
                while hwloc_obj_type_is_normal((*root).type_) == 0 {
                    /* If memory/io/misc, walk up to normal parent */
                    root = (*root).parent;
                }
                let weight = hwloc_bitmap_weight(cpuset) as c_uint;
                if weight == 0 {
                    continue;
                }
                /* Give to root a chunk proportional to its weight.
                 * If previous chunks got rounded-up, we may get a bit less. */
                let chunk = ((givenweight + weight) * n).div_ceil(tot_weight)
                    - (givenweight * n).div_ceil(tot_weight);
                if (*root).arity == 0 || chunk <= 1 || (*root).depth >= until {
                    /* We can't split any more, put everything there.  */
                    if chunk != 0 {
                        /* Fill cpusets with ours */
                        for j in 0..chunk {
                            *cpusetp.add(j as usize) = hwloc_bitmap_dup(cpuset);
                        }
                    } else {
                        /* We got no chunk, just merge our cpuset to a previous one
                         * (the first chunk cannot be empty)
                         * so that this root doesn't get ignored.
                         */
                        assert!(given != 0);
                        hwloc_bitmap_or(*cpusetp.sub(1), *cpusetp.sub(1), cpuset);
                    }
                } else {
                    /* Still more to distribute, recurse into children */
                    hwloc_distrib(
                        topology,
                        (*root).children,
                        (*root).arity,
                        cpusetp,
                        chunk,
                        until,
                        flags,
                    );
                }
                cpusetp = cpusetp.add(chunk as usize);
                given += chunk;
                givenweight += weight;
            }

            0
        }
    }

    /// `hwloc_cpuset_to_glibc_sched_affinity`
    pub unsafe fn hwloc_cpuset_to_glibc_sched_affinity(
        _topology: HwlocTopologyT,
        hwlocset: HwlocConstBitmapT,
        schedset: &mut super::CpuSetT,
    ) -> c_int {
        *schedset = super::CpuSetT::default();
        // SAFETY: `hwlocset` is a valid bitmap.
        unsafe {
            let mut cpu = hwloc_bitmap_first(hwlocset);
            while cpu != -1 {
                schedset.set(cpu as usize);
                cpu = hwloc_bitmap_next(hwlocset, cpu);
            }
        }
        0
    }
}

#[cfg(feature = "hwloc")]
mod with_hwloc {
    use super::hwloc_ffi::*;
    use super::*;
    use crate::engine::core::future::BadAlloc;
    use crate::log_error;
    use std::ffi::{c_int, c_uint};

    /// Returns a CPU mask with only `cpuid` set.
    pub fn cpuid_to_cpuset(cpuid: u32) -> CpuSetT {
        let mut cs = CpuSetT::default();
        cs.set(cpuid as usize);
        cs
    }

    /// Returns `num / denom` rounded up.
    pub fn div_roundup(num: usize, denom: usize) -> usize {
        num.div_ceil(denom)
    }

    fn get_memory_from_hwloc_obj(obj: HwlocObjT) -> HwlocUint64T {
        // HWLOC_API_VERSION >= 0x00020000
        // SAFETY: `obj` is an object of a loaded topology.
        unsafe { (*obj).total_memory }
    }

    fn set_memory_to_hwloc_obj(machine: HwlocObjT, memory: HwlocUint64T) {
        // SAFETY: `machine` is an object of a loaded topology.
        unsafe { (*machine).total_memory = memory };
    }

    fn alloc_from_node(
        this_cpu: &mut Cpu,
        node: HwlocObjT,
        used_mem: &mut UnorderedMap<HwlocObjT, usize>,
        alloc: usize,
    ) -> usize {
        let local_memory = get_memory_from_hwloc_obj(node) as usize;
        let used = used_mem.entry(node).or_insert(0);
        let taken = (local_memory - *used).min(alloc);
        if taken != 0 {
            *used += taken;
            // SAFETY: `node` is a NUMA node object of a loaded topology.
            let node_id = unsafe { hwloc_bitmap_first((*node).nodeset) };
            assert!(node_id != -1);
            this_cpu.mem.push(Memory { bytes: taken, nodeid: node_id as u32 });
            log_debug!(
                engine_logger(),
                "CPU{} allocated {} bytes from NODE{}",
                this_cpu.cpu_id,
                taken,
                node_id
            );
        }
        taken
    }

    // Find the numa node that contains a specific PU.
    fn get_numa_node_for_pu(topology: HwlocTopologyT, pu: HwlocObjT) -> HwlocObjT {
        // Can't use ancestry because hwloc 2.0 NUMA nodes are not ancestors of PUs
        let mut tmp: HwlocObjT = std::ptr::null_mut();
        // SAFETY: walking objects of a loaded topology.
        unsafe {
            let depth = hwloc_get_type_or_above_depth(topology, HWLOC_OBJ_NUMANODE);
            loop {
                tmp = hwloc_get_next_obj_by_depth(topology, depth, tmp);
                if tmp.is_null() {
                    break;
                }
                if hwloc_bitmap_intersects((*tmp).cpuset, (*pu).cpuset) != 0 {
                    return tmp;
                }
            }
        }
        std::ptr::null_mut()
    }

    fn hwloc_get_ancestor(
        type_: HwlocObjTypeT,
        topology: HwlocTopologyT,
        cpu_id: u32,
    ) -> HwlocObjT {
        // SAFETY: walking objects of a loaded topology.
        unsafe {
            let mut cur = hwloc_get_pu_obj_by_os_index(topology, cpu_id);

            while !cur.is_null() {
                if (*cur).type_ == type_ {
                    break;
                }
                cur = (*cur).parent;
            }

            cur
        }
    }

    fn break_cpus_into_groups(
        topology: HwlocTopologyT,
        cpus: Vec<u32>,
        type_: HwlocObjTypeT,
    ) -> UnorderedMap<HwlocObjT, Vec<u32>> {
        let mut groups: UnorderedMap<HwlocObjT, Vec<u32>> = UnorderedMap::default();

        for cpu_id in cpus {
            let anc = hwloc_get_ancestor(type_, topology, cpu_id);
            groups.entry(anc).or_default().push(cpu_id);
        }

        groups
    }

    struct DistributeObjects {
        cpu_sets: Vec<HwlocCpusetT>,
    }

    impl DistributeObjects {
        fn new(topology: HwlocTopologyT, nobjs: usize) -> Self {
            let mut cpu_sets = vec![std::ptr::null_mut(); nobjs];
            // SAFETY: the topology is loaded.
            let mut root = unsafe { hwloc_get_root_obj(topology) };
            // HWLOC_API_VERSION >= 0x00010900
            // SAFETY: one root, and room for nobjs cpusets.
            unsafe {
                hwloc_distrib(
                    topology,
                    &mut root,
                    1,
                    cpu_sets.as_mut_ptr(),
                    cpu_sets.len() as c_uint,
                    c_int::MAX,
                    0,
                )
            };
            Self { cpu_sets }
        }

        fn call(&self) -> &Vec<HwlocCpusetT> {
            &self.cpu_sets
        }
    }

    impl Drop for DistributeObjects {
        fn drop(&mut self) {
            for &cs in &self.cpu_sets {
                // SAFETY: each cpuset was allocated by hwloc_bitmap_dup (freeing null is a no-op).
                unsafe { hwloc_bitmap_free(cs) };
            }
        }
    }

    fn node_id_of(node: HwlocObjT) -> u32 {
        // SAFETY: `node` is a NUMA node object of a loaded topology.
        unsafe { hwloc_bitmap_first((*node).nodeset) as u32 }
    }

    fn allocate_io_queues(
        topology: HwlocTopologyT,
        cpus: &[Cpu],
        cpu_to_node: &UnorderedMap<u32, HwlocObjT>,
        mut num_io_groups: u32,
        last_node_idx: &mut u32,
    ) -> IoQueueTopology {
        let node_of_shard = |shard: u32| -> u32 {
            let node = cpu_to_node[&cpus[shard as usize].cpu_id];
            node_id_of(node)
        };

        // There are two things we are trying to achieve by populating a numa_nodes map.
        //
        // The first is to find out how many nodes we have in the system. We can't use
        // hwloc for that, because at this point we are not longer talking about the physical system,
        // but the actual booted engine server instead. So if we have restricted the run to a subset
        // of the available processors, counting topology nodes won't spur the same result.
        //
        // Secondly, we need to find out which processors live in each node. For a reason similar to the
        // above, hwloc won't do us any good here. Later on, we will use this information to assign
        // shards to coordinators that are node-local to themselves.
        let mut numa_nodes: UnorderedMap<u32, BTreeSet<u32>> = UnorderedMap::default();
        for shard in 0..cpus.len() as u32 {
            let node_id = node_of_shard(shard);

            numa_nodes.entry(node_id).or_default().insert(shard);
        }

        let mut ret = IoQueueTopology::new();
        ret.shard_to_group.resize(cpus.len(), 0);
        ret.shards_in_group.resize(cpus.len(), 0); // worst case

        if num_io_groups == 0 {
            num_io_groups = numa_nodes.len() as u32;
            assert!(num_io_groups != 0);
            log_debug!(engine_logger(), "Auto-configure {} IO groups", num_io_groups);
        } else if num_io_groups as usize > cpus.len() {
            // User may be playing with --smp option, but num_io_groups was independently
            // determined by iotune, so adjust for any conflicts.
            println!(
                "Warning: number of IO queues ({}) greater than logical cores ({}). Adjusting downwards.",
                num_io_groups,
                cpus.len()
            );
            num_io_groups = cpus.len() as u32;
        }

        let find_shard = |cpu_id: u32| -> u32 {
            for (idx, c) in cpus.iter().enumerate() {
                if c.cpu_id == cpu_id {
                    return idx as u32;
                }
            }
            panic!("assertion failed: 0");
        };

        let cpu_sets = DistributeObjects::new(topology, num_io_groups as usize);
        ret.queues.resize_with(cpus.len(), || std::cell::UnsafeCell::new(None));
        let mut nr_groups: u32 = 0;

        // First step: distribute the IO queues given the information returned in cpu_sets.
        // If there is one IO queue per processor, only this loop will be executed.
        let mut node_coordinators: UnorderedMap<u32, Vec<u32>> = UnorderedMap::default();
        for &cs in cpu_sets.call() {
            // SAFETY: `cs` is a cpuset produced by hwloc_distrib.
            let io_coordinator = find_shard(unsafe { hwloc_bitmap_first(cs) } as u32);
            let group_idx = nr_groups;
            nr_groups += 1;
            ret.shard_to_group[io_coordinator as usize] = group_idx;
            ret.shards_in_group[group_idx as usize] += 1;

            let node_id = node_of_shard(io_coordinator);
            node_coordinators.entry(node_id).or_default().push(io_coordinator);
            numa_nodes.entry(node_id).or_default().remove(&io_coordinator);
        }

        ret.groups.get_mut().resize_with(nr_groups as usize, || None);

        let available_nodes: Vec<u32> = node_coordinators.keys().copied().collect();

        // If there are more processors than coordinators, we will have to assign them to existing
        // coordinators. We prefer do that within the same NUMA node, but if not possible we assign
        // the shard to a random node.
        for (&node_first, node_second) in &numa_nodes {
            for (cid_idx, &remaining_shard) in node_second.iter().enumerate() {
                let mut my_node = node_first;
                // No I/O queue in this node, round-robin shards from this node into existing ones.
                if !node_coordinators.contains_key(&node_first) {
                    my_node = available_nodes[*last_node_idx as usize % available_nodes.len()];
                    *last_node_idx += 1;
                }
                let coords = &node_coordinators[&my_node];
                let idx = cid_idx % coords.len();
                let io_coordinator = coords[idx];
                let group_idx = ret.shard_to_group[io_coordinator as usize];
                ret.shard_to_group[remaining_shard as usize] = group_idx;
                ret.shards_in_group[group_idx as usize] += 1;
            }
        }

        ret
    }

    fn numa_node_id_to_cpuset(topo: HwlocTopologyT) -> UnorderedMap<u32, Cpuset> {
        let mut ret: UnorderedMap<u32, Cpuset> = UnorderedMap::default();
        // SAFETY: walking objects of a loaded topology.
        unsafe {
            let mut numa_node =
                hwloc_get_next_obj_by_type(topo, HWLOC_OBJ_NUMANODE, std::ptr::null_mut());
            while !numa_node.is_null() {
                let parent = (*numa_node).parent;
                let cpuset = (*parent).cpuset;
                let mut os_cpuset = CpuSetT::default();
                hwloc_cpuset_to_glibc_sched_affinity(topo, cpuset, &mut os_cpuset);
                for idx in 0..CPU_SETSIZE {
                    if os_cpuset.is_set(idx) {
                        ret.entry((*numa_node).os_index).or_default().insert(idx as u32);
                    }
                }
                numa_node = hwloc_get_next_obj_by_type(topo, HWLOC_OBJ_NUMANODE, numa_node);
            }
        }
        ret
    }

    struct BitmapGuard(HwlocBitmapT);

    impl Drop for BitmapGuard {
        fn drop(&mut self) {
            // SAFETY: the bitmap was allocated by hwloc_bitmap_alloc.
            unsafe { hwloc_bitmap_free(self.0) };
        }
    }

    /// Decides the CPUs and memory of every shard and the layout of the I/O queues, from the
    /// hwloc topology.
    ///
    /// The topology is restricted to `c.cpu_set`; memory is split per NUMA node among the shards
    /// local to it (at most 64 GiB per shard, so that addresses fit the allocator's 36-bit
    /// layout), and the shards are grouped into I/O groups per `c.num_io_groups`.
    ///
    /// # Errors
    /// Fails if the CPU set is invalid or larger than the machine, if the requested memory
    /// exceeds what is available, or if a CPU has no local NUMA node and
    /// `c.assign_orphan_cpus` is not set.
    pub fn allocate(c: &mut Configuration) -> Result<Resources, ExceptionPtr> {
        let topology = c.topology.get();
        // SAFETY: allocating an empty bitmap.
        let bm = unsafe { hwloc_bitmap_alloc() };
        let _free_bm = BitmapGuard(bm);
        for &idx in &c.cpu_set {
            // SAFETY: `bm` is a valid bitmap.
            unsafe { hwloc_bitmap_set(bm, idx) };
        }
        // SAFETY: restricting a loaded topology to a valid cpuset.
        let r = unsafe {
            hwloc_topology_restrict(
                topology,
                bm,
                HWLOC_RESTRICT_FLAG_ADAPT_MISC | HWLOC_RESTRICT_FLAG_ADAPT_IO,
            )
        };
        if r == -1 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if errno == 12
            /* ENOMEM */
            {
                return Err(make_exception_ptr(BadAlloc));
            }
            if errno == 22
            /* EINVAL */
            {
                return Err(make_exception_ptr(RuntimeError::new("bad cpuset".to_string())));
            }
            std::process::abort();
        }
        let procs = c.cpus as u32;
        // SAFETY: the topology is loaded.
        let available_procs = unsafe { hwloc_get_nbobjs_by_type(topology, HWLOC_OBJ_PU) } as u32;
        if procs > available_procs {
            return Err(make_exception_ptr(RuntimeError::new(format!(
                "insufficient processing units: needed {} available {}",
                procs, available_procs
            ))));
        }
        if procs == 0 {
            return Err(make_exception_ptr(RuntimeError::new(
                "number of processing units must be positive".to_string(),
            )));
        }

        let mut available_memory: usize = 0;

        // Get the list of NUMA nodes available
        let mut nodes: Vec<HwlocObjT> = Vec::new();

        let mut tmp: HwlocObjT = std::ptr::null_mut();
        // SAFETY: walking objects of a loaded topology.
        let num_nodes = unsafe { hwloc_get_nbobjs_by_type(topology, HWLOC_OBJ_NUMANODE) };
        assert!(num_nodes > 0);
        // SAFETY: as above.
        let nodes_depth = unsafe { hwloc_get_type_or_above_depth(topology, HWLOC_OBJ_NUMANODE) };
        loop {
            // SAFETY: as above.
            tmp = unsafe { hwloc_get_next_obj_by_depth(topology, nodes_depth, tmp) };
            if tmp.is_null() {
                break;
            }
            available_memory += get_memory_from_hwloc_obj(tmp) as usize;
            nodes.push(tmp);
        }

        if available_memory == 0 {
            // SAFETY: as above.
            let machine = unsafe {
                let machine_depth = hwloc_get_type_depth_checked(topology, HWLOC_OBJ_MACHINE);
                assert!(hwloc_get_nbobjs_by_depth(topology, machine_depth) == 1);
                hwloc_get_obj_by_depth(topology, machine_depth, 0)
            };
            available_memory = get_memory_from_hwloc_obj(machine) as usize;
            if available_memory == 0 {
                available_memory = get_machine_memory_from_sysconf() as usize;
                set_memory_to_hwloc_obj(machine, available_memory as u64);
                log_warn!(
                    engine_logger(),
                    "hwloc failed to detect machine-wide memory size, using memory size fetched from sysconf"
                );
            }

            let one_node_mem = available_memory / num_nodes as usize;
            let mut one_node_mem_remainder = available_memory % num_nodes as usize;
            for &node in &nodes {
                set_memory_to_hwloc_obj(node, (one_node_mem + one_node_mem_remainder) as u64);
                one_node_mem_remainder = 0;
            }
        }

        let mem = calculate_memory(c, available_memory.min(cgroup::memory_limit()), 1.0)?;
        // Limit each shard's memory so that its addresses fit in 36 bits (the allocator's layout).
        const MAX_MEM_PER_PROC: usize = 1usize << 36;
        let mem_per_proc =
            crate::engine::core::align::align_down::<usize>(mem / procs as usize, 2 << 20)
                .min(MAX_MEM_PER_PROC);

        let mut ret = Resources::default();
        let mut cpu_to_node: UnorderedMap<u32, HwlocObjT> = UnorderedMap::default();
        let mut orphan_pus: Vec<u32> = Vec::new();
        let mut topo_used_mem: UnorderedMap<HwlocObjT, usize> = UnorderedMap::default();
        let mut remains: Vec<(Cpu, usize)> = Vec::new();

        let cpu_sets = DistributeObjects::new(topology, procs as usize);

        for &cs in cpu_sets.call() {
            // SAFETY: `cs` is a cpuset produced by hwloc_distrib.
            let cpu_id = unsafe { hwloc_bitmap_first(cs) };
            assert!(cpu_id != -1);
            // SAFETY: as above.
            let pu = unsafe { hwloc_get_pu_obj_by_os_index(topology, cpu_id as u32) };
            let node = get_numa_node_for_pu(topology, pu);
            if node.is_null() {
                orphan_pus.push(cpu_id as u32);
            } else {
                cpu_to_node.insert(cpu_id as u32, node);
                // SAFETY: `node` is a NUMA node object.
                log_debug!(engine_logger(), "Assign CPU{} to NUMA{}", cpu_id, unsafe {
                    (*node).os_index
                });
            }
        }

        if !orphan_pus.is_empty() {
            if !c.assign_orphan_cpus {
                log_error!(
                    engine_logger(),
                    "CPUs without local NUMA nodes are disabled by the --allow-cpus-in-remote-numa-nodes=false option.\n"
                );
                return Err(make_exception_ptr(RuntimeError::new(
                    "no NUMA node for CPU".to_string(),
                )));
            }

            log_warn!(engine_logger(), "Assigning some CPUs to remote NUMA nodes");

            // Group orphan CPUs by ... some sane enough feature
            let mut grouped: UnorderedMap<HwlocObjT, Vec<u32>> = UnorderedMap::default();
            let group_by: [HwlocObjTypeT; 4] =
                [HWLOC_OBJ_L3CACHE, HWLOC_OBJ_L2CACHE, HWLOC_OBJ_L1CACHE, HWLOC_OBJ_PU];

            for gb in group_by {
                grouped = break_cpus_into_groups(topology, orphan_pus.clone(), gb);
                if grouped.len() >= nodes.len() {
                    // SAFETY: hwloc returns a static string.
                    let name = unsafe { std::ffi::CStr::from_ptr(hwloc_obj_type_string(gb)) };
                    log_debug!(
                        engine_logger(),
                        "Grouped orphan CPUs by {}",
                        name.to_string_lossy()
                    );
                    break;
                }
                // Try to scatter orphans into as much NUMA nodes as possible
                // by grouping them with more specific selection
            }

            // Distribute PUs among the nodes by groups
            let mut nid: usize = 0;
            for grp in grouped.values() {
                for &cpu_id in grp {
                    cpu_to_node.insert(cpu_id, nodes[nid]);
                    // SAFETY: nodes are NUMA node objects.
                    log_debug!(engine_logger(), "Assign orphan CPU{} to NUMA{}", cpu_id, unsafe {
                        (*nodes[nid]).os_index
                    });
                }
                nid = (nid + 1) % nodes.len();
            }
        }

        // Divide local memory to cpus
        for &cs in cpu_sets.call() {
            // SAFETY: `cs` is a cpuset produced by hwloc_distrib.
            let cpu_id = unsafe { hwloc_bitmap_first(cs) };
            assert!(cpu_id != -1);
            let node = cpu_to_node[&(cpu_id as u32)];
            let mut this_cpu = Cpu { cpu_id: cpu_id as u32, ..Cpu::default() };
            let remain = mem_per_proc
                - alloc_from_node(&mut this_cpu, node, &mut topo_used_mem, mem_per_proc);

            remains.push((this_cpu, remain));
        }

        // Divide the rest of the memory
        for (mut this_cpu, mut remain) in remains {
            let node = cpu_to_node[&this_cpu.cpu_id];
            let mut obj = node;

            while remain != 0 {
                remain -= alloc_from_node(&mut this_cpu, obj, &mut topo_used_mem, remain);
                loop {
                    // SAFETY: walking objects of a loaded topology.
                    obj = unsafe { hwloc_get_next_obj_by_depth(topology, nodes_depth, obj) };
                    if !obj.is_null() {
                        break;
                    }
                }
                if obj == node {
                    break;
                }
            }
            assert!(remain == 0);
            ret.cpus.push(this_cpu);
        }

        let mut last_node_idx: u32 = 0;
        for &q in &c.io_queues {
            let t = allocate_io_queues(
                topology,
                &ret.cpus,
                &cpu_to_node,
                c.num_io_groups,
                &mut last_node_idx,
            );
            ret.ioq_topology.entry(q).or_insert(t);
        }

        ret.numa_node_id_to_cpuset = numa_node_id_to_cpuset(topology);

        Ok(ret)
    }

    /// Returns the number of processing units (hardware threads) available to the process.
    pub fn nr_processing_units(c: &mut Configuration) -> u32 {
        // SAFETY: the topology is loaded by get().
        unsafe { hwloc_get_nbobjs_by_type(c.topology.get(), HWLOC_OBJ_PU) as u32 }
    }
}

#[cfg(feature = "hwloc")]
pub use with_hwloc::{allocate, cpuid_to_cpuset, div_roundup, nr_processing_units};

#[cfg(not(feature = "hwloc"))]
mod without_hwloc {
    use super::*;

    // Without hwloc the number of I/O groups cannot be tuned: all shards share one group.
    fn allocate_io_queues(_c: &Configuration, cpus: &[Cpu]) -> IoQueueTopology {
        let mut ret = IoQueueTopology::new();

        let nr_cpus = cpus.len() as u32;
        ret.queues.resize_with(nr_cpus as usize, || std::cell::UnsafeCell::new(None));
        ret.shard_to_group.resize(nr_cpus as usize, 0);
        ret.shards_in_group.resize(1, 0);
        ret.groups.get_mut().resize_with(1, || None);

        for shard in 0..nr_cpus {
            ret.shard_to_group[shard as usize] = 0;
            ret.shards_in_group[0] += 1;
        }
        ret
    }

    /// Decides the CPUs and memory of every shard and the layout of the I/O queues.
    ///
    /// Each CPU of `c.cpu_set` becomes one shard with an equal share of the usable memory (see
    /// [`calculate_memory`]), at most 64 GiB per shard so that addresses fit the allocator's
    /// 36-bit layout.
    ///
    /// # Errors
    /// Fails if the requested memory exceeds what is available.
    pub fn allocate(c: &mut Configuration) -> Result<Resources, ExceptionPtr> {
        let mut ret = Resources::default();

        let available_memory = get_machine_memory_from_sysconf() as usize;
        let mem = calculate_memory(c, available_memory, 1.0)?;
        let procs = c.cpus;
        ret.cpus.reserve(procs);
        // Limit each shard's memory so that its addresses fit in 36 bits (the allocator's layout).
        const MAX_MEM_PER_PROC: usize = 1usize << 36;
        let mem_per_proc = (mem / procs).min(MAX_MEM_PER_PROC);
        for &cpuid in &c.cpu_set {
            ret.cpus
                .push(Cpu { cpu_id: cpuid, mem: vec![Memory { bytes: mem_per_proc, nodeid: 0 }] });
        }

        let t = allocate_io_queues(c, &ret.cpus);
        ret.ioq_topology.entry(0).or_insert(t);
        Ok(ret)
    }

    /// Returns the number of processing units (hardware threads) available to the process.
    pub fn nr_processing_units(_c: &mut Configuration) -> u32 {
        // SAFETY: sysconf has no preconditions.
        unsafe { sysconf(_SC_NPROCESSORS_ONLN) as u32 }
    }
}

#[cfg(not(feature = "hwloc"))]
pub use without_hwloc::{allocate, nr_processing_units};
