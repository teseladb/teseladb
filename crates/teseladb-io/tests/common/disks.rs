//! The disks the real crash tests run with (012 FR-004, 023 FR-010): the engine's disk executor on
//! the files, with `O_DIRECT` where the file system takes it and with the kernel's page cache. The
//! parent passes the choice to the child in `TESELADB_EXECUTOR`. Each process runs one engine
//! (D4), and the stores run in engine threads of its shards, where the executor's synchronous
//! calls wait.
#![allow(dead_code)]

use std::path::Path;
use std::sync::mpsc;

use teseladb::engine::core::thread::async_ as in_thread;
use teseladb_io::DirectIo;
use teseladb_io::engine::disk::{DiskPlace, ShardDisk};
use teseladb_io::engine::{EngineConfig, NodeEngine};

/// Name of the variable that carries the choice to the child.
pub const EXECUTOR_VAR: &str = "TESELADB_EXECUTOR";

/// The ways the engine's files are written, with their names for `EXECUTOR_VAR`.
pub fn executors() -> Vec<(&'static str, DirectIo)> {
    vec![("direct", DirectIo::Auto), ("page-cache", DirectIo::Off)]
}

/// The choice named in `EXECUTOR_VAR` (`O_DIRECT` where it can when absent).
pub fn from_env() -> DirectIo {
    let name = std::env::var(EXECUTOR_VAR).unwrap_or_default();
    executors().into_iter().find(|(n, _)| *n == name).map_or(DirectIo::Auto, |(_, d)| d)
}

/// This process's engine, with `shards` shards writing files as `direct` says.
pub fn engine(shards: u16, direct: DirectIo) -> NodeEngine {
    let cfg = EngineConfig { shards, direct_io: direct, ..EngineConfig::default() };
    NodeEngine::start(&cfg).expect("the engine starts")
}

/// Starts `body` in an engine thread of `shard` without waiting for it.
pub fn spawn_on(e: &NodeEngine, shard: u16, body: impl FnOnce() + Send + 'static) {
    e.run_on(shard, move || drop(in_thread(body))).expect("the engine runs");
}

/// Runs `body` in an engine thread of `shard` and waits for what it gives.
pub fn run_on<T: Send + 'static>(
    e: &NodeEngine,
    shard: u16,
    body: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (tx, rx) = mpsc::sync_channel(1);
    spawn_on(e, shard, move || {
        let _ = tx.send(body());
    });
    rx.recv().expect("the body ran")
}

/// The executor of the files under `dir` on the calling shard (in an engine thread).
pub fn files(dir: impl AsRef<Path>) -> ShardDisk {
    DiskPlace::Files(dir.as_ref().to_path_buf()).on_shard().expect("the executor opens")
}
