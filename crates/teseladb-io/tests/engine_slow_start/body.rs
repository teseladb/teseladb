//! A node's start waits for its engine no longer than its limit (023 FR-051): for the engine to
//! run its first task, and for a shard to run each request of the start. Past the limit the start
//! fails with an error that names what did not answer, and the process is left able to start an
//! engine again. Before, a start whose engine came up late joined the engine's thread, which
//! waits for a stop nobody asks for, and a request to a shard that did not run it waited for
//! ever: a node in that state has no port, no error and no line in its log.
//!
//! It has its own `main`: it starts engines in sequence, and a wait that does not end would hang
//! a test harness instead of failing it.

use std::io;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use teseladb_io::engine::{EngineConfig, NodeEngine, wait};

/// The longest anything here may take: every wait under test ends in a few seconds.
const WATCH: Duration = Duration::from_secs(30);
/// How long a task holds shard 0, and the limit of the engine whose shard it holds.
const HOLD: Duration = Duration::from_secs(4);
const LIMIT: Duration = Duration::from_secs(1);

fn config(limit: Duration) -> EngineConfig {
    EngineConfig { shards: 2, start_limit: Some(limit), ..EngineConfig::default() }
}

/// What `f` gives, on a thread of its own; the process ends with an error if it gives nothing
/// within [`WATCH`], which is the wait without an end this test is about.
fn watched<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || drop(tx.send(f())));
    rx.recv_timeout(WATCH).unwrap_or_else(|_| {
        eprintln!("{what}: no answer in {} s", WATCH.as_secs());
        std::process::exit(1);
    })
}

pub fn main() {
    // No engine runs its first task a millisecond after its start: the start says so.
    let late = watched("a start whose engine comes up past its limit", || {
        NodeEngine::start(&config(Duration::from_millis(1))).map(|_| ())
    });
    let err = late.expect_err("an engine does not come up in a millisecond");
    assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
    assert!(err.to_string().contains("first task"), "{err}");
    // The engine that came up late was stopped: the next start takes the process's turn.
    let engine = watched("the start after it", || NodeEngine::start(&config(LIMIT)))
        .expect("the engine starts once the late one stopped");
    assert_eq!(engine.run_on(1, || 7).ok(), Some(7));
    // A shard held by a task runs no request: the request ends at the limit and names the
    // shard, and the other shard goes on answering.
    assert!(wait::post(0, Box::new(|| std::thread::sleep(HOLD))));
    let began = Instant::now();
    let err = engine.run_on(0, || 7).expect_err("shard 0 is held");
    assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
    assert!(err.to_string().contains("shard 0"), "{err}");
    assert!(began.elapsed() < HOLD, "it waited {:?} for a held shard", began.elapsed());
    assert_eq!(engine.run_on(1, || 8).ok(), Some(8));
    // Once the task ends the shard answers again.
    std::thread::sleep(HOLD);
    assert_eq!(engine.run_on(0, || 9).ok(), Some(9));
    watched("the stop", move || engine.stop());
    println!("a_start_and_its_requests_wait_for_the_engine_no_longer_than_their_limit: ok");
}
