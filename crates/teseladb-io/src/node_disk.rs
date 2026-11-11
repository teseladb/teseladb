//! The disk executor of a node's cores (012 FR-007, FR-003): the disk thread over [`FileDisk`]
//! or, on Linux with a kernel that has what it needs, [`crate::IoUringDisk`] in the core's own
//! loop; and whether whole-block accesses use `O_DIRECT`.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use teseladb_raftlog::{Disk, IoDone, IoErrno, IoOp};

use crate::FileDisk;
use crate::file_disk::direct_io_supported;

/// Whether `auto` picks io_uring when the kernel has it. FR-011 decides it with the comparison of
/// both executors (p99 and CPU per operation): until io_uring wins, the disk thread stays.
pub const URING_BY_DEFAULT: bool = false;

/// `--io-executor`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IoExecutor {
    /// io_uring if the kernel has it and [`URING_BY_DEFAULT`], otherwise the disk thread.
    #[default]
    Auto,
    /// io_uring; the node does not start without it.
    Uring,
    /// A disk thread per core.
    Threads,
}

/// `--direct-io`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DirectIo {
    /// `O_DIRECT` where the file system takes it (Linux).
    #[default]
    Auto,
    /// `O_DIRECT`; the node does not start without it.
    On,
    /// Buffered I/O.
    Off,
}

impl IoExecutor {
    /// The value of `--io-executor`.
    pub fn parse(s: &str) -> Option<IoExecutor> {
        match s {
            "auto" => Some(IoExecutor::Auto),
            "uring" => Some(IoExecutor::Uring),
            "threads" => Some(IoExecutor::Threads),
            _ => None,
        }
    }
}

impl DirectIo {
    /// The value of `--direct-io`.
    pub fn parse(s: &str) -> Option<DirectIo> {
        match s {
            "auto" => Some(DirectIo::Auto),
            "on" => Some(DirectIo::On),
            "off" => Some(DirectIo::Off),
            _ => None,
        }
    }
}

/// What the node uses, decided once at start, and why (for the log).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskChoice {
    /// Whether the cores use io_uring.
    pub uring: bool,
    /// Whether whole-block accesses use `O_DIRECT`.
    pub direct: bool,
    /// The lines the node logs about it.
    pub notes: Vec<String>,
}

impl fmt::Display for DiskChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let exec = if self.uring { "io_uring" } else { "disk threads" };
        let direct = if self.direct { "O_DIRECT" } else { "buffered I/O with fdatasync" };
        write!(f, "disk: {exec}, {direct}")?;
        for n in &self.notes {
            write!(f, "; {n}")?;
        }
        Ok(())
    }
}

/// Decides the executor and `O_DIRECT` for a data directory: `uring_default` whether `auto`
/// prefers io_uring ([`URING_BY_DEFAULT`]), `uring_support` what the kernel lacks for io_uring
/// (`Err`), `direct_support` whether the directory's file system takes `O_DIRECT`. Asking for
/// what is not there is an error.
pub fn decide(
    uring_default: bool,
    exec: IoExecutor,
    direct: DirectIo,
    uring_support: Result<(), String>,
    direct_support: io::Result<bool>,
) -> Result<DiskChoice, String> {
    let mut notes = Vec::new();
    let uring = match (exec, uring_support) {
        (IoExecutor::Threads, _) => false,
        (IoExecutor::Uring, Ok(())) => true,
        (IoExecutor::Uring, Err(why)) => return Err(format!("--io-executor uring: {why}")),
        (IoExecutor::Auto, Ok(())) if uring_default => true,
        (IoExecutor::Auto, Ok(())) => {
            notes.push("io_uring available; the disk threads stay the default (FR-011)".into());
            false
        }
        (IoExecutor::Auto, Err(why)) => {
            notes.push(format!("io_uring not available ({why})"));
            false
        }
    };
    let supported = direct_support.map_err(|e| format!("cannot check O_DIRECT: {e}"));
    let direct = match (direct, supported) {
        (DirectIo::Off, _) => false,
        (DirectIo::On, Ok(true)) | (DirectIo::Auto, Ok(true)) => true,
        (DirectIo::On, Ok(false)) => {
            return Err("--direct-io on: the file system of the data directory does not take \
                        O_DIRECT with 4 KiB blocks"
                .into());
        }
        (DirectIo::On, Err(e)) => return Err(e),
        (DirectIo::Auto, Ok(false)) => {
            notes.push("the file system does not take O_DIRECT".into());
            false
        }
        (DirectIo::Auto, Err(e)) => {
            notes.push(e);
            false
        }
    };
    Ok(DiskChoice { uring, direct, notes })
}

/// [`decide`] with what this host has, for data directory `dir`.
pub fn choose(exec: IoExecutor, direct: DirectIo, dir: &Path) -> Result<DiskChoice, String> {
    #[cfg(target_os = "linux")]
    let support = crate::io_uring_disk::check_support();
    #[cfg(not(target_os = "linux"))]
    let support = Err("io_uring is Linux only".to_string());
    decide(URING_BY_DEFAULT, exec, direct, support, direct_io_supported(dir))
}

/// A core's disk.
#[derive(Debug)]
pub enum NodeDisk {
    /// Executed by the core's disk thread.
    Threads(FileDisk),
    /// Executed in the core's loop.
    #[cfg(target_os = "linux")]
    Uring(Box<crate::IoUringDisk>),
}

impl NodeDisk {
    /// A disk rooted at `root` as `choice` says.
    pub fn open(root: impl Into<PathBuf>, choice: &DiskChoice) -> io::Result<NodeDisk> {
        let root = root.into();
        #[cfg(target_os = "linux")]
        if choice.uring {
            return crate::IoUringDisk::open(root, choice.direct)
                .map(|u| NodeDisk::Uring(Box::new(u)));
        }
        let mut d = FileDisk::open(root)?;
        d.set_direct_io(choice.direct);
        Ok(NodeDisk::Threads(d))
    }

    /// Its io_uring executor, when it has one.
    #[cfg(target_os = "linux")]
    pub fn uring(&mut self) -> Option<&mut crate::IoUringDisk> {
        match self {
            NodeDisk::Uring(u) => Some(u.as_mut()),
            NodeDisk::Threads(_) => None,
        }
    }

    fn disk(&mut self) -> &mut dyn Disk {
        match self {
            NodeDisk::Threads(d) => d,
            #[cfg(target_os = "linux")]
            NodeDisk::Uring(u) => u.as_mut(),
        }
    }
}

impl NodeDisk {
    /// The sync of a store's batch as a job for another thread, on the disk thread's `FileDisk`
    /// ([`FileDisk::detach_sync`]); io_uring runs its syncs in the core's loop.
    pub(crate) fn detach_sync(&self, op: &IoOp) -> Option<crate::file_disk::SyncJob> {
        match self {
            NodeDisk::Threads(d) => d.detach_sync(op),
            #[cfg(target_os = "linux")]
            NodeDisk::Uring(_) => None,
        }
    }
}

impl Disk for NodeDisk {
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
    fn options_parse() {
        assert_eq!(IoExecutor::parse("auto"), Some(IoExecutor::Auto));
        assert_eq!(IoExecutor::parse("uring"), Some(IoExecutor::Uring));
        assert_eq!(IoExecutor::parse("threads"), Some(IoExecutor::Threads));
        assert_eq!(IoExecutor::parse("x"), None);
        assert_eq!(DirectIo::parse("auto"), Some(DirectIo::Auto));
        assert_eq!(DirectIo::parse("on"), Some(DirectIo::On));
        assert_eq!(DirectIo::parse("off"), Some(DirectIo::Off));
        assert_eq!(DirectIo::parse("1"), None);
        assert_eq!(IoExecutor::default(), IoExecutor::Auto);
        assert_eq!(DirectIo::default(), DirectIo::Auto);
    }

    #[test]
    fn the_choice_follows_the_options_and_what_the_host_has() {
        let no = || Err::<(), String>("missing IORING_OP_RENAMEAT".into());
        let d = |e, d, u, s| decide(false, e, d, u, s);
        let c = d(IoExecutor::Auto, DirectIo::Auto, no(), Ok(true)).unwrap();
        assert!(!c.uring && c.direct);
        assert!(c.to_string().contains("io_uring not available (missing IORING_OP_RENAMEAT)"));
        let c = d(IoExecutor::Auto, DirectIo::Auto, Ok(()), Ok(false)).unwrap();
        assert!(!c.uring);
        assert!(!c.direct);
        let preferred = decide(true, IoExecutor::Auto, DirectIo::Off, Ok(()), Ok(true)).unwrap();
        assert!(preferred.uring && preferred.notes.is_empty());
        assert!(c.to_string().contains("does not take O_DIRECT"));
        let c = d(IoExecutor::Uring, DirectIo::On, Ok(()), Ok(true)).unwrap();
        assert!(c.uring && c.direct);
        assert_eq!(c.to_string(), "disk: io_uring, O_DIRECT");
        let c = d(IoExecutor::Threads, DirectIo::Off, Ok(()), Ok(true)).unwrap();
        assert_eq!(c.to_string(), "disk: disk threads, buffered I/O with fdatasync");
        assert!(d(IoExecutor::Uring, DirectIo::Off, no(), Ok(true)).unwrap_err().contains("uring"));
        let e = d(IoExecutor::Threads, DirectIo::On, Ok(()), Ok(false)).unwrap_err();
        assert!(e.contains("4 KiB"));
        let broken = || Err(io::Error::other("denied"));
        assert!(d(IoExecutor::Threads, DirectIo::On, Ok(()), broken()).is_err());
        let c = d(IoExecutor::Threads, DirectIo::Auto, Ok(()), broken()).unwrap();
        assert!(!c.direct && c.to_string().contains("denied"));
    }

    #[test]
    fn a_threads_disk_opens_and_executes() {
        let root = std::env::temp_dir().join(format!("teseladb-nd-{}", std::process::id()));
        let choice = choose(IoExecutor::Threads, DirectIo::Off, &std::env::temp_dir()).unwrap();
        let mut d = NodeDisk::open(&root, &choice).unwrap();
        let op = IoOp::SyncDir { path: ".".into() };
        assert!(matches!(d.execute(op), IoDone::Ok(_)));
        assert_eq!(d.list(Path::new(".")).unwrap(), Vec::<PathBuf>::new());
        assert_eq!(d.read_all(Path::new("x")), Err(IoErrno::NotFound));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
