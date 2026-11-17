//! A flushed memtable is freed a few thousand nodes at a time by the core's I/O steps, not whole
//! by whoever drops its last view: a memtable of a few million keys took a shard 170 ms to free
//! at once (one-node load, lane o-stats). It counts as alive until its last node goes.
//!
//! One test in this binary: it reads the process-wide count of the frozen memtables alive.

mod common;

use common::{open, small, ts};
use teseladb_lsm::{LsmConfig, frozen_alive_bytes};
use teseladb_raftlog::MemDisk;
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);
const KEYS: u32 = 40_000;

#[test]
fn a_flushed_memtable_is_freed_in_steps_once_no_view_holds_it() {
    let cfg = LsmConfig { memtable_bytes: 64 << 20, ..small() };
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    for k in 0..KEYS {
        lsm.put(G, u64::from(k) + 1, format!("k{k:06}").as_bytes(), ts(1, 0), b"v").unwrap();
    }
    let view = lsm.view();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    // The version readers on other threads take no longer holds it either.
    lsm.publish();
    assert!(frozen_alive_bytes() > 0, "the view holds the flushed memtable");
    lsm.run_io(&mut disk, Nanos::ZERO);
    assert!(frozen_alive_bytes() > 0, "still held");
    drop(view);
    assert!(frozen_alive_bytes() > 0, "dropping the last view frees nothing at once");
    // One step frees a part of its nodes; the memtable goes with its last one.
    lsm.poll_io_step(Nanos::ZERO);
    assert!(frozen_alive_bytes() > 0, "one step does not free 40,000 keys");
    let mut steps = 1;
    while frozen_alive_bytes() > 0 {
        lsm.poll_io_step(Nanos::ZERO);
        steps += 1;
        assert!(steps < 1000, "the teardown never ended");
    }
    assert!(steps >= 10, "{steps} steps for {KEYS} keys");
}
