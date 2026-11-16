//! Where the shards run and how much memory they have (012 FR-006, FR-013; 023 FR-027, FR-028,
//! T069, T071): shard `k` runs on the `k`-th CPU of `--cpu-list` given to the engine as its
//! cpuset, a list shorter than the cores or with a CPU the process may not use does not start a
//! node, `--memory-limit` is the engine's memory split evenly between the shards, and the start
//! logs the engine's line (`contracts/cli-engine.md`).
//!
//! One engine per process (D4): the nodes of this file start one at a time.
#![cfg(target_os = "linux")]

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use teseladb_io::engine::Placement;
use teseladb_io::engine::resources::allowed_cpus;
use teseladb_raftlog::MemDisk;
use teseladb_server::node::{Node, NodeConfig, StartupLog};

static ONE_NODE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn one_node_at_a_time() -> MutexGuard<'static, ()> {
    ONE_NODE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The CPUs a thread named `name` of this process may run on, from `/proc`.
fn cpus_of(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    for t in std::fs::read_dir("/proc/self/task").unwrap() {
        let dir = t.unwrap().path();
        if std::fs::read_to_string(dir.join("comm")).is_ok_and(|c| c.trim() == name) {
            let status = std::fs::read_to_string(dir.join("status")).unwrap();
            let line = status.lines().find(|l| l.starts_with("Cpus_allowed_list:")).unwrap();
            out.push(line.split_whitespace().nth(1).unwrap().to_string());
        }
    }
    out
}

/// The steps a node's start logs, as text.
fn steps() -> (StartupLog, Arc<Mutex<Vec<String>>>) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let keep = lines.clone();
    let log = StartupLog::new(move |msg, fields| {
        let mut l = msg.to_owned();
        for (k, v) in fields {
            l.push_str(&format!(" {k}={v}"));
        }
        keep.lock().unwrap_or_else(PoisonError::into_inner).push(l);
    });
    (log, lines)
}

#[test]
fn shard_k_runs_on_the_kth_cpu_of_the_list() {
    let _one = one_node_at_a_time();
    let allowed = allowed_cpus();
    assert!(allowed.len() >= 2, "the test needs two CPUs: {allowed:?}");
    // Given in decreasing order: the engine's cpuset is a set, so shard 0 takes the lower.
    let list: Vec<usize> = allowed.iter().rev().take(2).copied().collect();
    let (low, high) = (list[1], list[0]);
    let (log, lines) = steps();
    let cfg = NodeConfig {
        cores: Some(2),
        placement: Placement::List(list),
        memory_limit: Some(512 << 20),
        startup_log: log,
        ..NodeConfig::default()
    };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert_eq!(node.cpus(), [Some(low), Some(high)]);
    // Shard 0 is the engine's thread; the others are its reactors.
    assert_eq!(cpus_of("engine"), [low.to_string()]);
    assert_eq!(cpus_of("reactor-1"), [high.to_string()]);
    // The memory of the node, split evenly between its shards (FR-028).
    let memory = node.memory().expect("a node always has its memory");
    assert_eq!((memory.total, memory.per_core()), (512 << 20, 256 << 20));
    let line = lines
        .lock()
        .unwrap()
        .iter()
        .find(|l| l.starts_with("engine: backend "))
        .cloned()
        .expect("the engine's line");
    assert!(line.contains(&format!("shards 2 on CPUs {low},{high}")), "{line}");
    assert!(line.contains("memory 536870912 bytes"), "{line}");
    assert!(line.contains("files off"), "a node in memory uses the system's cache: {line}");
    node.stop();
}

#[test]
fn a_list_shorter_than_the_cores_or_outside_the_process_is_refused() {
    let _one = one_node_at_a_time();
    let first = allowed_cpus()[0];
    let short = NodeConfig {
        cores: Some(3),
        placement: Placement::List(vec![first]),
        ..NodeConfig::default()
    };
    let Err(e) = Node::start(short, || Ok(MemDisk::new()), None) else { panic!("started") };
    assert!(e.to_string().contains("--cpu-list names 1 CPUs for 3 cores"), "{e}");
    let outside = teseladb_io::engine::options::MAX_CPUS - 1;
    let cfg = NodeConfig {
        cores: Some(1),
        placement: Placement::List(vec![outside]),
        ..NodeConfig::default()
    };
    let Err(e) = Node::start(cfg, || Ok(MemDisk::new()), None) else { panic!("started") };
    assert!(e.to_string().contains(&format!("may not run on CPU {outside}")), "{e}");
}

#[test]
fn unpinned_shards_by_default_in_a_library_node_with_the_default_memory() {
    let _one = one_node_at_a_time();
    let cfg = NodeConfig { cores: Some(1), ..NodeConfig::default() };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert_eq!(node.cpus(), [None]);
    let default = teseladb_io::engine::resources::node_memory(None, 1);
    assert_eq!(node.memory().map(|m| m.total), Some(default));
    node.stop();
}
