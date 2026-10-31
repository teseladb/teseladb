//! Engine counters.

use teseladb_types::Histogram;

/// Counters of a core. Read by `teseladb-server`'s metrics exporter.
#[derive(Clone, Debug, Default)]
pub struct StorageMetrics {
    /// Bytes written to the WAL.
    pub wal_bytes_written: u64,
    /// Syncs (completed `Sync`s).
    pub syncs: u64,
    /// Closed batches.
    pub batches: u64,
    /// Requests (tickets) served by those batches.
    pub batched_requests: u64,
    /// Sync latency (ns, from `WriteAt` to a completed `Sync`).
    pub sync_latency: Histogram,
}

impl StorageMetrics {
    /// Average batch size in requests (0 if there are no batches).
    pub fn mean_batch_size(&self) -> f64 {
        if self.batches == 0 { 0.0 } else { self.batched_requests as f64 / self.batches as f64 }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn derived_values() {
        let mut m = StorageMetrics::default();
        assert_eq!(m.mean_batch_size(), 0.0);
        m.batches = 2;
        m.batched_requests = 50;
        m.sync_latency.record(1000);
        assert_eq!(m.mean_batch_size(), 25.0);
        assert_eq!(m.sync_latency.count(), 1);
    }
}
