//! Moving a reply's cluster time to the time after its command (008 FR-013) happens on every
//! reply of the hot path, so it allocates nothing (012 FR-008): it changes the values that were
//! stamped before the command in place.

use teseladb_auth::clustertime::{HmacKey, KEY_LEN, KeyRing};
use teseladb_bson::Document;
use teseladb_sys::alloc_count::{CountingAlloc, allocations};
use teseladb_wire::dispatch::{ClusterTime, NodeInfo, Topology};
use teseladb_wire::hello::{restamp_cluster_time, with_cluster_time};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const BEFORE: ClusterTime = ClusterTime { time: 1_793_376_615, increment: 1 };

#[test]
fn moving_a_reply_to_a_later_cluster_time_allocates_nothing() {
    let ring = KeyRing {
        keys: vec![HmacKey { id: 7, key: [3; KEY_LEN], expires_at: BEFORE.time + 1_000 }],
    };
    for keys in [None, Some(ring)] {
        let node = NodeInfo {
            topology: Topology { process_id: [1; 12], counter: 0 },
            cluster_time: BEFORE,
            now_ms: 0,
            keys: keys.clone().map(std::sync::Arc::new),
        };
        let mut reply = with_cluster_time(Document::new().with("ok", 1.0), &node);
        let start = allocations();
        for i in 1..=100 {
            let later = ClusterTime { time: BEFORE.time, increment: BEFORE.increment + i };
            assert!(restamp_cluster_time(&mut reply, later, keys.as_ref()));
        }
        assert_eq!(allocations() - start, 0, "{reply:?}");
    }
}
