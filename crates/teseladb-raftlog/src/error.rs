//! Engine errors (data-model §9).

use core::fmt;
use std::path::PathBuf;

use crate::disk::IoErrno;

/// Error of `teseladb-raftlog`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    /// Wrong checksum or `magic` in data that should be valid.
    Corrupt {
        /// File.
        file: PathBuf,
        /// Offset of the problem.
        offset: u64,
    },
    /// No space: the core is read-only until a retry succeeds.
    DiskFull,
    /// Another system error.
    Io {
        /// Operation that failed (short name).
        op: &'static str,
        /// Code.
        errno: IoErrno,
    },
    /// Unknown format version.
    FormatVersion {
        /// Version found.
        found: u32,
    },
    /// Started with a number of cores different from `node.meta`'s.
    CoreCountMismatch {
        /// Cores in `node.meta`.
        meta: u32,
        /// Cores requested.
        requested: u32,
    },
    /// Invalid request (for instance, entries that do not follow the log or a group that does not
    /// exist).
    Invalid(&'static str),
    /// The core stopped after a write or sync failure (fail-stop): the process has to be
    /// restarted.
    CoreFailed,
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::Corrupt { file, offset } => {
                write!(f, "corrupt data in {} (offset {offset})", file.display())
            }
            StorageError::DiskFull => f.write_str("disk full"),
            StorageError::Io { op, errno } => write!(f, "I/O error in {op}: {errno:?}"),
            StorageError::FormatVersion { found } => {
                write!(f, "unknown format version: {found}")
            }
            StorageError::CoreCountMismatch { meta, requested } => {
                write!(
                    f,
                    "the directory was created with {meta} cores and {requested} were requested"
                )
            }
            StorageError::Invalid(what) => write!(f, "invalid request: {what}"),
            StorageError::CoreFailed => f.write_str("core stopped after an I/O failure"),
        }
    }
}

impl std::error::Error for StorageError {}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn messages() {
        let all = [
            StorageError::Corrupt { file: "a/b".into(), offset: 9 },
            StorageError::DiskFull,
            StorageError::Io { op: "sync", errno: IoErrno::Other(5) },
            StorageError::FormatVersion { found: 9 },
            StorageError::CoreCountMismatch { meta: 2, requested: 4 },
            StorageError::Invalid("x"),
            StorageError::CoreFailed,
        ];
        for e in all {
            assert!(!e.to_string().is_empty());
        }
    }
}
