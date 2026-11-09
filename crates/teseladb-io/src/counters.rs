//! Counters of the node's real I/O, for its metrics.

use std::sync::atomic::{AtomicU64, Ordering};

static SYNCS: AtomicU64 = AtomicU64::new(0);
static DEVICE_FLUSHES: AtomicU64 = AtomicU64::new(0);
static DEVICE_FLUSH_MICROS: AtomicU64 = AtomicU64::new(0);

/// Counts a sync of a file or a folder to the disk.
pub(crate) fn record_sync() {
    SYNCS.fetch_add(1, Ordering::Relaxed);
}

/// The syncs of files and folders to the disk since the process started.
pub fn syncs() -> u64 {
    SYNCS.load(Ordering::Relaxed)
}

/// Counts a flush of a drive's cache to permanent storage.
pub(crate) fn record_device_flush() {
    DEVICE_FLUSHES.fetch_add(1, Ordering::Relaxed);
}

/// The flushes of a drive's cache since the process started: on macOS one serves every sync that
/// waited for it (H-015 of the native tests).
pub fn device_flushes() -> u64 {
    DEVICE_FLUSHES.load(Ordering::Relaxed)
}

/// Counts the time a flush of a drive's cache took.
pub(crate) fn record_device_flush_micros(micros: u64) {
    DEVICE_FLUSH_MICROS.fetch_add(micros, Ordering::Relaxed);
}

/// The microseconds the flushes of the drives' caches took since the process started: what is
/// waiting in a drive's cache makes them longer (H-008 of the native tests).
pub fn device_flush_micros() -> u64 {
    DEVICE_FLUSH_MICROS.load(Ordering::Relaxed)
}
