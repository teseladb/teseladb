//! Maintenance of a core's Raft log on a thread of its own (H-049 of the native tests): deleting
//! an old WAL segment or snapshot and syncing its directory. The log asks for it apart from its
//! batches and no batch waits for it, but a directory sync takes as long as the drive's flush (18
//! to 28 ms measured on macOS), and run on the core's disk thread it held every write of the log
//! queued behind it. Its answer may then come after the answers of later operations, so whoever
//! matches answers to operations matches these by operation ([`answered_apart`]).

use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};

use teseladb_raftlog::{IoDone, IoOp};

use crate::file_disk::io_errno;

/// The directories of a core's log: its WAL segments and its snapshots.
const LOG_DIRS: [&str; 2] = ["wal", "snap"];

/// Whether `path` is `core-NN/<dir>` of a log directory with `files` more components after it.
fn in_log_dir(path: &Path, files: usize) -> bool {
    let parts: Vec<&str> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    parts.len() == 2 + files
        && parts.len() == path.components().count()
        && parts[0].starts_with("core-")
        && LOG_DIRS.contains(&parts[1])
}

/// Whether `op` is maintenance of a core's Raft log: deleting a file of its WAL or snapshot
/// directory, or syncing one of those directories.
pub fn log_chore(op: &IoOp) -> bool {
    match op {
        IoOp::Remove { path } => in_log_dir(path, 1),
        IoOp::SyncDir { path } => in_log_dir(path, 0),
        _ => false,
    }
}

/// Whether the answer to `op` may come after the answers to operations handed out after it: the
/// sync of a log batch (`batch > 0`, 012 FR-014) and the maintenance of a log.
pub fn answered_apart(op: &IoOp) -> bool {
    matches!(op, IoOp::Sync { batch, .. } if *batch > 0) || log_chore(op)
}

/// A maintenance operation of a log handed to the core's chores thread: the operation and its
/// path under the node's directory.
#[derive(Debug)]
pub(crate) struct DetachedChore {
    path: PathBuf,
    op: IoOp,
}

impl DetachedChore {
    /// The chore `op` of a disk rooted at `root`, if it is one.
    pub(crate) fn of(root: &Path, op: &IoOp) -> Option<DetachedChore> {
        let path = match op {
            IoOp::Remove { path } | IoOp::SyncDir { path } if log_chore(op) => root.join(path),
            _ => return None,
        };
        Some(DetachedChore { path, op: op.clone() })
    }

    /// Runs it and gives its answer.
    pub(crate) fn run(self) -> IoDone {
        let r = match &self.op {
            IoOp::SyncDir { .. } => {
                crate::counters::record_sync();
                File::open(&self.path).and_then(|d| crate::drive_flush::make_durable(&d))
            }
            _ => fs::remove_file(&self.path),
        };
        match r {
            Ok(()) => IoDone::Ok(self.op),
            Err(e) => IoDone::Err { op: self.op, errno: io_errno(e) },
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_raftlog::{FileId, IoErrno};

    #[test]
    fn only_the_files_and_directories_of_a_log_are_its_chores() {
        let remove = |p: &str| IoOp::Remove { path: p.into() };
        let sync_dir = |p: &str| IoOp::SyncDir { path: p.into() };
        assert!(log_chore(&remove("core-02/wal/0000000000000001.wal")));
        assert!(log_chore(&remove("core-02/snap/g1-i2.snap")));
        assert!(log_chore(&sync_dir("core-02/wal")));
        assert!(log_chore(&sync_dir("core-17/snap")));
        // The LSM's files, the core's own directory and anything else are not.
        assert!(!log_chore(&remove("core-02/lsm/000001.sst")));
        assert!(!log_chore(&sync_dir("core-02/lsm")));
        assert!(!log_chore(&sync_dir("core-02")));
        assert!(!log_chore(&remove("core-02/REBUILT")));
        assert!(!log_chore(&remove("/abs/core-02/wal/x.wal")));
        assert!(!log_chore(&sync_dir("core-02/wal/../lsm")));
        assert!(!log_chore(&IoOp::Close { file: FileId(1) }));
        // A batch's sync is answered apart too; a sync outside a batch is not.
        assert!(answered_apart(&IoOp::Sync { file: FileId(1), batch: 3 }));
        assert!(!answered_apart(&IoOp::Sync { file: FileId(1), batch: 0 }));
        assert!(answered_apart(&sync_dir("core-02/wal")));
    }

    #[test]
    fn a_chore_deletes_its_file_and_syncs_its_directory_under_the_root() {
        let root = std::env::temp_dir().join(format!("teseladb-chore-{}", std::process::id()));
        let wal = root.join("core-03/wal");
        fs::create_dir_all(&wal).unwrap();
        fs::write(wal.join("a.wal"), b"x").unwrap();
        let remove = IoOp::Remove { path: "core-03/wal/a.wal".into() };
        let chore = DetachedChore::of(&root, &remove).expect("a chore");
        assert_eq!(chore.run(), IoDone::Ok(remove.clone()));
        assert!(!wal.join("a.wal").exists());
        let sync = IoOp::SyncDir { path: "core-03/wal".into() };
        assert_eq!(DetachedChore::of(&root, &sync).unwrap().run(), IoDone::Ok(sync));
        // A file already gone is the error of the operation.
        let again = DetachedChore::of(&root, &remove).unwrap().run();
        assert_eq!(again, IoDone::Err { op: remove, errno: IoErrno::NotFound });
        assert!(DetachedChore::of(&root, &IoOp::SyncDir { path: "core-03/lsm".into() }).is_none());
        let _ = fs::remove_dir_all(&root);
    }
}
