//! Tests of the resource planner without a reactor: the identity hash of its maps, the memory
//! the shards get (reserves, the low-memory floor, an explicit total and its errors), the
//! allocation of CPUs, memory and I/O queues to shards, the I/O queue topology, and the cgroup
//! settings of this process.
#![cfg(target_os = "linux")]

use std::hash::{BuildHasher, Hasher};

use teseladb::engine::core::resource::{
    Configuration, Cpuset, IdentityHasher, IoQueueTopology, Memory, UnorderedMap, allocate,
    calculate_memory, cgroup, nr_processing_units,
};

const GIB: usize = 1 << 30;
const MIB: usize = 1 << 20;
/// The floor `calculate_memory` grants when the reserve leaves less.
const MIN_MEMORY: usize = 500_000_000;

/// The memory the kernel keeps free, as `calculate_memory` reads it.
fn kernel_reservation() -> usize {
    std::fs::read_to_string("/proc/sys/vm/min_free_kbytes")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .map_or(0, |kb| kb * 1024)
}

fn config(
    reserve: Option<usize>,
    total: Option<usize>,
    per_shard: usize,
    cpus: usize,
) -> Configuration {
    Configuration {
        total_memory: total,
        reserve_memory: reserve,
        reserve_additional_memory_per_shard: per_shard,
        cpus,
        ..Configuration::default()
    }
}

#[test]
fn identity_hasher_returns_the_key() {
    let mut h = IdentityHasher::default();
    h.write_u32(7);
    assert_eq!(h.finish(), 7);
    h.write_u64(1 << 40);
    assert_eq!(h.finish(), 1 << 40);
    h.write_usize(99);
    assert_eq!(h.finish(), 99);
    let mut bytes = IdentityHasher::default();
    bytes.write(&[0x12, 0x34]);
    assert_eq!(bytes.finish(), 0x1234);
    // The maps hash integer keys to themselves.
    let mut m: UnorderedMap<u32, &str> = UnorderedMap::default();
    m.insert(3, "three");
    m.insert(1, "one");
    assert_eq!(m[&3], "three");
    assert_eq!(m.hasher().hash_one(5u64), 5);
}

#[test]
fn memory_left_after_the_reserves() {
    let kr = kernel_reservation();
    // An explicit reserve is subtracted with the kernel's.
    let c = config(Some(GIB), None, 0, 1);
    assert_eq!(calculate_memory(&c, 64 * GIB, 1.0).unwrap(), 64 * GIB - kr - GIB);
    // The per-shard reserve adds up over the shards.
    let c = config(Some(GIB), None, 16 * MIB, 4);
    assert_eq!(calculate_memory(&c, 64 * GIB, 1.0).unwrap(), 64 * GIB - kr - GIB - 64 * MIB);
    // The default reserve is the larger of 1.5 GiB and 7%, times the panic factor.
    let c = config(None, None, 0, 1);
    let avail = 100 * GIB - kr;
    let reserve = ((1536 * MIB).max((0.07 * avail as f64) as usize) as f64 * 2.0) as usize;
    assert_eq!(calculate_memory(&c, 100 * GIB, 2.0).unwrap(), avail - reserve);
    // A small machine still gets the floor.
    assert_eq!(calculate_memory(&c, GIB, 1.0).unwrap(), MIN_MEMORY);
}

#[test]
fn an_explicit_total_must_fit() {
    // The total minus the per-shard reserve, when it fits.
    let c = config(Some(0), Some(GIB), MIB, 4);
    assert_eq!(calculate_memory(&c, 64 * GIB, 1.0).unwrap(), GIB - 4 * MIB);
    // Less than the per-shard reserve.
    let c = config(Some(0), Some(MIB), MIB, 4);
    let e = calculate_memory(&c, 64 * GIB, 1.0).unwrap_err();
    assert!(e.to_string().contains("insufficient total memory"), "{e}");
    // More than the machine has.
    let c = config(Some(0), Some(1 << 50), 0, 1);
    let e = calculate_memory(&c, 64 * GIB, 1.0).unwrap_err();
    assert!(e.to_string().contains("insufficient physical memory"), "{e}");
}

// The planner of a build without hwloc: one I/O queue topology, for device 0, whatever the
// configuration names, and no NUMA map (the reference's allocate() without hwloc).
#[cfg(not(feature = "hwloc"))]
#[test]
fn allocation_gives_each_cpu_a_shard_and_one_io_group() {
    let mut c = config(Some(0), Some(64 * MIB), 0, 2);
    c.cpu_set = Cpuset::from([0, 1]);
    let r = allocate(&mut c).unwrap();
    let cpus: Vec<u32> = r.cpus.iter().map(|cpu| cpu.cpu_id).collect();
    assert_eq!(cpus, [0, 1]);
    for cpu in &r.cpus {
        assert_eq!(cpu.mem, [Memory { bytes: 32 * MIB, nodeid: 0 }]);
    }
    let topo = &r.ioq_topology[&0];
    assert_eq!(topo.queues.len(), 2);
    assert_eq!(topo.shard_to_group, [0, 0]);
    assert_eq!(topo.shards_in_group, [2]);
    assert!(r.numa_node_id_to_cpuset.is_empty());

    // Asking for more memory than the machine has fails.
    let mut c = config(Some(0), Some(1 << 50), 0, 1);
    c.cpu_set = Cpuset::from([0]);
    assert!(allocate(&mut c).is_err());

    assert!(nr_processing_units(&mut c) >= 1);
}

// The planner over hwloc: every CPU gets a shard and its share of the memory, an I/O queue
// topology exists for each device the configuration names (none otherwise), and the NUMA map
// covers the CPUs (the reference's allocate() with hwloc).
#[cfg(feature = "hwloc")]
#[test]
fn allocation_over_hwloc_follows_the_configured_devices() {
    let mut c = config(Some(0), Some(64 * MIB), 0, 2);
    c.cpu_set = Cpuset::from([0, 1]);
    let r = allocate(&mut c).unwrap();
    let cpus: Vec<u32> = r.cpus.iter().map(|cpu| cpu.cpu_id).collect();
    assert_eq!(cpus, [0, 1]);
    for cpu in &r.cpus {
        assert_eq!(cpu.mem.iter().map(|m: &Memory| m.bytes).sum::<usize>(), 32 * MIB);
    }
    assert!(r.ioq_topology.is_empty(), "no device was configured");
    assert!(!r.numa_node_id_to_cpuset.is_empty());
    assert!(r.numa_node_id_to_cpuset.values().any(|set| set.contains(&0)));

    let mut c = config(Some(0), Some(64 * MIB), 0, 2);
    c.cpu_set = Cpuset::from([0, 1]);
    c.io_queues = vec![7];
    let r = allocate(&mut c).unwrap();
    assert_eq!(r.ioq_topology.len(), 1);
    let topo = &r.ioq_topology[&7];
    assert_eq!(topo.queues.len(), 2);
    assert_eq!(topo.shard_to_group.len(), 2);
    assert_eq!(topo.shards_in_group.iter().sum::<u32>(), 2);

    // One I/O group for both shards: the second joins the first one's group.
    c.num_io_groups = 1;
    let r = allocate(&mut c).unwrap();
    let topo = &r.ioq_topology[&7];
    assert_eq!(topo.shard_to_group, [0, 0]);
    assert_eq!(topo.shards_in_group[0], 2);
    // More groups than shards are cut down to one per shard.
    c.num_io_groups = 5;
    let r = allocate(&mut c).unwrap();
    let topo = &r.ioq_topology[&7];
    let mut groups = topo.shard_to_group.clone();
    groups.sort_unstable();
    assert_eq!(groups, [0, 1]);

    // Asking for more memory than the machine has fails.
    let mut c = config(Some(0), Some(1 << 50), 0, 1);
    c.cpu_set = Cpuset::from([0]);
    assert!(allocate(&mut c).is_err());
    assert!(nr_processing_units(&mut c) >= 1);
}

// More shards than the CPU set holds, or none, fail the allocation with the reference's
// messages (resource.cc, allocate() with hwloc) instead of planning shards without a CPU.
#[test]
fn allocation_refuses_more_shards_than_cpus_and_zero_shards() {
    let mut c = config(Some(0), Some(64 * MIB), 0, 3);
    c.cpu_set = Cpuset::from([0, 1]);
    let e = allocate(&mut c).err().expect("three shards on two CPUs");
    assert!(e.to_string().contains("insufficient processing units: needed 3 available 2"), "{e}");

    let mut c = config(Some(0), Some(64 * MIB), 0, 0);
    c.cpu_set = Cpuset::from([0, 1]);
    let e = allocate(&mut c).err().expect("no shards");
    assert!(e.to_string().contains("number of processing units must be positive"), "{e}");
}

#[test]
fn io_queue_topologies_move_their_contents() {
    let mut a = IoQueueTopology {
        shard_to_group: vec![0, 1, 1],
        shards_in_group: vec![1, 2],
        ..IoQueueTopology::default()
    };
    a.queues.resize_with(3, || std::cell::UnsafeCell::new(None));
    a.groups.get_mut().resize_with(2, || None);
    let mut b = IoQueueTopology::take_from(&mut a);
    assert_eq!(b.shard_to_group, [0, 1, 1]);
    assert_eq!(b.shards_in_group, [1, 2]);
    assert_eq!(b.queues.len(), 3);
    assert_eq!(b.groups.get_mut().len(), 2);
    assert!(a.shard_to_group.is_empty() && a.queues.is_empty());
    assert!(a.groups.get_mut().is_empty());
    let fresh = IoQueueTopology::new();
    assert!(fresh.shards_in_group.is_empty());
}

#[test]
fn cgroup_settings_of_this_process() {
    let v2 = std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists();
    // Every cgroup v2 directory lists its processes, so the lowest one holding the file is
    // this process's own cgroup (or an ancestor); a v1 path that does not exist reads as none.
    let first =
        cgroup::read_setting_v1v2_as::<u32>("nosuch/cgroup.procs".into(), "cgroup.procs".into());
    assert_eq!(first.is_some(), v2);
    // A list of controller names is not a number.
    assert_eq!(
        cgroup::read_setting_v1v2_as::<u64>("nosuch/x".into(), "cgroup.controllers".into()),
        None
    );
    // `memory.max` reads "max" without a limit, which is no limit at all.
    let limit = cgroup::memory_limit();
    assert!(limit > 0);
    let cpus = cgroup::cpu_set();
    if let Some(cpus) = cpus {
        assert!(!cpus.is_empty());
    }
}
