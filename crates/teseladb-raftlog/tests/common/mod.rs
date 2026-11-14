//! Utilities of the engine's integration tests.
#![allow(dead_code, missing_docs)]

pub mod crash;

use teseladb_raftlog::{Completions, CoreStore, Disk, StorageConfig, Ticket};
use teseladb_types::Nanos;

/// Small configuration so that the tests rotate WAL segments and split batches.
pub fn small_cfg() -> StorageConfig {
    StorageConfig {
        wal_segment_size: 1 << 20,
        max_batch_bytes: 32 << 10,
        ..StorageConfig::default()
    }
}

pub fn recover(cfg: &StorageConfig, disk: &mut dyn Disk) -> CoreStore {
    CoreStore::recover(cfg, 0, disk).expect("recovery").0
}

/// Runs the I/O until nothing is left, advancing the clock if there are deadlines.
pub fn settle(s: &mut CoreStore, disk: &mut dyn Disk, now: &mut Nanos) -> Completions {
    let mut all = Completions::default();
    for _ in 0..1000 {
        let c = s.run_io(disk, *now);
        all.tickets.extend(c.tickets);
        all.snapshots.extend(c.snapshots);
        match s.next_deadline() {
            Some(d) if d > *now => *now = d,
            Some(_) => {}
            None => break,
        }
    }
    all
}

pub fn ok_tickets(c: &Completions) -> Vec<Ticket> {
    c.tickets.iter().filter(|(_, r)| r.is_ok()).map(|(t, _)| *t).collect()
}
