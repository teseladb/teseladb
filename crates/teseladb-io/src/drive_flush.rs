//! Making a file durable. On macOS `fsync(2)` leaves data in the drive's cache and only
//! `F_FULLFSYNC` makes it durable, by writing the drive's whole cache to permanent storage; on
//! Linux `fsync` asks the drive to flush as part of the call. The Raft logs of every core go
//! through one writer per device that syncs once for all of them (012 FR-035, D-154), so the
//! syncs that reach here are the LSM's, the snapshots' and the directories', each made on its own.

use std::io;

/// Makes `file` durable: its data goes to the drive and the drive flushes its cache.
pub(crate) fn make_durable(file: &std::fs::File) -> io::Result<()> {
    let start = std::time::Instant::now();
    let out = make_durable_now(file);
    let nanos = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    teseladb_stages::record(teseladb_stages::Stage::DriveFlushWait, nanos);
    out
}

/// [`make_durable`] without timing it.
fn make_durable_now(file: &std::fs::File) -> io::Result<()> {
    crate::counters::record_device_flush();
    #[cfg(target_os = "macos")]
    {
        use teseladb_sys::drive;
        drive::push_to_drive(file)?;
        timed(|| drive::flush_drive(file))
    }
    #[cfg(not(target_os = "macos"))]
    {
        timed(|| file.sync_all())
    }
}

/// Runs a flush of a drive's cache and counts how long it took.
fn timed<T>(flush: impl FnOnce() -> T) -> T {
    let start = std::time::Instant::now();
    let out = flush();
    let micros = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
    crate::counters::record_device_flush_micros(micros);
    out
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_made_durable() {
        let dir = std::env::temp_dir().join(format!("teseladb-io-durable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f");
        std::fs::write(&path, b"x").unwrap();
        let before = crate::counters::device_flushes();
        make_durable(&std::fs::File::open(&path).unwrap()).unwrap();
        assert!(crate::counters::device_flushes() > before);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
