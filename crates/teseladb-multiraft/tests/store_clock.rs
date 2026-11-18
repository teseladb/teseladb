//! The log store's clock follows its core's at each turn: the wait of a batch from its first
//! append until its sync is handed out (`log_append_to_sync_out`, 012 FR-015) starts when the
//! entries were appended, in the turn that reads the clock, and not at the store's last poll,
//! one turn earlier (002 run p53b: the stage read 0.55 ms, the gap between two turns of a core,
//! where the real parts of a write's wait for its log added up to 1.3 of the 1.8 ms measured).

mod common;

use common::*;
use teseladb_stages::{Stage, count, total_nanos};
use teseladb_types::Nanos;

#[test]
fn a_batch_s_wait_for_its_sync_starts_in_its_own_turn() {
    let mut c = Cluster::new(1);
    let leader = c.elect();
    c.settle();
    // The core's next turn comes ten milliseconds later: it reads the clock, takes a write,
    // appends its entries and hands them to the disk at its end.
    let gap = Nanos::from_millis(10);
    c.now = c.now + gap;
    let now = c.now;
    c.nodes.get_mut(&leader).unwrap().core.on_clock(now, WALL0 + now.as_millis());
    let stage = Stage::LogAppendToSyncOut;
    let (waits, waited) = (count(stage), total_nanos(stage));
    c.propose(leader, b"x").unwrap();
    c.settle();
    assert!(count(stage) > waits, "no batch handed its sync out");
    let waited = total_nanos(stage) - waited;
    assert!(waited < gap.0, "the wait counted {waited} ns from before the batch's turn");
}
