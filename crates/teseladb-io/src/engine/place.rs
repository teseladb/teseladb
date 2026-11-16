//! Where a node keeps its disks (023 FR-010): the files under its data directory, disks in memory
//! (`teseladb dev`, the tests and the tools), or disks a test makes (one that fails on purpose).
//! Every disk of the files sees the same files; every disk in memory starts empty and is its own.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use teseladb_raftlog::{Disk, IoDone, IoErrno, IoOp, MemDisk};

#[cfg(target_os = "linux")]
use super::disk::{IoGroups, ShardDisk};
#[cfg(not(target_os = "linux"))]
use super::unsupported::disk::ShardDisk;
use crate::device_log::LogDisk;
use crate::files::BlockingDisk;

/// Makes a disk of a [`DiskPlace::Made`] place.
pub type DiskMaker = Arc<dyn Fn() -> io::Result<Box<dyn Disk + Send>> + Send + Sync>;

/// Where a node keeps its disks.
#[derive(Clone)]
pub enum DiskPlace {
    /// The files under this directory.
    Files(PathBuf),
    /// Disks in memory.
    Memory,
    /// Disks this function makes, each run as a disk in memory is.
    Made(DiskMaker),
}

impl fmt::Debug for DiskPlace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiskPlace::Files(p) => f.debug_tuple("Files").field(p).finish(),
            DiskPlace::Memory => f.write_str("Memory"),
            DiskPlace::Made(_) => f.write_str("Made"),
        }
    }
}

impl DiskPlace {
    /// A model of a disk, for the places that are not files.
    fn model(&self) -> io::Result<Box<dyn Disk + Send>> {
        match self {
            DiskPlace::Made(make) => make(),
            _ => Ok(Box::new(MemDisk::new())),
        }
    }

    /// The executor of a disk on the calling shard. It waits for the engine with `get()`, so it
    /// runs in an engine thread (as a core is built in `NodeEngine::start_core`).
    ///
    /// # Errors
    ///
    /// When the directory cannot be made, or the disk made.
    #[cfg(target_os = "linux")]
    pub fn on_shard(&self) -> io::Result<ShardDisk> {
        match self {
            DiskPlace::Files(root) => {
                let failed = |e: teseladb::engine::core::future::ExceptionPtr| {
                    io::Error::other(e.to_string())
                };
                let groups = IoGroups::get_or_create().get().map_err(failed)?;
                ShardDisk::open(root.clone(), groups).get().map_err(failed)
            }
            other => Ok(ShardDisk::model(other.model()?)),
        }
    }

    /// Fails: the engine runs on Linux only (D3).
    ///
    /// # Errors
    ///
    /// Always.
    #[cfg(not(target_os = "linux"))]
    pub fn on_shard(&self) -> io::Result<ShardDisk> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "the server runs on Linux only"))
    }

    /// A disk for the node's start, before its engine runs (its identity and data format size the
    /// engine): blocking calls on the files ([`BlockingDisk`]).
    ///
    /// # Errors
    ///
    /// When the directory cannot be made, or the disk made.
    pub fn startup_disk(&self) -> io::Result<StartupDisk> {
        match self {
            DiskPlace::Files(root) => Ok(StartupDisk::Files(BlockingDisk::open(root.clone())?)),
            other => Ok(StartupDisk::Model(other.model()?)),
        }
    }

    /// The disk of the node's device log.
    ///
    /// # Errors
    ///
    /// When the disk cannot be made.
    pub fn log_disk(&self) -> io::Result<LogDisk> {
        match self {
            DiskPlace::Files(root) => Ok(LogDisk::Files(root.clone())),
            other => Ok(LogDisk::Memory(other.model()?)),
        }
    }
}

/// A disk for the node's start ([`DiskPlace::startup_disk`]).
pub enum StartupDisk {
    /// Blocking calls on the files.
    Files(BlockingDisk),
    /// A disk in memory, or another model of one.
    Model(Box<dyn Disk + Send>),
}

impl StartupDisk {
    fn disk(&mut self) -> &mut dyn Disk {
        match self {
            StartupDisk::Files(d) => d,
            StartupDisk::Model(d) => d.as_mut(),
        }
    }
}

impl Disk for StartupDisk {
    fn list(&mut self, dir: &Path) -> Result<Vec<PathBuf>, IoErrno> {
        self.disk().list(dir)
    }

    fn read_all(&mut self, path: &Path) -> Result<Vec<u8>, IoErrno> {
        self.disk().read_all(path)
    }

    fn execute(&mut self, op: IoOp) -> IoDone {
        self.disk().execute(op)
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_place_says_its_kind_and_gives_its_disks_for_the_start() {
        let made: DiskMaker = Arc::new(|| Ok(Box::new(MemDisk::with_capacity(1)) as Box<_>));
        assert_eq!(format!("{:?}", DiskPlace::Made(made.clone())), "Made");
        assert_eq!(format!("{:?}", DiskPlace::Memory), "Memory");
        assert!(format!("{:?}", DiskPlace::Files("/d".into())).contains("/d"));
        // A made disk is the one the function makes: this one has no room.
        let mut d = DiskPlace::Made(made).startup_disk().unwrap();
        let op = IoOp::Create { file: teseladb_raftlog::FileId(1), path: "a".into(), prealloc: 2 };
        assert!(matches!(d.execute(op), IoDone::Err { errno: IoErrno::NoSpace, .. }));
        let mut m = DiskPlace::Memory.startup_disk().unwrap();
        assert_eq!(m.read_all(Path::new("a")), Err(IoErrno::NotFound));
        assert_eq!(m.list(Path::new("")), Err(IoErrno::NotFound));
        let root = std::env::temp_dir().join(format!("teseladb-place-{}", std::process::id()));
        let mut f = DiskPlace::Files(root.clone()).startup_disk().unwrap();
        assert_eq!(f.list(Path::new("")).unwrap(), Vec::<PathBuf>::new());
        assert!(matches!(DiskPlace::Files(root.clone()).log_disk().unwrap(), LogDisk::Files(_)));
        assert!(matches!(DiskPlace::Memory.log_disk().unwrap(), LogDisk::Memory(_)));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
