//! Write pressure and the compaction budget (004 story 3, T020, FR-012): with 20 files in level 0
//! the LSM asks to slow down and with 36 to reject; compactions bring it back; compactions spend
//! at most their budget of bytes per second.

mod common;

use common::{open, small, ts};
use teseladb_lsm::{LsmConfig, Pressure};
use teseladb_raftlog::MemDisk;
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);

#[test]
fn level_0_files_slow_down_then_reject_and_compaction_brings_it_back() {
    // Compactions only start after the test looks at the limits.
    let cfg = LsmConfig { l0_trigger: 1000, ..small() };
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    let mut seen = Vec::new();
    for i in 1..=40u64 {
        lsm.put(G, i, format!("k{i:03}").as_bytes(), ts(i, 0), b"v").unwrap();
        lsm.flush();
        lsm.run_io(&mut disk, Nanos::ZERO);
        seen.push((lsm.metrics().level_bytes.len(), lsm.pressure()));
        assert_eq!(lsm.write_debt(), i, "one file in level 0 per flush (005 FR-038)");
    }
    let at = |n: usize| seen[n - 1].1;
    assert_eq!(at(19), Pressure::Normal);
    assert_eq!(at(20), Pressure::Slow);
    assert_eq!(at(35), Pressure::Slow);
    assert_eq!(at(36), Pressure::Reject);
    let m = lsm.metrics();
    assert_eq!((m.stalls_total, m.rejects_total), (16, 4), "writes made under each signal");
    // Writes the log confirmed still apply under Reject.
    lsm.put(G, 41, b"k041", ts(41, 0), b"v").unwrap();
    lsm.compact_all();
    lsm.run_io(&mut disk, Nanos::ZERO);
    assert_eq!(lsm.pressure(), Pressure::Normal);
}

#[test]
fn many_frozen_memtables_ask_to_slow_down_too() {
    let cfg = small();
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    for i in 1..=cfg.max_frozen as u64 * 2 {
        lsm.put(G, i, b"k", ts(i, 0), b"v").unwrap();
        lsm.flush();
        let frozen = i as usize;
        let expected = if frozen >= 2 * cfg.max_frozen {
            Pressure::Reject
        } else if frozen >= cfg.max_frozen {
            Pressure::Slow
        } else {
            Pressure::Normal
        };
        assert_eq!(lsm.pressure(), expected, "{frozen} frozen");
        assert_eq!(lsm.write_debt(), i, "frozen memtables are debt too");
    }
    lsm.run_io(&mut disk, Nanos::ZERO);
    assert_eq!(lsm.pressure(), Pressure::Normal);
}

#[test]
fn compactions_spend_at_most_their_budget_per_second() {
    let rate = 64 << 10;
    let cfg = LsmConfig { compaction_bytes_per_sec: rate, io_chunk_bytes: 8 << 10, ..small() };
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    let mut index = 0;
    for round in 0..4u64 {
        for k in 0..400u64 {
            index += 1;
            lsm.put(G, index, format!("k{k:04}").as_bytes(), ts(round + 1, 0), &[1; 100]).unwrap();
        }
        lsm.flush();
    }
    let flushed = lsm.run_io(&mut disk, Nanos::ZERO);
    assert!(!flushed.flushed.is_empty());
    // With time standing still, the compaction stops once its first second of budget is spent.
    assert_eq!(lsm.metrics().compactions_total, 0);
    let mut now = Nanos::ZERO;
    let mut seconds = 0;
    while lsm.metrics().compactions_total == 0 {
        now = now + Nanos::from_secs(1);
        seconds += 1;
        lsm.run_io(&mut disk, now);
        assert!(seconds < 100, "the compaction never ends");
    }
    // About 4 inputs of ~45 KiB read and ~180 KiB written at 64 KiB/s: several seconds.
    assert!(seconds >= 3, "{seconds} s");
}
