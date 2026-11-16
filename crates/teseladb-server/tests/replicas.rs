//! Nodes that join get replicas of the data range (005 FR-016, FR-012): with three nodes the
//! balancer on Group 0's leader asks for three replicas, the range's leader adds the two new nodes
//! as learners and promotes them, and Group 0 records the change as settled. The founder's log is
//! compacted before they join, so each one receives the range by export and ingestion from
//! another replica. Losing the founder then leaves a range that elects a leader, keeps every
//! document and takes writes. With two cores, Group 0 and the range are on different cores. The
//! same runs on the engine's files, where the pieces of the exported files go from the disk
//! executor's reads to the joining nodes (012 FR-026, 023 FR-017).

use std::thread;
use std::time::{Duration, Instant};

use teseladb_io::engine::disk::DiskPlace;
use teseladb_lsm::LsmConfig;
use teseladb_meta::bootstrap::DATA_START;
use teseladb_meta::group0::ReplicaRole;
use teseladb_raft::Role;
use teseladb_server::data::DataStatus;
use teseladb_server::node::{Node, NodeConfig};
use teseladb_types::NodeId;

const READY: Duration = Duration::from_secs(30);
const ASK: Duration = Duration::from_secs(2);
const DOCS: u32 = 400;

fn config(join: Vec<String>) -> NodeConfig {
    NodeConfig {
        // Two cores: Group 0 on core 0 and the range on core 1, which talk through messages.
        cores: Some(2),
        join,
        // Snapshots every 32 entries and a memtable of 16 KiB: the founder's log is compacted
        // well before the others join.
        snapshot_entries: 32,
        lsm: LsmConfig {
            memtable_bytes: 16 << 10,
            block_bytes: 4096,
            target_file_bytes: 64 << 10,
            l1_bytes: 256 << 10,
            cache_bytes: 1 << 20,
            ..LsmConfig::default()
        },
        ..NodeConfig::default()
    }
}

fn start(join: Vec<String>, place: DiskPlace) -> Node {
    Node::start(config(join), place, None).expect("the node starts")
}

fn key(i: u32) -> Vec<u8> {
    [&DATA_START[..], format!("doc-{i:05}").as_bytes()].concat()
}

fn value(i: u32) -> Vec<u8> {
    format!("{i:05}-{}", "w".repeat(300)).into_bytes()
}

fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + READY;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "never: {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn status(n: &Node) -> DataStatus {
    n.data_status(ASK).expect("the range's core answers")
}

/// Writes `i` on whichever node leads the range.
fn put(nodes: &[&Node], i: u32) {
    until(&format!("write {i}"), || nodes.iter().find_map(|n| n.put(&key(i), &value(i), ASK).ok()));
}

#[test]
fn nodes_that_join_receive_the_range_and_survive_the_loss_of_the_founder() {
    join_and_lose_the_founder(|_| DiskPlace::Memory);
}

#[cfg(target_os = "linux")]
#[test]
fn nodes_on_the_engine_s_files_receive_the_range_from_its_reads() {
    let base = std::env::temp_dir().join(format!("teseladb-replicas-files-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    join_and_lose_the_founder(|k| DiskPlace::Files(base.join(k.to_string())));
    std::fs::remove_dir_all(&base).unwrap();
}

/// Three nodes, the disks of node `k` at `place(k)`.
fn join_and_lose_the_founder(place: impl Fn(u32) -> DiskPlace) {
    let n1 = start(Vec::new(), place(1));
    assert!(n1.wait_ready(READY));
    until("a leader", || (status(&n1).role == Some(Role::Leader)).then_some(()));
    for i in 0..DOCS {
        put(&[&n1], i);
    }
    let seed = vec![n1.peer_addr().to_string()];
    let n2 = start(seed.clone(), place(2));
    let n3 = start(seed, place(3));
    assert!(n2.wait_ready(READY) && n3.wait_ready(READY));
    let three = [NodeId(1), NodeId(2), NodeId(3)];
    for n in [&n1, &n2, &n3] {
        until("three voters", || (status(n).voters == three).then_some(()));
    }
    // Group 0 records the change as settled, with three voters.
    let desc = until("settled", || n1.view(ASK)?.data_range.filter(|d| !d.settling));
    assert!(desc.replicas.iter().all(|(_, r)| *r == ReplicaRole::Voter), "{desc:?}");
    assert_eq!(desc.nodes(), three);
    // Each new replica received the data by export and ingestion.
    for n in [&n2, &n3] {
        let s = status(n);
        assert!(s.ingested >= 1 && !s.waiting_data, "{:?}: {s:?}", n.node_id());
    }
    let applied = status(&n1).applied;
    for n in [&n2, &n3] {
        until("caught up", || (status(n).applied >= applied).then_some(()));
    }
    n1.stop();

    let alive = [&n2, &n3];
    until("a new leader", || {
        alive.iter().any(|n| status(n).role == Some(Role::Leader)).then_some(())
    });
    put(&alive, DOCS);
    let leader =
        until("the leader", || alive.iter().find(|n| status(n).role == Some(Role::Leader)));
    for i in 0..=DOCS {
        assert_eq!(leader.get(&key(i), ASK).unwrap(), Some(value(i)), "document {i}");
    }
    n2.stop();
    n3.stop();
}
