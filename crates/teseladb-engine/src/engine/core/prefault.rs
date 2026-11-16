//! Background pre-faulting of the memory given to the shards.
//!
//! The first touch of each page of a large anonymous mapping costs a soft page fault. To keep that
//! latency out of request handling, [`MemoryPrefaulter`](internal::MemoryPrefaulter) starts worker
//! threads at start-up that touch every page of each shard's memory ranges, one thread per NUMA node.
//! Its methods are implemented next to the SMP start-up code in
//! [`smp`](crate::engine::core::smp).

/// The pre-faulter type, used by the SMP start-up code.
pub mod internal {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU32};

    use crate::engine::core::memory::internal::MemoryRange;
    use crate::engine::core::posix::PosixThread;

    /// Touches the shards' memory in background threads so that soft page faults do not hit
    /// application code later.
    ///
    /// It lives in a heap node owned by [`MemoryPrefaulterPtr`](crate::engine::core::smp::MemoryPrefaulterPtr),
    /// because the worker threads keep its address; dropping that owner asks the workers to stop
    /// and joins them. The workers run at `SCHED_IDLE` priority.
    pub struct MemoryPrefaulter {
        /// Set to ask the worker threads to stop early.
        pub(crate) _stop_request: AtomicBool,
        /// The worker threads, one per NUMA node with memory to touch.
        pub(crate) _worker_threads: Vec<PosixThread>,
        /// Memory ranges to touch, grouped by NUMA node id. Built before the workers start so that
        /// they do not allocate.
        pub(crate) _layout_by_node_id: HashMap<u32, Vec<MemoryRange>>,
        /// Number of worker threads still running.
        pub(crate) _active_threads: AtomicU32,
    }
}
