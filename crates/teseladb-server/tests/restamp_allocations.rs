//! Moving a reply's cluster time to the time after its command (008 FR-013) happens on every
//! reply of the hot path, so it allocates nothing (012 FR-008): it changes the values that were
//! stamped before the command in place. Counted by the engine's allocator on a shard (023
//! FR-030).
#![cfg(target_os = "linux")]

use teseladb_auth::clustertime::{HmacKey, KEY_LEN, KeyRing};
use teseladb_bson::Document;
use teseladb_io::engine::stats::allocations_here;
use teseladb_io::engine::{DirectIo, EngineConfig, NodeEngine};
use teseladb_wire::dispatch::{ClusterTime, NodeInfo, Topology};
use teseladb_wire::hello::{restamp_cluster_time, with_cluster_time};

const BEFORE: ClusterTime = ClusterTime { time: 1_793_376_615, increment: 1 };

/// The allocations of 100 moves of a reply's cluster time, with `keys` or without, and whether
/// every move found the fields to change.
fn restamps(keys: Option<KeyRing>) -> (u64, bool) {
    let node = NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: BEFORE,
        now_ms: 0,
        keys: keys.clone().map(std::sync::Arc::new),
        replica_set: None,
    };
    let mut reply = with_cluster_time(Document::new().with("ok", 1.0), &node);
    let mut moved = true;
    let start = allocations_here();
    for i in 1..=100 {
        let later = ClusterTime { time: BEFORE.time, increment: BEFORE.increment + i };
        moved &= restamp_cluster_time(&mut reply, later, keys.as_ref());
    }
    (allocations_here() - start, moved)
}

#[test]
fn moving_a_reply_to_a_later_cluster_time_allocates_nothing() {
    let ring = KeyRing {
        keys: vec![HmacKey { id: 7, key: [3; KEY_LEN], expires_at: BEFORE.time + 1_000 }],
    };
    let engine = NodeEngine::start(&EngineConfig {
        shards: 1,
        memory: Some(256 << 20),
        direct_io: DirectIo::Off,
        ..EngineConfig::default()
    })
    .unwrap();
    // The engine's allocator counts on its shard: an allocation there is seen.
    let counted = engine.run_on(0, || {
        let start = allocations_here();
        drop(std::hint::black_box(vec![1u8; 64]));
        allocations_here() - start
    });
    let plain = engine.run_on(0, || restamps(None));
    let signed = engine.run_on(0, move || restamps(Some(ring)));
    engine.stop();
    assert!(counted.unwrap() >= 1, "the shard's allocator counts");
    assert_eq!(plain.unwrap(), (0, true));
    assert_eq!(signed.unwrap(), (0, true));
}
