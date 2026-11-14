//! Interface with the disk: operations the engine requests and results it receives. The consumer's
//! disk executor or a test disk executes them.

use std::path::{Path, PathBuf};

use teseladb_types::Bytes;

/// Identifier of an open file, assigned by the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(pub u32);

/// Error code of a disk operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoErrno {
    /// No space (`ENOSPC`).
    NoSpace,
    /// The file or directory does not exist (`ENOENT`).
    NotFound,
    /// It already exists (`EEXIST`).
    AlreadyExists,
    /// Unknown `FileId` or file not open.
    BadFile,
    /// Device I/O error (`EIO`).
    Io,
    /// The directory still has entries (`ENOTEMPTY`).
    NotEmpty,
    /// Another system code.
    Other(i32),
}

/// Disk operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IoOp {
    /// Creates an empty file (error if it exists) and reserves `prealloc` bytes that read as zeros.
    Create {
        /// Identifier the engine will use for it.
        file: FileId,
        /// Path.
        path: PathBuf,
        /// Bytes to reserve (0 = none).
        prealloc: u64,
    },
    /// Opens an existing file for reading and writing.
    Open {
        /// Identifier the engine will use for it.
        file: FileId,
        /// Path.
        path: PathBuf,
    },
    /// Writes `data` at `offset` (without syncing).
    WriteAt {
        /// File.
        file: FileId,
        /// Offset.
        offset: u64,
        /// Data.
        data: Bytes,
        /// Batch it belongs to (for tracing).
        batch: u64,
    },
    /// Reads `len` bytes at `offset`; the result arrives as [`IoDone::Read`] with `token`.
    ReadAt {
        /// File.
        file: FileId,
        /// Offset.
        offset: u64,
        /// Bytes a leer.
        len: u32,
        /// Token returned with the result.
        token: u64,
    },
    /// Syncs the file's data (`fdatasync`).
    Sync {
        /// File.
        file: FileId,
        /// Batch that becomes durable.
        batch: u64,
    },
    /// Syncs a directory (so that creations, renames and deletions are durable).
    SyncDir {
        /// Directory.
        path: PathBuf,
    },
    /// Changes the length of the file.
    Truncate {
        /// File.
        file: FileId,
        /// New length.
        len: u64,
    },
    /// Renames atomically (replacing the target if it exists).
    Rename {
        /// Source.
        from: PathBuf,
        /// Destination.
        to: PathBuf,
    },
    /// Gives the file at `from` a second name `to` (a hard link), which must not exist; the
    /// new name is durable after a `SyncDir` of its directory.
    Link {
        /// Existing name.
        from: PathBuf,
        /// New name.
        to: PathBuf,
    },
    /// Deletes a file.
    Remove {
        /// Path.
        path: PathBuf,
    },
    /// Deletes an empty directory (`rmdir`); durable after a `SyncDir` of its parent.
    RemoveDir {
        /// Path.
        path: PathBuf,
    },
    /// Closes an open file.
    Close {
        /// File.
        file: FileId,
    },
}

impl IoOp {
    /// Short name of the operation (for errors and metrics).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            IoOp::Create { .. } => "create",
            IoOp::Open { .. } => "open",
            IoOp::WriteAt { .. } => "write",
            IoOp::ReadAt { .. } => "read",
            IoOp::Sync { .. } => "sync",
            IoOp::SyncDir { .. } => "sync_dir",
            IoOp::Truncate { .. } => "truncate",
            IoOp::Rename { .. } => "rename",
            IoOp::Link { .. } => "link",
            IoOp::Remove { .. } => "remove",
            IoOp::RemoveDir { .. } => "remove_dir",
            IoOp::Close { .. } => "close",
        }
    }

    /// The same operation without the data payload (what [`IoDone::Ok`] returns).
    #[must_use]
    pub fn without_data(&self) -> IoOp {
        match self {
            IoOp::WriteAt { file, offset, batch, .. } => {
                IoOp::WriteAt { file: *file, offset: *offset, data: Bytes::new(), batch: *batch }
            }
            other => other.clone(),
        }
    }
}

/// Result of an operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IoDone {
    /// Successful; returns the operation without data.
    Ok(IoOp),
    /// Result of a read.
    Read {
        /// Token of the request.
        token: u64,
        /// Data read or error.
        data: Result<Bytes, IoErrno>,
    },
    /// Failed.
    Err {
        /// Operation (without data).
        op: IoOp,
        /// Code.
        errno: IoErrno,
    },
}

/// Synchronous I/O used by the executors and by recovery.
pub trait Disk {
    /// Files of a directory (full paths, sorted). `NotFound` error if it does not exist.
    ///
    /// # Errors
    ///
    /// The [`IoErrno`] of the system; `NotFound` when the directory does not exist.
    fn list(&mut self, dir: &Path) -> Result<Vec<PathBuf>, IoErrno>;
    /// Whole contents of a file.
    ///
    /// # Errors
    ///
    /// The [`IoErrno`] of the system.
    fn read_all(&mut self, path: &Path) -> Result<Vec<u8>, IoErrno>;
    /// Executes an operation.
    fn execute(&mut self, op: IoOp) -> IoDone;
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn names_and_without_data() {
        let ops = [
            IoOp::Create { file: FileId(1), path: "a".into(), prealloc: 0 },
            IoOp::Open { file: FileId(1), path: "a".into() },
            IoOp::WriteAt { file: FileId(1), offset: 3, data: Bytes::from("x"), batch: 7 },
            IoOp::ReadAt { file: FileId(1), offset: 0, len: 1, token: 2 },
            IoOp::Sync { file: FileId(1), batch: 7 },
            IoOp::SyncDir { path: "d".into() },
            IoOp::Truncate { file: FileId(1), len: 0 },
            IoOp::Rename { from: "a".into(), to: "b".into() },
            IoOp::Link { from: "a".into(), to: "c".into() },
            IoOp::Remove { path: "a".into() },
            IoOp::RemoveDir { path: "d".into() },
            IoOp::Close { file: FileId(1) },
        ];
        let mut names: Vec<_> = ops.iter().map(IoOp::name).collect();
        names.dedup();
        assert_eq!(names.len(), ops.len());
        assert_eq!(
            ops[2].without_data(),
            IoOp::WriteAt { file: FileId(1), offset: 3, data: Bytes::new(), batch: 7 }
        );
        assert_eq!(ops[4].without_data(), ops[4]);
    }
}
