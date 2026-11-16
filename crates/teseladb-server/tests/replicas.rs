//! Nodes that join get replicas of the data range (005 FR-016, FR-012): with three nodes the
//! balancer on Group 0's leader asks for three replicas, the range's leader adds the two new nodes
//! as learners and promotes them, and Group 0 records the change as settled. The founder's log is
//! compacted before they join, so each one receives the range by export and ingestion from
//! another replica. Losing the founder then leaves a range that elects a leader, keeps every
//! document and takes writes. With two cores, Group 0 and the range are on different cores. On
//! Linux the same runs on io_uring disks, where the pieces of the exported files go from the ring's
//! registered buffers to the joining nodes (012 FR-026).

#[cfg(target_os = "linux")]
#[path = "common/procs.rs"]
mod procs;

#[cfg(target_os = "linux")]
use std::thread;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use teseladb_lsm::LsmConfig;
#[cfg(target_os = "linux")]
use teseladb_meta::bootstrap::DATA_START;
#[cfg(target_os = "linux")]
use teseladb_meta::group0::ReplicaRole;
#[cfg(target_os = "linux")]
use teseladb_raft::Role;
#[cfg(target_os = "linux")]
use teseladb_server::data::DataStatus;
#[cfg(target_os = "linux")]
use teseladb_server::node::NodeConfig;
#[cfg(target_os = "linux")]
use teseladb_server::procs::{NodeProc, start_self};
#[cfg(target_os = "linux")]
use teseladb_types::NodeId;

#[cfg(target_os = "linux")]
fn main() {
    let first: procs::Test = (
        "nodes_that_join_receive_the_range_and_survive_the_loss_of_the_founder",
        nodes_that_join_receive_the_range_and_survive_the_loss_of_the_founder,
    );
    let tests: Vec<procs::Test> = std::iter::once(first).chain(linux_tests()).collect();
    procs::main(&tests, &[("replicas", |a| config(a.join.clone()))]);
}

/// The tests of io_uring, which is Linux's.
#[cfg(target_os = "linux")]
fn linux_tests() -> Vec<procs::Test> {
    vec![(
        "nodes_on_io_uring_disks_receive_the_range_from_the_ring_s_buffers",
        nodes_on_io_uring_disks_receive_the_range_from_the_ring_s_buffers,
    )]
}

/// The tests of io_uring, which is Linux's: none here.
#[cfg(target_os = "linux")]
#[cfg(not(target_os = "linux"))]
fn linux_tests() -> Vec<procs::Test> {
    Vec::new()
}

#[cfg(target_os = "linux")]
const READY: Duration = Duration::from_secs(30);
#[cfg(target_os = "linux")]
const ASK: Duration = Duration::from_secs(2);
#[cfg(target_os = "linux")]
const DOCS: u32 = 400;

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn key(i: u32) -> Vec<u8> {
    [&DATA_START[..], format!("doc-{i:05}").as_bytes()].concat()
}

#[cfg(target_os = "linux")]
fn value(i: u32) -> Vec<u8> {
    format!("{i:05}-{}", "w".repeat(300)).into_bytes()
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn status(n: &NodeProc) -> DataStatus {
    n.data_status(ASK).expect("the range's core answers")
}

/// Writes `i` on whichever node leads the range.
#[cfg(target_os = "linux")]
fn put(nodes: &[&NodeProc], i: u32) {
    until(&format!("write {i}"), || nodes.iter().find_map(|n| n.put(&key(i), &value(i), ASK).ok()));
}

#[cfg(target_os = "linux")]
fn nodes_that_join_receive_the_range_and_survive_the_loss_of_the_founder() {
    join_and_lose_the_founder(|join| start_self("replicas", &join, None, &[]));
}

#[cfg(target_os = "linux")]
fn nodes_on_io_uring_disks_receive_the_range_from_the_ring_s_buffers() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use teseladb_io::node_disk::{DirectIo, IoExecutor, choose};
    use teseladb_server::procs::{DISK, URING};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let base = std::env::temp_dir().join(format!("teseladb-replicas-uring-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let choice = choose(IoExecutor::Uring, DirectIo::Auto, false, &std::env::temp_dir()).unwrap();
    assert!(choice.uring, "{choice}");
    // Each node process on io_uring disks in a directory of its own.
    let uring = format!("{DISK}={URING}");
    join_and_lose_the_founder(|join| {
        let dir = base.join(NEXT.fetch_add(1, Ordering::Relaxed).to_string());
        start_self("replicas", &join, Some(&dir), &[&uring])
    });
    std::fs::remove_dir_all(&base).unwrap();
}

/// The test with each node from `start`, which takes the addresses to join.
#[cfg(target_os = "linux")]
fn join_and_lose_the_founder(start: impl Fn(Vec<String>) -> NodeProc) {
    let n1 = start(Vec::new());
    assert!(n1.wait_ready(READY));
    until("a leader", || (status(&n1).role == Some(Role::Leader)).then_some(()));
    for i in 0..DOCS {
        put(&[&n1], i);
    }
    let seed = vec![n1.peer_addr().to_string()];
    let n2 = start(seed.clone());
    let n3 = start(seed);
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

/// The tests run on Linux only (D-177): elsewhere the program does nothing.
#[cfg(not(target_os = "linux"))]
fn main() {}
