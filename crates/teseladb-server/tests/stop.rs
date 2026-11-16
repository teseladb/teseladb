//! The stop of a node through the engine's signals (023 FR-031, T073): `SIGTERM` reaches shard 0's
//! reactor, the node stops in order and ends with 0, and every write confirmed before the signal,
//! with `w: 1` and with `w: "majority"` and the journal, is on disk when the node starts again on
//! the same directory. `SIGINT` stops it the same way.
//!
//! One process per node (D4): the node runs in a process of its own (`common/procs.rs`).

#[cfg(target_os = "linux")]
#[path = "common/procs.rs"]
mod procs;

#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use teseladb_range::ack::{W, WriteConcern};
#[cfg(target_os = "linux")]
use teseladb_server::node::NodeConfig;
#[cfg(target_os = "linux")]
use teseladb_server::procs::{NodeArgs, NodeProc, start_self};
#[cfg(target_os = "linux")]
use teseladb_sys::process::{Stop, send_stop};

#[cfg(target_os = "linux")]
fn main() {
    procs::main(
        &[
            (
                "writes_confirmed_before_sigterm_are_on_disk_after_the_stop",
                writes_confirmed_before_sigterm_are_on_disk_after_the_stop,
            ),
            ("sigint_stops_the_node_in_order", sigint_stops_the_node_in_order),
        ],
        &[("node", node_config)],
    );
}

/// The tests run on Linux only (D-177): elsewhere the program does nothing.
#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
const READY: Duration = Duration::from_secs(30);
#[cfg(target_os = "linux")]
const ASK: Duration = Duration::from_secs(5);
/// Longest a stop takes: the quiesce period (15 s at most) and the cores' writes (10 s).
#[cfg(target_os = "linux")]
const STOP_LIMIT: Duration = Duration::from_secs(30);
/// Writes of each kind.
#[cfg(target_os = "linux")]
const WRITES: u32 = 200;

#[cfg(target_os = "linux")]
fn node_config(_: &NodeArgs) -> NodeConfig {
    NodeConfig { cores: Some(2), ..NodeConfig::default() }
}

/// A data directory of its own, removed at the end.
#[cfg(target_os = "linux")]
struct Dir(PathBuf);

#[cfg(target_os = "linux")]
impl Dir {
    fn new(name: &str) -> Dir {
        let p = std::env::temp_dir().join(format!("teseladb-stop-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Dir(p)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

#[cfg(target_os = "linux")]
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(target_os = "linux")]
fn key(kind: &str, i: u32) -> Vec<u8> {
    format!("stop-{kind}-{i:05}").into_bytes()
}

/// Writes until the node takes it (its range may not lead yet right after a start).
#[cfg(target_os = "linux")]
fn put(n: &NodeProc, k: &[u8], wc: WriteConcern) {
    let end = Instant::now() + READY;
    loop {
        match n.put_with(k, k, wc, ASK) {
            Ok(_) => return,
            Err(e) => assert!(Instant::now() < end, "write {k:?}: {e:?}"),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Reads `k` once the node answers (its range may not lead yet right after a start).
#[cfg(target_os = "linux")]
fn get(n: &NodeProc, k: &[u8]) -> Option<Vec<u8>> {
    let end = Instant::now() + READY;
    loop {
        match n.get(k, ASK) {
            Ok(v) => return v,
            Err(e) => assert!(Instant::now() < end, "read {k:?}: {e:?}"),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Sends `sig` to the node and waits for its end: 0, within [`STOP_LIMIT`].
#[cfg(target_os = "linux")]
fn stop_with(n: &mut NodeProc, sig: Stop) {
    let start = Instant::now();
    send_stop(n.pid(), sig).unwrap();
    let status = loop {
        if let Some(s) = n.ended() {
            break s;
        }
        assert!(start.elapsed() < STOP_LIMIT, "the node still runs after {sig:?}");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(0), "{sig:?}: {status}\n{}", n.log_text());
}

#[cfg(target_os = "linux")]
fn writes_confirmed_before_sigterm_are_on_disk_after_the_stop() {
    let dir = Dir::new("sigterm");
    let mut n = start_self("node", &[], Some(dir.path()), &[]);
    assert!(n.wait_ready(READY));
    let w1 = WriteConcern { w: W::Nodes(1), j: false };
    let majority = WriteConcern { w: W::Majority, j: true };
    for i in 0..WRITES {
        put(&n, &key("w1", i), w1);
        put(&n, &key("majority", i), majority);
    }
    stop_with(&mut n, Stop::Terminate);
    n.restart().expect("the node starts again on its directory");
    assert!(n.wait_ready(READY));
    for i in 0..WRITES {
        for kind in ["w1", "majority"] {
            let k = key(kind, i);
            assert_eq!(get(&n, &k), Some(k.clone()), "{kind} write {i}");
        }
    }
    assert!(n.stop().success());
}

#[cfg(target_os = "linux")]
fn sigint_stops_the_node_in_order() {
    let dir = Dir::new("sigint");
    let mut n = start_self("node", &[], Some(dir.path()), &[]);
    assert!(n.wait_ready(READY));
    let k = key("sigint", 0);
    put(&n, &k, WriteConcern { w: W::Majority, j: true });
    stop_with(&mut n, Stop::Interrupt);
    n.restart().expect("the node starts again on its directory");
    assert!(n.wait_ready(READY));
    assert_eq!(get(&n, &k), Some(k.clone()));
    assert!(n.stop().success());
}
