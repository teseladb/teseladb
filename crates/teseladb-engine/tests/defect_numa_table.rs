#![allow(unsafe_code)]
#![cfg(target_os = "linux")]
//! The shard-to-NUMA-node table has one entry per shard, in shard order.

use teseladb::engine::core::reactor::engine;
use teseladb::engine::core::smp::Smp;
use teseladb::engine_thread_test_case;

engine_thread_test_case!(the_numa_table_has_one_entry_per_shard, {
    // SAFETY: on this shard's reactor thread; the reference is used for this call only.
    let smp = unsafe { (*engine()).smp() };
    let table = smp.shard_to_numa_node_mapping().to_vec();
    assert_eq!(table.len(), Smp::count() as usize, "{table:?}");
});
