//! Errors of the LSM (`lsm-api.md`, «Errores»; FR-014, FR-015).

use std::fmt;

use teseladb_raftlog::IoErrno;

/// An LSM error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LsmError {
    /// Level 0 is past its rejection limit (`pressure() = Reject`).
    Overloaded,
    /// The disk is full: nothing new is written until a retry succeeds.
    ReadOnly,
    /// A checksum or a structure does not match: the core is corrupt and must be rebuilt.
    Corrupt {
        /// File number (0 for the manifest or `CURRENT`).
        file: u64,
        /// Offset of the bad bytes.
        offset: u64,
    },
    /// The ingested range already has local data.
    IngestOverlap,
    /// An I/O error other than a full disk stopped the core.
    Stopped(IoErrno),
}

impl fmt::Display for LsmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LsmError::Overloaded => write!(f, "level 0 is full: proposals are rejected"),
            LsmError::ReadOnly => write!(f, "the disk is full: the core is read-only"),
            LsmError::Corrupt { file, offset } => {
                write!(f, "corrupt data in file {file} at offset {offset}")
            }
            LsmError::IngestOverlap => write!(f, "the ingested range has local data"),
            LsmError::Stopped(e) => write!(f, "I/O error, the core stopped: {e:?}"),
        }
    }
}

impl std::error::Error for LsmError {}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn every_error_has_a_message() {
        let all = [
            LsmError::Overloaded,
            LsmError::ReadOnly,
            LsmError::Corrupt { file: 3, offset: 4096 },
            LsmError::IngestOverlap,
            LsmError::Stopped(IoErrno::Io),
        ];
        let texts: Vec<String> = all.iter().map(ToString::to_string).collect();
        assert!(texts.iter().all(|t| !t.is_empty()));
        assert!(texts[2].contains("4096"));
    }
}
