//! What each range publishes (FR-022): the applied index, how long proposals take, the reads served
//! with the lease and with ReadIndex, the requests that waited for a latch, and the node's clock
//! offset. The host counts proposals and latch waits; the core counts reads and keeps the offset.

use teseladb_types::Histogram;

/// The metrics of one range on this replica.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RangeMetrics {
    /// Highest index applied here.
    pub applied_index: u64,
    /// From a write's proposal to the core's answer, in nanoseconds (on the leader).
    pub propose_latency: Histogram,
    /// Reads served with the lease.
    pub lease_reads: u64,
    /// Reads that confirmed the leadership with ReadIndex.
    pub read_index_reads: u64,
    /// Reads and writes that waited for another write's latch (each counted once).
    pub latch_waits: u64,
    /// This node's clock minus the median of the voters heard in the last second, in ms.
    pub clock_offset_ms: Option<i64>,
}

/// What the host counts for a range.
#[derive(Clone, Debug, Default)]
pub(crate) struct Counters {
    pub(crate) propose_latency: Histogram,
    pub(crate) latch_waits: u64,
}
