//! The disk executors the real crash tests run with (012 FR-004): the disk thread's `FileDisk`
//! and, on Linux, io_uring; both with `O_DIRECT` where the file system takes it. The parent passes
//! the executor to the child in `TESELADB_EXECUTOR`.

use std::path::Path;

use teseladb_io::NodeDisk;
use teseladb_io::node_disk::{DirectIo, IoExecutor, choose};

/// Name of the variable that carries the executor to the child.
pub const EXECUTOR_VAR: &str = "TESELADB_EXECUTOR";

/// The executors of this platform, with their names for `EXECUTOR_VAR`.
pub fn executors() -> Vec<(&'static str, IoExecutor)> {
    let mut v = vec![("threads", IoExecutor::Threads)];
    if cfg!(target_os = "linux") {
        v.push(("uring", IoExecutor::Uring));
    }
    v
}

/// The executor named in `EXECUTOR_VAR` (the disk thread's when absent).
pub fn from_env() -> IoExecutor {
    let name = std::env::var(EXECUTOR_VAR).unwrap_or_default();
    IoExecutor::parse(&name).unwrap_or(IoExecutor::Threads)
}

/// A disk at `dir` with executor `exec`.
pub fn open(dir: impl AsRef<Path>, exec: IoExecutor) -> NodeDisk {
    let dir = dir.as_ref();
    std::fs::create_dir_all(dir).unwrap();
    let choice = choose(exec, DirectIo::Auto, false, dir).unwrap();
    NodeDisk::open(dir, &choice).unwrap()
}
