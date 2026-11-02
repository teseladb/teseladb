//! Real crash (001 FR-004, 012 FR-004). A child process appends Raft entries with each disk
//! executor (the disk thread's `FileDisk` and, on Linux, io_uring, with `O_DIRECT` where the file
//! system takes it) and prints every acknowledgement; the parent kills it with `SIGKILL` at an arbitrary point, recovers the
//! directory and checks that every acknowledged entry is there and correct.
//!
//! Without the test harness: the same binary is the parent and, with `TESELADB_CHILD_DIR`, the
//! child. `TESELADB_KILL_ROUNDS` (10 by default) sets the number of kills.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::{CoreStore, LogEntry, LogGap, SnapshotMeta, StorageConfig};
use teseladb_types::{Bytes, Durability, GroupId, Nanos, Rng};

const G: GroupId = GroupId(1);
/// A snapshot every this many entries, so that WAL segments are released while writing.
const SNAPSHOT_EVERY: u64 = 100;

fn cfg() -> StorageConfig {
    StorageConfig {
        wal_segment_size: 1 << 20,
        max_batch_bytes: 64 << 10,
        ..StorageConfig::default()
    }
}

/// Entry `index`: its content depends only on the index, so every life writes the same one.
fn entry(index: u64) -> LogEntry {
    let mut rng = Rng::new(index);
    let mut data = vec![0u8; rng.range(1, 8000) as usize];
    rng.fill(&mut data);
    LogEntry { index, term: 1, kind: EntryKind::Normal, data: Bytes::from_vec(data) }
}

/// Child process: appends non-stop after the last durable entry and prints "ok <index>" for every
/// acknowledged one.
fn child(dir: &str) {
    let mut disk = disks::open(dir, disks::from_env());
    let mut s = CoreStore::recover(&cfg(), 0, &mut disk).unwrap().0;
    if s.group(G).is_none() {
        s.create_group(G, Bytes::new(), Durability::Strict).unwrap();
    }
    let out = std::io::stdout();
    let mut out = out.lock();
    // Until the parent kills it.
    let mut index = s.group(G).unwrap().last_index() + 1;
    loop {
        s.append_raft(G, &[entry(index)], None, Durability::Strict).unwrap();
        if index.is_multiple_of(SNAPSHOT_EVERY) {
            let meta = SnapshotMeta { index, term: 1, conf: Bytes::new() };
            s.save_snapshot(G, meta, Bytes::from("state")).unwrap();
        }
        let c = s.run_io(&mut disk, Nanos::ZERO);
        for (_, r) in c.tickets {
            r.unwrap();
        }
        writeln!(out, "ok {index}").unwrap();
        out.flush().unwrap();
        index += 1;
    }
}

#[path = "common/disks.rs"]
mod disks;

fn main() {
    match std::env::var("TESELADB_CHILD_DIR") {
        Ok(dir) => child(&dir),
        Err(_) => {
            for (name, exec) in disks::executors() {
                sigkill_never_loses_confirmed_entries(name, exec);
            }
            println!("crash_real: ok");
        }
    }
}

fn sigkill_never_loses_confirmed_entries(name: &str, exec: teseladb_io::node_disk::IoExecutor) {
    let dir =
        std::env::temp_dir().join(format!("teseladb-crash-real-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut confirmed: Vec<u64> = Vec::new();
    let mut rng = Rng::new(42);
    let rounds: u64 =
        std::env::var("TESELADB_KILL_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(10);
    for round in 0..rounds {
        let mut c = Command::new(std::env::current_exe().unwrap())
            .env("TESELADB_CHILD_DIR", &dir)
            .env(disks::EXECUTOR_VAR, name)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = c.stdout.take().unwrap();
        let wanted = rng.range(50, 400);
        let mut seen = 0;
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if let Some(i) = line.strip_prefix("ok ") {
                confirmed.push(i.parse().unwrap());
                seen += 1;
                if seen >= wanted {
                    break;
                }
            }
        }
        c.kill().unwrap(); // SIGKILL
        c.wait().unwrap();
        let mut disk = disks::open(&dir, exec);
        let s = CoreStore::recover(&cfg(), 0, &mut disk)
            .unwrap_or_else(|e| panic!("{name}, round {round}: {e:?}"))
            .0;
        for &i in &confirmed {
            match s.entries(G, i, i + 1, u64::MAX) {
                // Covered by a durable snapshot: released from the WAL, which is correct.
                Err(LogGap::Compacted { first }) if i < first => {}
                got => assert_eq!(
                    got.ok().and_then(|v| v.first().cloned()),
                    Some(entry(i)),
                    "round {round}, entry {i}"
                ),
            }
        }
        assert!(s.group(G).unwrap().last_index() >= confirmed.last().copied().unwrap_or(0));
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
