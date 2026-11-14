//! Where the cores run (012 FR-006, acceptance scenario 1 of story 3): core `k` runs on the
//! `k`-th CPU of `--cpu-list`, and a list shorter than the cores does not start a node.

use teseladb_raftlog::MemDisk;
use teseladb_server::affinity::Affinity;
use teseladb_server::node::{Node, NodeConfig};

/// The tests of this file start nodes one at a time: the threads of two nodes would share the
/// names `core-0` and `core-1` that [`cpus_of`] looks for.
static ONE_NODE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn one_node_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    ONE_NODE_AT_A_TIME.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The CPUs a thread named `name` of this process may run on, from `/proc` (Linux).
#[cfg(target_os = "linux")]
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

#[test]
fn core_k_runs_on_the_kth_cpu_of_the_list() {
    let _one = one_node_at_a_time();
    let allowed = teseladb_sys::numa::allowed_cpus().unwrap();
    let list: Vec<usize> = allowed.iter().rev().take(2).copied().collect();
    let cfg = NodeConfig {
        cores: Some(2),
        affinity: Affinity::List(list.clone()),
        ..NodeConfig::default()
    };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert_eq!(node.cpus(), [Some(list[0]), Some(list[1])]);
    #[cfg(target_os = "linux")]
    {
        assert_eq!(cpus_of("core-0"), [list[0].to_string()]);
        assert_eq!(cpus_of("core-1"), [list[1].to_string()]);
    }
    node.stop();
}

#[test]
fn a_list_shorter_than_the_cores_is_refused() {
    let _one = one_node_at_a_time();
    let cfg =
        NodeConfig { cores: Some(3), affinity: Affinity::List(vec![0]), ..NodeConfig::default() };
    let Err(e) = Node::start(cfg, || Ok(MemDisk::new()), None) else { panic!("started") };
    assert!(e.to_string().contains("--cpu-list names 1 CPUs for 3 cores"), "{e}");
}

#[test]
fn unpinned_cores_by_default_in_a_library_node() {
    let _one = one_node_at_a_time();
    let cfg = NodeConfig { cores: Some(1), ..NodeConfig::default() };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert_eq!(node.cpus(), [None]);
    node.stop();
}
