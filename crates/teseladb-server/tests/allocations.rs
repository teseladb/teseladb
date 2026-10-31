//! Allocations of the hot path (012 FR-008, T011): the operations of the `reference` scenario (an
//! `insert` confirmed by the majority with the journal and a `find` by `_id` served with the lease)
//! make a bounded number of allocations each, counted by a test allocator over the whole process
//! (client, workers, cores, disk threads), and that number does not grow with the operations run.
//! The work the node does on its own as time passes (ticks, heartbeats, renewing the bound on disk)
//! allocates too, at a rate that does not depend on the operations: it is measured with the node
//! idle and taken out, so the count does not grow with how slow the host is: about 11 000 a second
//! (31-10), 7 per operation on the host and some 150 under x86_64 emulation, where the same
//! operations take many times longer.

mod common;

use std::time::{Duration, Instant};

use common::{Client, READY, batch, docs, ok, until};
use teseladb_bson::Document;
use teseladb_raftlog::MemDisk;
use teseladb_server::node::Node;
use teseladb_sys::alloc_count::{CountingAlloc, allocations};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// Most allocations an operation of `reference` may make, over the whole process and without the
/// work the node does on its own: three runs of this test measured 447 to 461 with that work
/// included (31-10, 01:30), and 484 to 486 without it later that day. Of that rise, 32 were two
/// defects fixed the same day: every reply stamped its cluster time twice (the second time,
/// after the command, rebuilt the fields) and every read with a read concern asked the core for
/// the catalog to learn when its database was added, which only `atClusterTime` needs; 452 to
/// 455 after them. The rest is room for runs that differ.
const MAX_PER_OP: u64 = 600;

/// How long the node stays idle to measure the allocations of its own work.
const IDLE: Duration = Duration::from_secs(2);

/// Allocations per second of the node's own work, with no operation running.
fn idle_rate() -> f64 {
    let before = allocations();
    let start = Instant::now();
    std::thread::sleep(IDLE);
    (allocations() - before) as f64 / start.elapsed().as_secs_f64()
}

/// Runs `n` pairs of a write and a read of its document; the allocations per operation.
fn per_op(c: &mut Client, first: i32, n: i32, idle: f64) -> u64 {
    let before = allocations();
    let start = Instant::now();
    for id in first..first + n {
        let wc = Document::new().with("w", "majority").with("j", true);
        let r = c.run(
            "reference",
            Document::new()
                .with("insert", "docs")
                .with("documents", docs(id..id + 1))
                .with("writeConcern", wc),
        );
        assert!(ok(&r), "{r:?}");
        let rc = Document::new().with("level", "local");
        let r = c.run(
            "reference",
            Document::new()
                .with("find", "docs")
                .with("filter", Document::new().with("_id", id))
                .with("limit", 1)
                .with("readConcern", rc),
        );
        assert_eq!(batch(&r, "firstBatch").len(), 1, "{r:?}");
    }
    let own_work = (idle * start.elapsed().as_secs_f64()) as u64;
    (allocations() - before).saturating_sub(own_work) / (2 * n as u64)
}

#[test]
fn an_operation_of_the_reference_scenario_makes_a_bounded_number_of_allocations() {
    let node = Node::start(common::config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    until("the node to be ready", || node.wait_ready(READY).then_some(()));
    let mut c = Client::new(&node);
    per_op(&mut c, 0, 200, 0.0);
    let idle = idle_rate();
    let short = per_op(&mut c, 1_000, 200, idle);
    let long = per_op(&mut c, 10_000, 1_000, idle);
    assert!(short > 0 && long > 0, "the allocator counts ({short}, {long})");
    assert!(long <= MAX_PER_OP, "{long} allocations per operation");
    assert!(long <= short + short / 4 + 10, "it grows with the operations: {short} then {long}");
}
