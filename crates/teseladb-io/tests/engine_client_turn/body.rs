//! A thread that is not a shard reaches the engine of the process through a lease
//! (`lease_engine`): the node engine that runs, or else a client engine of one shard started for
//! it. Before, the client engine never stopped and took no turn: a node engine started after it
//! failed with «an engine already runs», and a client that found a node engine running posted to
//! it with `alien` after it stopped, which aborts the process (the bench's unit tests, which mix
//! both in one process, aborted in `submit_to_default`).
//!
//! It has its own `main`: it starts and stops engines one after the other.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use teseladb::engine::core::reactor::engine_is_ready;
use teseladb_io::engine::{EngineConfig, EngineLease, NodeEngine, lease_engine};

const T: Duration = Duration::from_secs(10);

/// Whether a closure posted with `lease` runs on a shard.
fn runs_on_a_shard(lease: &EngineLease) -> bool {
    let (tx, rx) = mpsc::channel();
    if !lease.post(move || drop(tx.send(engine_is_ready()))) {
        return false;
    }
    rx.recv_timeout(T).unwrap_or(false)
}

fn node_engine() -> NodeEngine {
    NodeEngine::start(&EngineConfig { shards: 1, ..EngineConfig::default() })
        .expect("the node engine starts")
}

/// A lease without an engine starts the client engine; a node engine that starts later stops
/// it once its lease is gone, and takes over.
fn a_node_engine_takes_the_turn_of_an_idle_client_engine() {
    let lease = lease_engine(true).expect("a client engine starts");
    assert!(runs_on_a_shard(&lease));
    // The node engine waits while the lease lives.
    let (tx, rx) = mpsc::channel();
    let starter = thread::spawn(move || {
        let e = node_engine();
        let _ = tx.send(());
        e
    });
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "the lease keeps the turn");
    assert!(runs_on_a_shard(&lease), "the client engine still runs under its lease");
    drop(lease);
    rx.recv_timeout(T).expect("the node engine starts once the lease is gone");
    let node = starter.join().expect("the starter");
    // A lease now reaches the node engine and starts nothing.
    let lease = lease_engine(false).expect("the node engine is the process's");
    assert!(runs_on_a_shard(&lease));
    drop(lease);
    node.stop();
    println!("a_node_engine_takes_the_turn_of_an_idle_client_engine: ok");
}

/// A lease of a node engine that stops fails its posts instead of aborting, and the next lease
/// starts a client engine.
fn a_lease_of_a_stopped_node_engine_fails_and_the_next_one_starts_a_client() {
    let node = node_engine();
    let lease = lease_engine(false).expect("the node engine is the process's");
    node.stop();
    assert!(!lease.post(|| {}), "a post to a stopped engine fails");
    drop(lease);
    assert!(lease_engine(false).is_err(), "no engine runs");
    let lease = lease_engine(true).expect("a client engine starts");
    assert!(runs_on_a_shard(&lease));
    drop(lease);
    // The client engine gives its turn to the next node engine.
    node_engine().stop();
    println!("a_lease_of_a_stopped_node_engine_fails_and_the_next_one_starts_a_client: ok");
}

pub fn main() {
    a_node_engine_takes_the_turn_of_an_idle_client_engine();
    a_lease_of_a_stopped_node_engine_fails_and_the_next_one_starts_a_client();
}
