//! Engine configuration.

use teseladb_types::{Durability, Nanos};

/// Parameters of a core's engine. The defaults are the specification's.
#[derive(Clone, Debug, PartialEq)]
pub struct StorageConfig {
    /// Size of a WAL segment (preallocated).
    pub wal_segment_size: u64,
    /// Maximum bytes of a group-commit batch.
    pub max_batch_bytes: u64,
    /// Maximum wait to group requests into a `strict` batch (0 = no wait).
    pub group_commit_delay: Nanos,
    /// Maximum delay to persist what was written in `memory` mode.
    pub memory_flush_interval: Nanos,
    /// Default durability mode.
    pub default_durability: Durability,
    /// Use `O_DIRECT` (Linux only; the executor applies it).
    pub direct_io: bool,
    /// Node (written in the headers of new files; 0 = unassigned).
    pub node_id: u64,
    /// Encryption at rest of the WAL and the snapshots; `None` writes them in clear.
    pub encryption: Option<teseladb_crypto::at_rest::Encryption>,
    /// Batches of the core in flight at once (at least 1): a batch that only appends to the
    /// current WAL segment is written while the ones before it wait for their syncs, and every
    /// batch becomes durable after those before it.
    pub max_inflight_batches: usize,
}

impl Default for StorageConfig {
    fn default() -> Self {
        StorageConfig {
            wal_segment_size: 64 << 20,
            max_batch_bytes: 4 << 20,
            group_commit_delay: Nanos::ZERO,
            memory_flush_interval: Nanos::from_millis(1),
            default_durability: Durability::Strict,
            direct_io: false,
            node_id: 0,
            encryption: None,
            max_inflight_batches: 1,
        }
    }
}

impl StorageConfig {
    /// Checks that the values are consistent.
    ///
    /// # Errors
    ///
    /// The first inconsistency, as text.
    pub fn validate(&self) -> Result<(), String> {
        if self.wal_segment_size < 1 << 20 || !self.wal_segment_size.is_multiple_of(4096) {
            return Err("wal_segment_size must be ≥ 1 MiB and a multiple of 4 KiB".into());
        }
        if self.max_batch_bytes + 8192 > self.wal_segment_size {
            return Err("max_batch_bytes does not fit in a WAL segment".into());
        }
        if self.max_inflight_batches == 0 {
            return Err("max_inflight_batches must be at least 1".into());
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_checks_fire() {
        assert_eq!(StorageConfig::default().validate(), Ok(()));
        let bad = |f: fn(&mut StorageConfig)| {
            let mut c = StorageConfig::default();
            f(&mut c);
            c.validate().expect_err("validate fails")
        };
        assert!(bad(|c| c.wal_segment_size = 1000).contains("wal_segment_size"));
        assert!(bad(|c| c.wal_segment_size = (1 << 20) + 1).contains("wal_segment_size"));
        assert!(bad(|c| c.max_batch_bytes = 64 << 20).contains("max_batch_bytes"));
        assert!(bad(|c| c.max_inflight_batches = 0).contains("max_inflight_batches"));
    }
}
