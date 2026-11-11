//! Counters of the node's real I/O, for its metrics.

use std::sync::atomic::{AtomicU64, Ordering};

static SYNCS: AtomicU64 = AtomicU64::new(0);
static DEVICE_FLUSHES: AtomicU64 = AtomicU64::new(0);
static DEVICE_FLUSH_MICROS: AtomicU64 = AtomicU64::new(0);
static DEVICE_PUSHES: AtomicU64 = AtomicU64::new(0);
static DEVICE_PUSH_MICROS: AtomicU64 = AtomicU64::new(0);
static WRITE_BEHINDS: AtomicU64 = AtomicU64::new(0);

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

/// Counts the files the leader of a flush group sent to the drive before its flush, and the time
/// it took to send them all.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn record_device_pushes(files: u64, micros: u64) {
    DEVICE_PUSHES.fetch_add(files, Ordering::Relaxed);
    DEVICE_PUSH_MICROS.fetch_add(micros, Ordering::Relaxed);
}

/// The files the leaders of flush groups sent to the drive since the process started.
pub fn device_pushes() -> u64 {
    DEVICE_PUSHES.load(Ordering::Relaxed)
}

/// The microseconds the leaders of flush groups spent sending their files to the drive before
/// each flush: added to the flushes', the length of the groups a confirmed write waits for.
pub fn device_push_micros() -> u64 {
    DEVICE_PUSH_MICROS.load(Ordering::Relaxed)
}

/// Counts a file sent towards the device as it grows (`teseladb_sys::write_behind`).
pub(crate) fn record_write_behind() {
    WRITE_BEHINDS.fetch_add(1, Ordering::Relaxed);
}

/// The times files were sent towards the device as they grew since the process started.
pub fn write_behinds() -> u64 {
    WRITE_BEHINDS.load(Ordering::Relaxed)
}
