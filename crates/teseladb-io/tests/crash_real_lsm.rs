//! Real crashes of the log and the LSM together (004 T030, second pass; FR-010, FR-011). A child
//! process writes entries to a group's log with `FileDisk`, applies each durable one to the LSM,
//! declares the LSM's release floor, takes snapshots (refused past the floor) and prints every
//! applied entry; the parent kills it with `SIGKILL` at an arbitrary point, recovers both,
//! replays the log after the LSM's flushed index and checks every printed entry.
//!
//! Without the test harness: the same binary is the parent and, with `TESELADB_CHILD_DIR`, the
//! child. `TESELADB_KILL_ROUNDS` (10 by default) sets the number of kills.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use teseladb_io::FileDisk;
use teseladb_keys::Hlc;
use teseladb_lsm::{Lookup, Lsm, LsmConfig};
use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::{CoreStore, LogEntry, ReleaseProvider, SnapshotMeta, StorageConfig};
use teseladb_types::{Bytes, Durability, GroupId, Nanos, Rng};

const G: GroupId = GroupId(1);
const KEYS: u64 = 500;
/// A snapshot attempt every this many entries.
const SNAPSHOT_EVERY: u64 = 50;

fn store_cfg() -> StorageConfig {
    StorageConfig {
        wal_segment_size: 1 << 20,
        max_batch_bytes: 64 << 10,
        ..StorageConfig::default()
    }
}

fn lsm_cfg() -> LsmConfig {
    LsmConfig {
        dir: "core-00/lsm".into(),
        memtable_bytes: 64 << 10,
        block_bytes: 4096,
        target_file_bytes: 256 << 10,
        l1_bytes: 1 << 20,
        compaction_bytes_per_sec: 1 << 40,
        ..LsmConfig::default()
    }
}

/// The value of entry `index`: it depends only on the index.
fn value(index: u64) -> Vec<u8> {
    let mut rng = Rng::new(index);
    let mut v = vec![0u8; rng.range(1, 3000) as usize];
    rng.fill(&mut v);
    v
}

fn key(index: u64) -> Vec<u8> {
    format!("k{:04}", index % KEYS).into_bytes()
}

fn at(index: u64) -> Hlc {
    Hlc { physical: index, logical: 0 }
}

/// Child process: writes, applies and prints "ok <index>" until the parent kills it.
fn child(dir: &str) {
    let mut disk = FileDisk::open(dir).unwrap();
    let mut s = CoreStore::recover(&store_cfg(), 0, &mut disk).unwrap().0;
    let (mut lsm, flushed) = Lsm::recover(&lsm_cfg(), &mut disk).unwrap();
    if s.group(G).is_none() {
        s.create_group(G, Bytes::new(), Durability::Strict).unwrap();
        s.run_io(&mut disk, Nanos::ZERO);
    }
    // The log after the flushed index goes back into the LSM before anything else.
    let last = s.group(G).unwrap().last_index();
    let from = flushed.get(&G).copied().unwrap_or(0) + 1;
    for e in s.entries(G, from, last + 1, u64::MAX).unwrap() {
        lsm.put(G, e.index, &key(e.index), at(e.index), &e.data).unwrap();
    }
    s.set_release_floor(G, ReleaseProvider::Lsm, lsm.release_floor(G));
    let out = std::io::stdout();
    let mut out = out.lock();
    let mut index = last + 1;
    loop {
        let entry = LogEntry {
            index,
            term: 1,
            kind: EntryKind::Normal,
            data: Bytes::from_vec(value(index)),
        };
        s.append_raft(G, &[entry], None, Durability::Strict).unwrap();
        for (_, r) in s.run_io(&mut disk, Nanos::ZERO).tickets {
            r.unwrap();
        }
        lsm.put(G, index, &key(index), at(index), &value(index)).unwrap();
        lsm.run_io(&mut disk, Nanos::ZERO);
        s.set_release_floor(G, ReleaseProvider::Lsm, lsm.release_floor(G));
        if index.is_multiple_of(SNAPSHOT_EVERY) {
            // Refused while the LSM still holds entries up to `index`; an accepted one never
            // passes what the LSM has flushed.
            let meta = SnapshotMeta { index, term: 1, conf: Bytes::new() };
            if s.save_snapshot(G, meta, Bytes::new()).is_ok() && lsm.flushed_index(G) < index {
                let (floor, flushed) = (lsm.release_floor(G), lsm.flushed_index(G));
                writeln!(out, "bad snapshot {index} floor {floor:?} flushed {flushed}").unwrap();
            }
        }
        writeln!(out, "ok {index}").unwrap();
        out.flush().unwrap();
        index += 1;
    }
}

fn main() {
    match std::env::var("TESELADB_CHILD_DIR") {
        Ok(dir) => child(&dir),
        Err(_) => {
            sigkill_never_loses_applied_entries();
            println!("crash_real_lsm: ok");
        }
    }
}

fn read(lsm: &mut Lsm, disk: &mut FileDisk, index: u64) -> Option<Vec<u8>> {
    let v = lsm.view();
    loop {
        match v.get(lsm, &key(index), at(index)) {
            Lookup::Hit(f) => return Some(f.value.to_vec()),
            Lookup::Miss(_) => {
                lsm.run_io(disk, Nanos::ZERO);
            }
            other => panic!("entry {index}: {other:?}"),
        }
    }
}

fn sigkill_never_loses_applied_entries() {
    let dir = std::env::temp_dir().join(format!("teseladb-crash-real-lsm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut applied: Vec<u64> = Vec::new();
    let mut rng = Rng::new(43);
    let rounds: u64 =
        std::env::var("TESELADB_KILL_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(10);
    for round in 0..rounds {
        let mut c = Command::new(std::env::current_exe().unwrap())
            .env("TESELADB_CHILD_DIR", &dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = c.stdout.take().unwrap();
        let wanted = rng.range(100, 600);
        let mut seen = 0;
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            assert!(!line.starts_with("bad "), "round {round}: {line}");
            if let Some(i) = line.strip_prefix("ok ") {
                applied.push(i.parse().unwrap());
                seen += 1;
                if seen >= wanted {
                    break;
                }
            }
        }
        c.kill().unwrap(); // SIGKILL
        c.wait().unwrap();
        let mut disk = FileDisk::open(&dir).unwrap();
        let s = CoreStore::recover(&store_cfg(), 0, &mut disk).unwrap().0;
        let (mut lsm, flushed) =
            Lsm::recover(&lsm_cfg(), &mut disk).unwrap_or_else(|e| panic!("round {round}: {e}"));
        let last = s.group(G).unwrap().last_index();
        let from = flushed.get(&G).copied().unwrap_or(0) + 1;
        let replay = s.entries(G, from, last + 1, u64::MAX).unwrap_or_else(|e| {
            panic!(
                "round {round}: the log released entries from {from} the LSM had not flushed: {e:?}"
            )
        });
        for e in replay {
            lsm.put(G, e.index, &key(e.index), at(e.index), &e.data).unwrap();
        }
        for &i in &applied {
            assert_eq!(read(&mut lsm, &mut disk, i), Some(value(i)), "round {round}, entry {i}");
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
