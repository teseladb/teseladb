//! A core whose files do not recover is rebuilt from the replicas: its files are set aside (renamed with a `.corrupt` suffix, which recovery ignores and an
//! operator can still salvage), the core starts empty and a marker (`core-XX/REBUILT`) remembers,
//! across restarts, that it must not vote with that empty state until it rejoined every group as a
//! new replica.

use std::path::PathBuf;

use crate::disk::{Disk, FileId, IoDone, IoOp};
use crate::error::StorageError;
use crate::paths;

/// File that marks a core being rebuilt.
#[must_use]
pub fn rebuild_marker(core: u16) -> PathBuf {
    paths::core_dir(core).join("REBUILT")
}

fn exec(disk: &mut dyn Disk, op: IoOp) -> Result<(), StorageError> {
    match disk.execute(op) {
        IoDone::Err { op, errno } => Err(StorageError::Io { op: op.name(), errno }),
        _ => Ok(()),
    }
}

/// Sets a corrupt core's files aside and leaves the marker. Returns how many files were set aside.
///
/// # Errors
///
/// [`StorageError::Io`] or [`StorageError::DiskFull`] when a file cannot be moved or the marker written.
pub fn set_aside(disk: &mut dyn Disk, core: u16) -> Result<usize, StorageError> {
    // The marker first: a set-aside cut short (a crash, a full disk) still starts rebuilt, never
    // with part of its files.
    if !rebuilding(disk, core) {
        let file = FileId(u32::MAX - 3);
        exec(disk, IoOp::Create { file, path: rebuild_marker(core), prealloc: 0 })?;
        exec(disk, IoOp::Sync { file, batch: 0 })?;
        exec(disk, IoOp::Close { file })?;
        exec(disk, IoOp::SyncDir { path: paths::core_dir(core) })?;
    }
    let mut moved = 0;
    for dir in [paths::wal_dir(core), paths::snap_dir(core)] {
        // A directory that does not exist has nothing to set aside.
        let files = disk.list(&dir).unwrap_or_default();
        let live: Vec<PathBuf> =
            files.into_iter().filter(|p| p.extension().is_none_or(|e| e != "corrupt")).collect();
        for p in &live {
            let to = PathBuf::from(format!("{}.corrupt", p.display()));
            exec(disk, IoOp::Rename { from: p.clone(), to })?;
        }
        if !live.is_empty() {
            exec(disk, IoOp::SyncDir { path: dir })?;
        }
        moved += live.len();
    }
    Ok(moved)
}

/// Whether the core is being rebuilt (its marker is there).
pub fn rebuilding(disk: &mut dyn Disk, core: u16) -> bool {
    disk.read_all(&rebuild_marker(core)).is_ok()
}
