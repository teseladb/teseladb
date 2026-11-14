//! In-memory disk without crashes: everything is durable immediately. Used by `none` mode and by
//! the tests that do not study durability.

use std::path::{Path, PathBuf};

use crate::disk::{Disk, IoDone, IoErrno, IoOp};
use crate::sim_disk::{SimDisk, SimDiskConfig};

/// In-memory disk. Like [`SimDisk`] but without torn writes or crashes.
#[derive(Debug)]
pub struct MemDisk {
    inner: SimDisk,
}

impl Default for MemDisk {
    fn default() -> Self {
        MemDisk::new()
    }
}

impl MemDisk {
    /// Empty disk with unlimited capacity.
    #[must_use]
    pub fn new() -> MemDisk {
        MemDisk { inner: SimDisk::new(SimDiskConfig::default()) }
    }

    /// Empty disk with limited capacity (to test `ENOSPC`).
    #[must_use]
    pub fn with_capacity(capacity: u64) -> MemDisk {
        MemDisk { inner: SimDisk::new(SimDiskConfig { capacity, ..Default::default() }) }
    }

    /// Bytes used.
    #[must_use]
    pub fn used(&self) -> u64 {
        self.inner.used()
    }

    /// Makes the next operation named `op` fail with `errno`.
    pub fn inject_error(&mut self, op: &'static str, errno: IoErrno) {
        self.inner.inject_error(op, errno);
    }
}

impl Disk for MemDisk {
    fn list(&mut self, dir: &Path) -> Result<Vec<PathBuf>, IoErrno> {
        self.inner.list(dir)
    }

    fn read_all(&mut self, path: &Path) -> Result<Vec<u8>, IoErrno> {
        self.inner.read_all(path)
    }

    fn execute(&mut self, op: IoOp) -> IoDone {
        self.inner.execute(op)
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::disk::FileId;
    use teseladb_types::Bytes;

    #[test]
    fn basic_operations() {
        let mut d = MemDisk::default();
        d.execute(IoOp::Create { file: FileId(1), path: "a".into(), prealloc: 0 });
        d.execute(IoOp::WriteAt { file: FileId(1), offset: 0, data: Bytes::from("xy"), batch: 0 });
        assert_eq!(d.read_all(Path::new("a")).expect("read_all succeeds"), b"xy");
        assert_eq!(d.list(Path::new("")).expect("list succeeds"), vec![PathBuf::from("a")]);
        assert_eq!(d.used(), 2);
        d.inject_error("close", IoErrno::Io);
        assert!(matches!(d.execute(IoOp::Close { file: FileId(1) }), IoDone::Err { .. }));
        // A directory goes once it is empty.
        d.execute(IoOp::Create { file: FileId(2), path: "d/b".into(), prealloc: 0 });
        d.execute(IoOp::Close { file: FileId(2) });
        assert_eq!(
            d.execute(IoOp::RemoveDir { path: "d".into() }),
            IoDone::Err { op: IoOp::RemoveDir { path: "d".into() }, errno: IoErrno::NotEmpty }
        );
        d.execute(IoOp::Remove { path: "d/b".into() });
        assert_eq!(
            d.execute(IoOp::RemoveDir { path: "d".into() }),
            IoDone::Ok(IoOp::RemoveDir { path: "d".into() })
        );
        assert_eq!(d.list(Path::new("d")), Err(IoErrno::NotFound));
        let mut small = MemDisk::with_capacity(1);
        assert!(matches!(
            small.execute(IoOp::Create { file: FileId(1), path: "a".into(), prealloc: 2 }),
            IoDone::Err { errno: IoErrno::NoSpace, .. }
        ));
    }
}
