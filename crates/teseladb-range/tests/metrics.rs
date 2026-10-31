//! The metrics of a range (005 FR-022, T027): applied index, proposal latency, reads with the lease
//! and with ReadIndex, latch waits counted once per request, and the node's clock offset.

mod common;

use common::*;

fn metrics(c: &Cluster, n: NodeId) -> teseladb_range::metrics::RangeMetrics {
    let node = &c.nodes[&n];
    node.host.metrics(&node.core, G, c.now).expect("the host has the range")
}

#[test]
fn a_range_publishes_its_applied_index_latency_reads_waits_and_clock_offset() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let base = metrics(&c, l);
    assert_eq!(base.propose_latency.count(), 0);
    assert_eq!(base.latch_waits, 0);
    // Two writes: each records how long its proposal took.
    for v in [1, 2] {
        let w = c.mutate(l, KEY, v, MAJORITY);
        c.ticks(2);
        assert!(matches!(c.reply(l, w), Some(Ok(_))));
    }
    let m = metrics(&c, l);
    assert_eq!(m.propose_latency.count(), 2);
    assert_eq!(m.applied_index, c.nodes[&l].core.status(G).unwrap().applied);
    assert!(m.applied_index > base.applied_index);
    // With the lease of a few heartbeat rounds, `majority` goes through it; `linearizable` always
    // through ReadIndex.
    c.ticks(20);
    let base = metrics(&c, l);
    let r = c.get(l, KEY, ReadConcern::Majority);
    c.settle();
    assert_eq!(value(c.reply(l, r)), Some(2));
    let r = c.get(l, KEY, ReadConcern::Linearizable);
    c.ticks(2);
    assert_eq!(value(c.reply(l, r)), Some(2));
    let m = metrics(&c, l);
    assert_eq!(
        (m.lease_reads - base.lease_reads, m.read_index_reads - base.read_index_reads),
        (1, 1)
    );
    // The clocks agree: the offset is known and small.
    assert!(m.clock_offset_ms.is_some_and(|o| o.abs() < 50), "{:?}", m.clock_offset_ms);
    // A read behind a write in flight waits; however many polls it takes, it counts once.
    let followers = c.others(l);
    c.slow.extend(followers);
    let w = c.mutate(l, KEY, 3, MAJORITY);
    c.settle();
    let r = c.get(l, KEY, ReadConcern::Majority);
    for _ in 0..5 {
        c.settle();
    }
    assert_eq!(c.reply(l, r), None);
    assert_eq!(metrics(&c, l).latch_waits, 1);
    c.slow.clear();
    c.ticks(5);
    assert!(matches!(c.reply(l, w), Some(Ok(_))));
    assert_eq!(value(c.reply(l, r)), Some(3));
    // A follower publishes its applied index too; it proposes nothing.
    let f = c.others(l)[0];
    let fm = metrics(&c, f);
    assert_eq!(fm.applied_index, c.nodes[&f].core.status(G).unwrap().applied);
    assert_eq!(fm.propose_latency.count(), 0);
    // A range the host does not have has no metrics.
    let node = &c.nodes[&l];
    assert!(node.host.metrics(&node.core, GroupId(99), c.now).is_none());
}

#[test]
fn a_write_waiting_for_another_writes_latch_counts_once() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let followers = c.others(l);
    c.slow.extend(followers);
    let first = c.mutate(l, KEY, 1, MAJORITY);
    let second = c.mutate(l, KEY, 2, MAJORITY);
    for _ in 0..5 {
        c.settle();
    }
    assert_eq!((c.reply(l, first), c.reply(l, second)), (None, None));
    assert_eq!(metrics(&c, l).latch_waits, 1, "the second write waits for the first");
    c.slow.clear();
    c.ticks(10);
    assert!(matches!(c.reply(l, second), Some(Ok(_))));
    assert_eq!(metrics(&c, l).propose_latency.count(), 2);
}
