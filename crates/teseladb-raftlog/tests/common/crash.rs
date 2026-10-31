//! Random workload of the crash tests: writes Raft entries, takes snapshots and crashes the
//! simulated disk at a random operation.

use std::collections::HashMap;

use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::{CoreStore, LogEntry, SimDisk};
use teseladb_types::{Bytes, Durability, GroupId, Nanos, Rng};

use super::*;

/// Acknowledged Raft entries: (group, index) → data.
pub type Entries = HashMap<(u64, u64), Bytes>;
pub const GROUPS: u64 = 4;

/// Runs a random workload and crashes at a random point, recording the acknowledged entries.
pub fn run_until_crash(seed: u64, disk: &mut SimDisk, entries: &mut Entries) {
    run_until_crash_traced(seed, disk, entries, &mut Vec::new());
}

/// Like `run_until_crash`, recording in `trace` (life, event) every operation and
/// acknowledgement.
pub fn run_until_crash_traced(
    seed: u64,
    disk: &mut SimDisk,
    confirmed_entries: &mut Entries,
    trace: &mut Vec<(u64, String)>,
) {
    let cfg = small_cfg();
    let mut rng = Rng::new(seed);
    let mut s = CoreStore::recover(&cfg, 0, disk).expect("recovery without corruption").0;
    let mut now = Nanos::ZERO;
    let mut in_flight_entries: HashMap<teseladb_raftlog::Ticket, Vec<((u64, u64), Bytes)>> =
        HashMap::new();
    for g in 0..GROUPS {
        if s.group(GroupId(g)).is_none() {
            s.create_group(GroupId(g), Bytes::new(), Durability::Strict).unwrap();
        }
    }
    let ops_before_crash = rng.range(1, 400);
    let mut ops = 0;
    loop {
        // Queue some work.
        if rng.chance(0.8) {
            let g = rng.range(0, GROUPS);
            let first = s.group(GroupId(g)).unwrap().last_index() + 1;
            let n = rng.range(1, 4);
            let new: Vec<LogEntry> = (0..n)
                .map(|i| {
                    let len =
                        if rng.chance(0.1) { rng.range(10_000, 40_000) } else { rng.range(1, 300) };
                    let mut d = vec![0u8; len as usize];
                    rng.fill(&mut d);
                    LogEntry {
                        index: first + i,
                        term: 1,
                        kind: EntryKind::Normal,
                        data: Bytes::from_vec(d),
                    }
                })
                .collect();
            let t = s.append_raft(GroupId(g), &new, None, Durability::Strict).unwrap();
            in_flight_entries
                .insert(t, new.iter().map(|e| ((g, e.index), e.data.clone())).collect());
        }
        if rng.chance(0.08) {
            // Snapshot of a group up to its last index: releases WAL.
            let g = GroupId(rng.range(0, GROUPS));
            let log = s.group(g).unwrap();
            let (last, snap) = (log.last_index(), log.snapshot_index);
            if last > snap {
                let meta =
                    teseladb_raftlog::SnapshotMeta { index: last, term: 1, conf: Bytes::new() };
                // It may be rejected if one with that index is already in progress.
                let _ = s.save_snapshot(g, meta, Bytes::from_vec(vec![7; 1000]));
            }
        }
        // Run a few disk operations, or crash.
        for _ in 0..rng.range(1, 6) {
            let Some(op) = s.poll_io(now) else { break };
            ops += 1;
            if ops >= ops_before_crash {
                trace.push((seed, format!("crash before {op:?}").chars().take(160).collect()));
                disk.crash(&mut rng);
                return;
            }
            let opname = format!("{:?}", op.without_data()).chars().take(120).collect::<String>();
            trace.push((seed, opname));
            let done = disk.execute(op);
            let c = s.on_io_done(done, now);
            for (t, r) in c.tickets {
                r.expect("no I/O errors");
                for (k, data) in in_flight_entries.remove(&t).unwrap_or_default() {
                    trace.push((seed, format!("acknowledged entry {k:?} ({} bytes)", data.len())));
                    confirmed_entries.insert(k, data);
                }
            }
        }
        now = now + Nanos::from_micros(10);
    }
}
