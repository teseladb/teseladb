//! Engines started one after the other in one process (D4: one engine at a time): each makes
//! its own disk classes (023 FR-012) and runs work in them, as many times as a process restarts
//! its node; the scheduling groups of an engine that stopped are not used again (past the
//! engine's number of groups, the classes of a later engine run in its default group).
//!
//! It has its own `main`: it starts and stops engines in sequence.

use std::sync::mpsc;
use std::time::Duration;

use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::reactor::internal_misc::run_in_background;
use teseladb::engine::core::with_scheduling_group::with_scheduling_group;
use teseladb_io::IoClass;
use teseladb_io::engine::disk::IoGroups;
use teseladb_io::engine::{EngineConfig, NodeEngine, wait};

/// More engines than the engine has scheduling groups for three classes each.
const ENGINES: usize = 8;
const T: Duration = Duration::from_secs(10);

/// Makes (or takes) the disk classes on shard 0 and runs a task in each; whether all ran.
fn work_in_every_class() -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    assert!(wait::post(
        0,
        Box::new(move || {
            let ran = IoGroups::get_or_create().then(|g: IoGroups| {
                let run = move |c| with_scheduling_group(g.of(c), || make_ready_future(()));
                run(IoClass::Log)
                    .then(move |()| run(IoClass::Data))
                    .then(move |()| run(IoClass::Background))
            });
            run_in_background(ran.then_wrapped(move |f: Future<()>| {
                let _ = tx.send(f.get().map_err(|e: ExceptionPtr| e.to_string()));
                make_ready_future(())
            }));
        })
    ));
    rx.recv_timeout(T).expect("shard 0 answers")
}

pub fn main() {
    for i in 0..ENGINES {
        let e = NodeEngine::start(&EngineConfig { shards: 2, ..EngineConfig::default() })
            .expect("the engine starts");
        assert_eq!(work_in_every_class(), Ok(()), "engine {i}");
        e.stop();
    }
    println!("each_engine_of_the_process_runs_work_in_its_own_disk_classes: ok");
}
