//! Ranges split by size on a running node and their replicas follow the cluster (007 FR-003,
//! FR-005, FR-007, FR-022): with a small split size, writing a few hundred documents splits the
//! data range several times while the writes go on; the halves cover the data space without a
//! gap and every document stays readable through the routing table. Two nodes that join then get
//! replicas of every range, those born from a split as a snapshot (their logs lack the data the
//! split left in the founder's LSM), and losing the founder keeps every document and takes writes.

use std::thread;
use std::time::{Duration, Instant};

use teseladb_lsm::LsmConfig;
use teseladb_meta::bootstrap::{DATA_END, DATA_RANGE, DATA_START};
use teseladb_raft::Role;
use teseladb_raftlog::MemDisk;
use teseladb_router::table::RouteEntry;
use teseladb_server::node::{Node, NodeConfig};
use teseladb_types::NodeId;

const READY: Duration = Duration::from_secs(60);
const ASK: Duration = Duration::from_secs(2);
const DOCS: u32 = 600;
/// Split size: a few dozen documents.
const SPLIT_BYTES: u64 = 24 << 10;

fn config(join: Vec<String>) -> NodeConfig {
    NodeConfig {
        cores: Some(2),
        join,
        snapshot_entries: 32,
        split_bytes: SPLIT_BYTES,
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

fn start(join: Vec<String>) -> Node<MemDisk> {
    Node::start(config(join), || Ok(MemDisk::new()), None).expect("the node starts")
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

/// Writes `i` on whichever node leads its range.
fn put(nodes: &[&Node<MemDisk>], i: u32) {
    until(&format!("write {i}"), || nodes.iter().find_map(|n| n.put(&key(i), &value(i), ASK).ok()));
}

/// Reads `i` on whichever node leads its range.
fn get(nodes: &[&Node<MemDisk>], i: u32) -> Option<Vec<u8>> {
    until(&format!("read {i}"), || nodes.iter().find_map(|n| n.get(&key(i), ASK).ok()))
}

/// The ranges cover the data space, one after the other.
fn contiguous(ranges: &[RouteEntry]) -> bool {
    let chained = ranges.windows(2).all(|w| w[0].end == w[1].start);
    let first = ranges.first().is_some_and(|r| r.start == DATA_START);
    let last = ranges.last().is_some_and(|r| r.end == DATA_END);
    chained && first && last
}

#[test]
fn a_range_splits_while_it_takes_writes_and_its_halves_get_replicas() {
    let n1 = start(Vec::new());
    assert!(n1.wait_ready(READY));
    until("a leader", || (n1.data_status(ASK)?.role == Some(Role::Leader)).then_some(()));
    for i in 0..DOCS {
        put(&[&n1], i);
    }
    // A split Group 0 recorded may still be on its way when the writes end: each half's state
    // machine ends up with the bounds Group 0 gives it.
    let ranges = until("splits applied as Group 0 records them", || {
        let ranges = n1.ranges();
        let applied = ranges.iter().all(|r| {
            n1.range_status(r.group, ASK)
                .is_some_and(|s| s.bounds == (r.start.clone(), r.end.clone()))
        });
        (ranges.len() >= 4 && applied).then_some(ranges)
    });
    assert!(contiguous(&ranges), "{ranges:?}");
    assert!(ranges.iter().all(|r| r.generation >= 1 && r.core == ranges[0].core));
    for i in 0..DOCS {
        assert_eq!(get(&[&n1], i), Some(value(i)), "document {i}");
    }

    let seed = vec![n1.peer_addr().to_string()];
    let n2 = start(seed.clone());
    let n3 = start(seed);
    assert!(n2.wait_ready(READY) && n3.wait_ready(READY));
    let three = [NodeId(1), NodeId(2), NodeId(3)];
    // The ranges may still split while the replicas move: wait until the table settles.
    let ranges = until("every range with three voters", || {
        let ranges = n1.ranges();
        let all = ranges.iter().all(|r| {
            r.voters == three
                && [&n1, &n2, &n3]
                    .iter()
                    .all(|n| n.range_status(r.group, ASK).is_some_and(|s| s.voters == three))
        });
        all.then_some(ranges)
    });
    assert!(contiguous(&ranges), "{ranges:?}");
    // The replicas of ranges born from a split arrived as snapshots.
    let born = ranges.iter().filter(|r| r.group != DATA_RANGE).count();
    assert!(born >= 3);
    // Ranges born before the two nodes joined reach them as snapshots (those born later get
    // their keys from the parent's cut on every replica).
    for n in [&n2, &n3] {
        let s = n.data_status(ASK).expect("the range's core answers");
        assert!(s.ingested >= 2, "{:?}: {s:?}", n.node_id());
    }
    for r in &ranges {
        let applied = n1.range_status(r.group, ASK).map_or(0, |s| s.applied);
        for n in [&n2, &n3] {
            until("caught up", || (n.range_status(r.group, ASK)?.applied >= applied).then_some(()));
        }
    }
    n1.stop();

    let alive = [&n2, &n3];
    for i in 0..DOCS {
        assert_eq!(get(&alive, i), Some(value(i)), "document {i}");
    }
    for i in DOCS..DOCS + 50 {
        put(&alive, i);
    }
    for i in DOCS..DOCS + 50 {
        assert_eq!(get(&alive, i), Some(value(i)), "document {i}");
    }
    n2.stop();
    n3.stop();
}
