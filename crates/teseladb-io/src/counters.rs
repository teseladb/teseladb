//! Counters of the node's real I/O, for its metrics.

use std::sync::atomic::{AtomicU64, Ordering};

static SYNCS: AtomicU64 = AtomicU64::new(0);

/// Counts a sync of a file or a folder to the disk.
pub(crate) fn record_sync() {
    SYNCS.fetch_add(1, Ordering::Relaxed);
}

/// The syncs of files and folders to the disk since the process started.
pub fn syncs() -> u64 {
    SYNCS.load(Ordering::Relaxed)
}
