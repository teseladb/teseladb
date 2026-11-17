//! Engines started one after another in one process (023 FR-012; a node stopped and started
//! again, as `node_restart` does): what one engine makes for itself is not used by the next.
//!
//! - The disk classes' scheduling groups are each engine's own: before, the first engine's groups
//!   were kept for the whole process and the second engine, whose reactors have no task queue for
//!   them, crashed on its first disk operation in a class. Each engine also gives their ids back
//!   when it exits: the process has a fixed number of them.
//! - The work handed to the threads that are not shards outlives its engine: when it ends while a
//!   later engine runs, its result reaches nobody. Before, the numbers of that work restarted with
//!   each engine, and a result of the first engine resolved a wait of the second one.
//!
//! One engine at a time (D4): the cases take turns. Linux only: the engine runs on Linux.
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::{Mutex, PoisonError, mpsc};
use std::time::Duration;

use teseladb::engine::core::future::Future;
use teseladb::engine::core::reactor::internal::scheduling_group_count;
use teseladb::engine::core::thread::async_ as in_thread;
use teseladb_io::IoClass;
use teseladb_io::engine::blocking::offload;
use teseladb_io::engine::disk::{BACKGROUND_SHARES, DATA_SHARES, IoGroups, LOG_SHARES, ShardDisk};
use teseladb_io::engine::{EngineConfig, NodeEngine};
use teseladb_raftlog::{FileId, IoDone, IoOp};
use teseladb_types::Bytes;

/// Engines started in turn: more than the ids of the process hold sets of three groups for.
const ROUNDS: usize = 6;
/// Shards of each engine.
const SHARDS: u16 = 2;
/// How long the first engine's work on the threads that are not shards keeps them busy: long
/// enough for that engine to stop and the next one to start and queue its own work behind it.
const OUTLIVES: Duration = Duration::from_secs(3);
/// Works queued by each engine of the offload case.
const WORKS: u64 = 32;

/// A directory of its own, removed when dropped.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let p = std::env::temp_dir()
            .join(format!("teseladb-engine-restart-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        Dir(p)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn start() -> NodeEngine {
    NodeEngine::start(&EngineConfig { shards: SHARDS, ..EngineConfig::default() })
        .expect("the engine starts")
}

/// Runs `body` in an engine thread of `shard` and waits for what it gives.
fn in_engine_thread<T: Send + 'static>(
    e: &NodeEngine,
    shard: u16,
    body: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (tx, rx) = mpsc::sync_channel(1);
    e.run_on(shard, move || {
        drop(in_thread(move || {
            let _ = tx.send(body());
        }));
    })
    .expect("the engine runs");
    rx.recv().expect("the body ran")
}

/// One operation of each class on the files under `root`, in the groups of the running engine.
fn use_every_class(root: PathBuf) -> Result<(), String> {
    let groups = IoGroups::get_or_create().get().map_err(|e| e.to_string())?;
    let shares = [LOG_SHARES, DATA_SHARES, BACKGROUND_SHARES];
    let classes = [IoClass::Log, IoClass::Data, IoClass::Background];
    for (class, shares) in classes.iter().zip(shares) {
        if groups.of(*class).get_shares() != shares {
            return Err(format!("{class:?} has the shares of another group"));
        }
    }
    let disk = ShardDisk::open(root, groups).get().map_err(|e| e.to_string())?;
    let mut file = 0;
    for class in classes {
        file += 1;
        let create = IoOp::Create { file: FileId(file), path: format!("f{file}").into(), prealloc: 0 };
        let data = Bytes::copy_from_slice(b"kept");
        let write = IoOp::WriteAt { file: FileId(file), offset: 0, data, batch: 0 };
        for op in [create, write] {
            match disk.call(op, class).get().map_err(|e| e.to_string())? {
                IoDone::Ok(_) => {}
                other => return Err(format!("{class:?}: {other:?}")),
            }
        }
    }
    Ok(())
}

fn the_disk_groups_of_each_engine_are_its_own() {
    let dir = Dir::new("groups");
    let mut after_first = None;
    for round in 0..ROUNDS {
        let e = start();
        for shard in 0..SHARDS {
            let root = dir.0.join(format!("r{round}-s{shard}"));
            let done = in_engine_thread(&e, shard, move || use_every_class(root));
            assert_eq!(done, Ok(()), "engine {round}, shard {shard}");
        }
        e.stop();
        // The groups' ids went back: every engine leaves the process as the first one left it.
        let count = scheduling_group_count();
        assert_eq!(*after_first.get_or_insert(count), count, "engine {round} kept group ids");
    }
}

fn an_offloaded_work_of_a_stopped_engine_answers_nobody_in_the_next() {
    let first = start();
    // Both threads busy past the engine's stop, and works queued behind them.
    first
        .run_on(0, || {
            for n in 0..WORKS {
                let f: Future<u64> = offload(move || {
                    if n < 2 {
                        std::thread::sleep(OUTLIVES);
                    }
                    1
                });
                drop(f);
            }
        })
        .expect("the engine runs");
    first.stop();
    let second = start();
    // Queued behind the first engine's works, which end while this engine runs.
    let got = in_engine_thread(&second, 0, || {
        let works: Vec<Future<u64>> = (0..WORKS).map(|_| offload(|| 2)).collect();
        works.into_iter().map(|f| f.get().map_err(|e| e.to_string())).collect::<Vec<_>>()
    });
    second.stop();
    for (n, r) in got.iter().enumerate() {
        assert_eq!(r, &Ok(2), "work {n} of the second engine got another engine's result");
    }
}

/// One engine at a time in the process (D4): the cases take turns.
static ONE_ENGINE: Mutex<()> = Mutex::new(());

#[test]
fn disk_groups_of_each_engine_are_its_own() {
    let _turn = ONE_ENGINE.lock().unwrap_or_else(PoisonError::into_inner);
    the_disk_groups_of_each_engine_are_its_own();
}

#[test]
fn offloaded_work_of_a_stopped_engine_answers_nobody_in_the_next() {
    let _turn = ONE_ENGINE.lock().unwrap_or_else(PoisonError::into_inner);
    an_offloaded_work_of_a_stopped_engine_answers_nobody_in_the_next();
}
