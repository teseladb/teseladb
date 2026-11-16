//! The class of a disk operation of a core (012 FR-023): the log's operations do not wait behind
//! the data's, the clients' reads do not wait behind the LSM's background writes (012 FR-028),
//! and the core matches each result with an operation of its class. On the engine each class is
//! a scheduling group whose shares the engine's I/O queue uses (023 FR-012,
//! `engine::disk`).
//!
//! Two kinds of operation are answered as soon as they end instead of in their class's order
//! ([`answered_apart`]): the sync of a log's batch and the maintenance of a core's log ([`log_chore`]:
//! deleting an old WAL segment or snapshot, syncing their directory). No batch of the log waits
//! for a maintenance operation: a directory sync takes as long as the drive's flush (H-049 of the
//! native tests). Whoever matches answers to operations matches these by operation.

use std::path::{Component, Path};

use teseladb_raftlog::IoOp;

/// Which queue of its core's executor a disk operation goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IoClass {
    /// The Raft log and the rollback files: what a write waits for before it is answered.
    Log,
    /// The LSM's reads for the clients, exports, ingestions and the rest of the core's files.
    Data,
    /// The LSM's background work (compactions and flushes) and the core changes: within the
    /// data's share of the disk, behind the clients' reads (012 FR-028).
    Background,
}

/// How many classes there are: the executor keeps a queue for each.
pub const CLASSES: usize = 3;

impl IoClass {
    /// Its position among the classes.
    pub fn index(self) -> usize {
        match self {
            IoClass::Log => 0,
            IoClass::Data => 1,
            IoClass::Background => 2,
        }
    }
}

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

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_raftlog::FileId;

    #[test]
    fn each_class_has_its_own_queue() {
        let all = [IoClass::Log, IoClass::Data, IoClass::Background];
        assert_eq!(all.map(IoClass::index), [0, 1, 2]);
        assert_eq!(all.len(), CLASSES);
    }

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
}
