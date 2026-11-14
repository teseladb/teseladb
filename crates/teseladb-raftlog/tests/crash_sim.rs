//! Crashes with `SimDisk` (lost writes and writes torn by sectors). What was
//! acknowledged always survives and recovery never mistakes a broken tail for corruption. A failing
//! seed is traced with `SEED=<n> cargo run -p teseladb-raftlog --example crash_trace`.
mod common;

use common::crash::{Entries, run_until_crash};
use common::*;
use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::{
    CoreStore, Disk, FileId, IoOp, LogEntry, SimDisk, SimDiskConfig, StorageError,
};
use teseladb_types::{Bytes, Durability, GroupId, Nanos};

#[test]
fn confirmed_entries_survive_random_crashes() {
    let env = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let start = env("TESELADB_RAFTLOG_CRASH_START", 0);
    let seeds = env("TESELADB_RAFTLOG_CRASH_SEEDS", 300);
    for seed in start..start + seeds {
        let mut disk =
            SimDisk::new(SimDiskConfig { torn_write_probability: 0.5, ..Default::default() });
        let mut entries = Entries::new();
        // Several lives on the same disk: write, crash, recover…
        for life in 0..3 {
            run_until_crash(seed * 10 + life, &mut disk, &mut entries);
            let cfg = small_cfg();
            let s = CoreStore::recover(&cfg, 0, &mut disk)
                .unwrap_or_else(|e| panic!("seed {seed}, life {life}: {e:?}"))
                .0;
            for ((g, index), data) in &entries {
                match s.entries(GroupId(*g), *index, index + 1, u64::MAX) {
                    // Covered by a snapshot: correct.
                    Err(teseladb_raftlog::LogGap::Compacted { first }) if *index < first => {}
                    got => assert_eq!(
                        got.ok().and_then(|v| v.first()).map(|e| &e.data),
                        Some(data),
                        "seed {seed}, life {life}, group {g}, index {index}"
                    ),
                }
            }
        }
    }
}

#[test]
fn flipped_byte_in_a_durable_wal_record_is_reported_as_corrupt() {
    let cfg = small_cfg();
    let mut disk = SimDisk::default();
    let mut s = CoreStore::recover(&cfg, 0, &mut disk).expect("recover succeeds").0;
    let mut now = Nanos::ZERO;
    let g = GroupId(1);
    s.create_group(g, Bytes::new(), Durability::Strict).expect("create_group succeeds");
    let payload: Vec<u8> = (0..500u32).map(|i| (i * 7 % 251) as u8).collect();
    let first = LogEntry {
        index: 1,
        term: 1,
        kind: EntryKind::Normal,
        data: Bytes::from_vec(payload.clone()),
    };
    s.append_raft(g, &[first], None, Durability::Strict).expect("append_raft succeeds");
    settle(&mut s, &mut disk, &mut now);
    // A second durable batch after it: the damaged one is not the tail, so it is not a torn write.
    let second = LogEntry { index: 2, term: 1, kind: EntryKind::Normal, data: Bytes::from("tail") };
    s.append_raft(g, &[second], None, Durability::Strict).expect("append_raft succeeds");
    settle(&mut s, &mut disk, &mut now);
    let seg = disk.list(&teseladb_raftlog::paths::wal_dir(0)).expect("list succeeds")[0].clone();
    let data = disk.read_all(&seg).expect("read_all succeeds");
    let pos = data.windows(16).position(|w| w == &payload[100..116]).expect("position succeeds");
    disk.execute(IoOp::Open { file: FileId(9999), path: seg.clone() });
    let mut b = data[pos..=pos].to_vec();
    b[0] ^= 0xff;
    disk.execute(IoOp::WriteAt {
        file: FileId(9999),
        offset: pos as u64,
        data: Bytes::from_vec(b),
        batch: 0,
    });
    match CoreStore::recover(&cfg, 0, &mut disk) {
        Err(StorageError::Corrupt { file, .. }) => assert_eq!(file, seg),
        other => panic!("the damaged batch must be reported, got {:?}", other.map(|(_, r)| r)),
    }
}
