//! Traces a seed of `tests/crash_sim.rs` that fails: the operations around a lost entry, or the
//! state of the WAL files when recovery refuses the disk.
//! `SEED=<n> cargo run -p teseladb-raftlog --example crash_trace`

#[path = "../tests/common/mod.rs"]
mod common;

use common::crash::{Entries, run_until_crash_traced};
use common::small_cfg;
use teseladb_raftlog::{CoreStore, Disk, LogGap, SimDisk, SimDiskConfig};
use teseladb_types::GroupId;

fn main() {
    let seed: u64 = std::env::var("SEED").expect("SEED=<n>").parse().expect("a number");
    let mut disk =
        SimDisk::new(SimDiskConfig { torn_write_probability: 0.5, ..Default::default() });
    let mut entries = Entries::new();
    let mut trace = Vec::new();
    for life in 0..3 {
        run_until_crash_traced(seed * 10 + life, &mut disk, &mut entries, &mut trace);
        let cfg = small_cfg();
        match CoreStore::recover(&cfg, 0, &mut disk) {
            Ok((s, _)) => {
                for ((g, index), data) in &entries {
                    let got = s.entries(GroupId(*g), *index, index + 1, u64::MAX);
                    let lost = match got {
                        Err(LogGap::Compacted { first }) if *index < first => false,
                        Ok(v) => v.first().map(|e| &e.data) != Some(data),
                        Err(_) => true,
                    };
                    if lost {
                        println!("life {life}: lost entry {index} of group {g}");
                        let key = format!("acknowledged entry ({g}, {index})");
                        let pos = trace.iter().position(|t| t.1.starts_with(&key)).unwrap_or(0);
                        for t in &trace[pos.saturating_sub(30)..(pos + 40).min(trace.len())] {
                            println!("   [{}] {}", t.0 % 10, t.1);
                        }
                        return;
                    }
                }
            }
            Err(e) => {
                println!("life {life}: {e:?}");
                for p in disk.list(&teseladb_raftlog::paths::wal_dir(0)).expect("the WAL directory")
                {
                    let d = disk.read_all(&p).expect("a WAL segment");
                    let used = d
                        .chunks(4096)
                        .rposition(|b| b.iter().any(|&x| x != 0))
                        .map_or(0, |i| i + 1);
                    println!(
                        "{} len {} used blocks {used} first {:02x?}",
                        p.display(),
                        d.len(),
                        &d[..8.min(d.len())]
                    );
                }
                for t in trace.iter().rev().take(60).rev() {
                    println!("   [{}] {}", t.0 % 10, t.1);
                }
                return;
            }
        }
    }
    println!("seed {seed}: no loss and every recovery works");
}
