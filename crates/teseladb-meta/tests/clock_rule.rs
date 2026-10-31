//! The clock rule of 005 (T005, FR-006; D-004): a node more than 125 ms from the median of the
//! voters cannot lead, as Group 0 reports it every 500 ms; and each node, comparing its clock
//! with the Raft heartbeats of its peers in the last second, stops leading and assigning marks by
//! itself, without waiting for Group 0. A node 400 ms off is set aside in less than a second; one
//! 100 ms off keeps leading.

use teseladb_meta::clock::{ClockSample, MAX_CLOCK_OFFSET_MS, PeerClocks, may_lead, offsets};
use teseladb_meta::liveness::NODE_HEARTBEAT;
use teseladb_types::{Nanos, NodeId};

fn sample(node: u64, skew: i64) -> ClockSample {
    ClockSample { node: NodeId(node), sent_ms: 10_000 + skew, received_ms: 10_000, rtt_ms: 0 }
}

#[test]
fn group_0_sets_aside_a_node_400_ms_off_and_keeps_one_100_ms_off() {
    assert_eq!(MAX_CLOCK_OFFSET_MS, 125);
    assert_eq!(NODE_HEARTBEAT, Nanos::from_millis(500), "clock reports every 500 ms");
    let voters = [NodeId(1), NodeId(2), NodeId(3)];
    let got = offsets(NodeId(1), &voters, &[sample(2, 400), sample(3, 100), sample(4, -130)]);
    let lead: Vec<(NodeId, bool)> = got.iter().map(|(n, o)| (*n, may_lead(*o))).collect();
    assert_eq!(
        lead,
        vec![(NodeId(1), true), (NodeId(2), false), (NodeId(3), true), (NodeId(4), false)]
    );
}

#[test]
fn a_node_sets_itself_aside_from_its_peers_heartbeats_within_a_second() {
    let mut peers = PeerClocks::default();
    let mut now = Nanos::from_secs(100);
    assert!(peers.in_margin(now), "without samples nothing is known against the node");
    // The node's clock runs 400 ms ahead of its two peers' heartbeats: within one heartbeat
    // interval of 150 ms it knows.
    for step in 0..3 {
        now = now + Nanos::from_millis(50);
        // Its own clock minus each peer's clock in the heartbeat (half the round trip discounted).
        peers.observe(NodeId(2), 400, now);
        peers.observe(NodeId(3), 410, now);
        if step == 0 {
            assert!(!peers.in_margin(now));
        }
    }
    assert_eq!(peers.median(now), Some(400));
    // Samples older than a second stop counting.
    let later = now + Nanos::from_millis(1_001);
    assert!(peers.in_margin(later) && peers.median(later).is_none());
    // 100 ms off: it keeps leading.
    peers.observe(NodeId(2), 100, later);
    peers.observe(NodeId(3), -90, later);
    assert!(peers.in_margin(later));
    // A peer's newer sample replaces its older one.
    peers.observe(NodeId(3), 300, later);
    peers.observe(NodeId(2), 300, later);
    assert!(!peers.in_margin(later));
}
