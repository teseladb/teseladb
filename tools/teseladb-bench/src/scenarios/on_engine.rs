//! The disk scenarios on the engine (023 FR-010): each runs in an engine thread of shard 0 of an
//! engine of its own, started for the scenario and stopped after it, and writes its files through
//! the engine's disk executor ([`BenchDisk`]), as a node's cores do. The engine runs on Linux only
//! (D3): elsewhere these scenarios say so.

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

#[cfg(target_os = "linux")]
pub use linux::{BenchDisk, run, sleep};
#[cfg(not(target_os = "linux"))]
pub use other::{BenchDisk, run, sleep};

/// One engine runs in a process at a time (D4): whoever starts one (a disk scenario here, the
/// open-loop generator) holds this turn until it stopped it, so the scenarios that run at once in
/// one process (the tests) take turns instead of failing.
static ENGINE_TURN: Mutex<()> = Mutex::new(());

/// The variable that sets the mode of the scenarios' engine files (`auto`, `on` or `off`, as a
/// node's `--direct-io`): `off` keeps the kernel's page cache, so the reads of the files compare
/// with reads that do not go through the engine.
pub const DIRECT_IO_VAR: &str = "TESELADB_BENCH_DIRECT_IO";

/// The process's turn to run an engine, held until dropped.
pub fn engine_turn() -> MutexGuard<'static, ()> {
    ENGINE_TURN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The executor of the files under `path`, on the calling shard.
///
/// # Errors
///
/// When the directory cannot be made.
pub fn disk(path: &Path) -> Result<BenchDisk, String> {
    BenchDisk::open(path)
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::time::Duration;

    use teseladb::engine::core::sleep::sleep_steady;
    use teseladb::engine::core::thread::async_ as in_thread;
    use teseladb_io::IoClass;
    use teseladb_io::engine::disk::{DiskPlace, ShardDisk};
    use teseladb_io::engine::{DirectIo, EngineConfig, NodeEngine};
    use teseladb_raftlog::{Disk, IoDone, IoErrno, IoOp};

    use super::DIRECT_IO_VAR;
    use crate::scenarios::msg;

    /// Runs `scenario` in an engine thread of shard 0 of an engine of its own, and stops the
    /// engine after it.
    ///
    /// # Errors
    ///
    /// The scenario's, or the engine's when it cannot start.
    pub fn run<T: Send + 'static>(
        scenario: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let _turn = super::engine_turn();
        let cfg = EngineConfig { shards: 1, direct_io: direct_io()?, ..EngineConfig::default() };
        let e = NodeEngine::start(&cfg).map_err(msg)?;
        let (tx, rx) = mpsc::sync_channel(1);
        let ran = e.run_on(0, move || {
            drop(in_thread(move || {
                let _ = tx.send(scenario());
            }));
        });
        let result = match ran {
            Ok(()) => rx.recv().unwrap_or_else(|_| Err("the engine stopped".to_owned())),
            Err(err) => Err(msg(err)),
        };
        e.stop();
        result
    }

    /// The mode of the engine's files, as a node's `--direct-io` (023 FR-033): from
    /// [`DIRECT_IO_VAR`], `auto` without it.
    fn direct_io() -> Result<DirectIo, String> {
        match std::env::var(DIRECT_IO_VAR) {
            Err(_) => Ok(DirectIo::Auto),
            Ok(v) => DirectIo::parse(&v)
                .ok_or_else(|| format!("{DIRECT_IO_VAR}: {v:?} is not auto, on or off")),
        }
    }

    /// Waits `d` in the scenario's engine thread, letting the shard run meanwhile.
    pub fn sleep(d: Duration) {
        let _ = sleep_steady(d).get();
    }

    /// The engine's disk executor of a scenario's files.
    #[derive(Debug, Clone)]
    pub struct BenchDisk(ShardDisk);

    impl BenchDisk {
        pub(super) fn open(path: &Path) -> Result<BenchDisk, String> {
            let place = DiskPlace::Files(PathBuf::from(path));
            place.on_shard().map(BenchDisk).map_err(|e| format!("{}: {e}", path.display()))
        }

        /// Hands `op` to the executor in the log's class; its result comes from
        /// [`BenchDisk::wait_done`].
        pub fn submit(&self, op: IoOp) {
            self.0.submit(op, IoClass::Log);
        }

        /// Waits for results and appends them to `out`, in submission order.
        pub fn wait_done(&self, out: &mut Vec<IoDone>) {
            if self.0.ready().get().is_err() {
                return;
            }
            let mut got = Vec::new();
            self.0.take_done(&mut got);
            out.extend(got.into_iter().map(|(d, _)| d));
        }
    }

    impl Disk for BenchDisk {
        fn list(&mut self, dir: &Path) -> Result<Vec<PathBuf>, IoErrno> {
            self.0.list(dir)
        }

        fn read_all(&mut self, path: &Path) -> Result<Vec<u8>, IoErrno> {
            self.0.read_all(path)
        }

        fn execute(&mut self, op: IoOp) -> IoDone {
            self.0.execute(op)
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod other {
    use std::convert::Infallible;
    use std::path::{Path, PathBuf};

    use teseladb_raftlog::{Disk, IoDone, IoErrno, IoOp};

    /// Fails: the engine runs on Linux only (D3).
    ///
    /// # Errors
    ///
    /// Always.
    pub fn run<T: Send + 'static>(
        _scenario: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        Err("this scenario writes through the engine, which runs on Linux only".to_owned())
    }

    /// Unreachable: no scenario runs without the engine.
    pub fn sleep(_d: std::time::Duration) {}

    /// The engine's disk executor of a scenario's files (Linux only).
    #[derive(Debug, Clone)]
    pub struct BenchDisk {
        never: Infallible,
    }

    impl BenchDisk {
        pub(super) fn open(_path: &Path) -> Result<BenchDisk, String> {
            Err("the engine runs on Linux only".to_owned())
        }

        /// Unreachable.
        pub fn submit(&self, _op: IoOp) {
            match self.never {}
        }

        /// Unreachable.
        pub fn wait_done(&self, _out: &mut Vec<IoDone>) {
            match self.never {}
        }
    }

    impl Disk for BenchDisk {
        fn list(&mut self, _dir: &Path) -> Result<Vec<PathBuf>, IoErrno> {
            match self.never {}
        }

        fn read_all(&mut self, _path: &Path) -> Result<Vec<u8>, IoErrno> {
            match self.never {}
        }

        fn execute(&mut self, _op: IoOp) -> IoDone {
            match self.never {}
        }
    }
}
