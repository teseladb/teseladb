//! Real crash (001 FR-004, 012 FR-004, 023 FR-010). A child process appends Raft entries through the
//! engine's disk executor (with `O_DIRECT` where the file system takes it, and with the kernel's
//! page cache) and prints every acknowledgement; the parent kills it with `SIGKILL` at an arbitrary
//! point, recovers the directory and checks that every acknowledged entry is there and correct.
//!
//! In the `pipelined` mode (012 FR-014, D-159) the child hands its operations to the executor with
//! several batches in flight, whose syncs end while the writes of the next batches run.
//!
//! In the `shared` mode (012 FR-035, D-154, 023 FR-013) four cores write at once through the node's
//! device log, whose writer runs on shard 0, each core on a shard of its own, and the parent checks
//! every core's entries after each kill.
//!
//! Linux only (the engine). Without the test harness: the same binary is the parent and, with
//! `TESELADB_CHILD_DIR`, the child; each process runs one engine. `TESELADB_KILL_ROUNDS` (10 by
//! default) sets the number of kills.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use teseladb_io::device_log::{DeviceLog, DeviceLogConfig, LogDisk};
use teseladb_io::files::BlockingDisk;
use teseladb_io::{DirectIo, IoClass};
use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::{CoreStore, LogEntry, LogGap, SnapshotMeta, StorageConfig};
use teseladb_types::{Bytes, Durability, GroupId, Nanos, Rng};

const G: GroupId = GroupId(1);
/// A snapshot every this many entries, so that WAL segments are released while writing.
const SNAPSHOT_EVERY: u64 = 100;
/// Variable that tells the child to write through the disk thread with batches in flight.
const MODE_VAR: &str = "TESELADB_CHILD_MODE";
const PIPELINED: &str = "pipelined";
const SHARED: &str = "shared";
/// Cores writing to the device log at once in the `shared` mode.
const SHARED_CORES: u16 = 4;
/// Batches in flight in the `pipelined` mode, as the server has them.
const INFLIGHT: usize = 4;
/// Entries the `pipelined` child keeps waiting for their acknowledgement.
const OUTSTANDING: usize = 8;

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
/// acknowledged one. Runs in an engine thread.
fn child(dir: &str) {
    let mut disk = disks::files(dir);
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

/// Child process of the `pipelined` mode: hands the store's operations to the executor, several
/// entries and batches at once, and prints "ok <index>" as each is acknowledged. Runs in an engine
/// thread.
fn child_pipelined(dir: &str) {
    let cfg = StorageConfig { max_inflight_batches: INFLIGHT, ..cfg() };
    let mut disk = disks::files(dir);
    let mut s = CoreStore::recover(&cfg, 0, &mut disk).unwrap().0;
    if s.group(G).is_none() {
        s.create_group(G, Bytes::new(), Durability::Strict).unwrap();
        s.run_io(&mut disk, Nanos::ZERO);
    }
    let out = std::io::stdout();
    let mut out = out.lock();
    let mut index = s.group(G).unwrap().last_index() + 1;
    let mut waiting = VecDeque::new();
    let mut done = Vec::new();
    // Until the parent kills it.
    loop {
        while waiting.len() < OUTSTANDING {
            let t = s.append_raft(G, &[entry(index)], None, Durability::Strict).unwrap();
            waiting.push_back((t, index));
            index += 1;
        }
        while let Some(op) = s.poll_io(Nanos::ZERO) {
            disk.submit(op, IoClass::Log);
        }
        // Waits in the engine thread for the next results.
        disk.ready().get().unwrap();
        disk.take_done(&mut done);
        for (d, _) in done.drain(..) {
            for (t, r) in s.on_io_done(d, Nanos::ZERO).tickets {
                r.unwrap();
                // Tickets end in the order they were taken.
                let (first, i) = waiting.pop_front().unwrap();
                assert_eq!(first, t);
                writeln!(out, "ok {i}").unwrap();
            }
        }
        out.flush().unwrap();
    }
}

/// Child process of the `shared` mode: every core appends on its own shard through the device
/// log, whose writer runs on shard 0, and prints "ok <core> <index>" for every acknowledged entry.
fn child_shared(e: &teseladb_io::engine::NodeEngine, dir: &str) -> DeviceLog {
    let disk = LogDisk::Files(PathBuf::from(dir));
    let (log, ports) = DeviceLog::start(
        e,
        disk,
        SHARED_CORES,
        DeviceLogConfig { file_bytes: 1 << 20 },
        Arc::default(),
    )
    .unwrap();
    for mut port in ports {
        let dir = dir.to_string();
        disks::spawn_on(e, port.core(), move || {
            let core = port.core();
            let mut own = disks::files(&dir);
            let mut disk = port.disk(&mut own);
            let mut s = CoreStore::recover(&cfg(), core, &mut disk).unwrap().0;
            if s.group(G).is_none() {
                s.create_group(G, Bytes::new(), Durability::Strict).unwrap();
            }
            let mut index = s.group(G).unwrap().last_index() + 1;
            loop {
                s.append_raft(G, &[entry(index)], None, Durability::Strict).unwrap();
                if index.is_multiple_of(SNAPSHOT_EVERY) {
                    let meta = SnapshotMeta { index, term: 1, conf: Bytes::new() };
                    s.save_snapshot(G, meta, Bytes::from("state")).unwrap();
                }
                for (_, r) in s.run_io(&mut disk, Nanos::ZERO).tickets {
                    r.unwrap();
                }
                println!("ok {core} {index}");
                index += 1;
            }
        });
    }
    log
}

#[path = "../common/disks.rs"]
mod disks;

/// The child: one engine, its writers in engine threads, until the parent kills it.
fn run_child(dir: &str) -> ! {
    let mode = std::env::var(MODE_VAR).unwrap_or_default();
    let shards = if mode == SHARED { SHARED_CORES } else { 1 };
    let e = disks::engine(shards, disks::from_env());
    let _log = match mode.as_str() {
        SHARED => Some(child_shared(&e, dir)),
        PIPELINED => {
            let dir = dir.to_string();
            disks::spawn_on(&e, 0, move || child_pipelined(&dir));
            None
        }
        _ => {
            let dir = dir.to_string();
            disks::spawn_on(&e, 0, move || child(&dir));
            None
        }
    };
    loop {
        std::thread::park();
    }
}

pub fn main() {
    match std::env::var("TESELADB_CHILD_DIR") {
        Ok(dir) => run_child(&dir),
        Err(_) => {
            // The parent's engine runs the device log it recovers.
            let e = disks::engine(1, DirectIo::Auto);
            for (name, _) in disks::executors() {
                sigkill_never_loses_confirmed_entries(&e, name, None);
                sigkill_never_loses_confirmed_entries(&e, name, Some(PIPELINED));
                sigkill_never_loses_confirmed_entries(&e, name, Some(SHARED));
            }
            e.stop();
            println!("crash_real: ok");
        }
    }
}

fn sigkill_never_loses_confirmed_entries(
    e: &teseladb_io::engine::NodeEngine,
    name: &str,
    mode: Option<&str>,
) {
    let tag = mode.unwrap_or("run-io");
    let dir = std::env::temp_dir()
        .join(format!("teseladb-crash-real-{name}-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let shared = mode == Some(SHARED);
    let cores = if shared { SHARED_CORES } else { 1 };
    let mut confirmed: Vec<Vec<u64>> = vec![Vec::new(); usize::from(cores)];
    let mut rng = Rng::new(42);
    let rounds: u64 =
        std::env::var("TESELADB_KILL_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(10);
    for round in 0..rounds {
        let mut c = Command::new(std::env::current_exe().unwrap())
            .env("TESELADB_CHILD_DIR", &dir)
            .env(disks::EXECUTOR_VAR, name)
            .env(MODE_VAR, tag)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = c.stdout.take().unwrap();
        let wanted = rng.range(50, 400);
        let mut seen = 0;
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if let Some(rest) = line.strip_prefix("ok ") {
                let (core, i) = match rest.split_once(' ') {
                    Some((c, i)) => (c.parse::<usize>().unwrap(), i),
                    None => (0, rest),
                };
                confirmed[core].push(i.parse().unwrap());
                seen += 1;
                if seen >= wanted {
                    break;
                }
            }
        }
        c.kill().unwrap(); // SIGKILL
        c.wait().unwrap();
        let stores: Vec<CoreStore> = if shared {
            let cfg_log = DeviceLogConfig { file_bytes: 1 << 20 };
            let disk = LogDisk::Files(dir.clone());
            let (log, ports) = DeviceLog::start(e, disk, cores, cfg_log, Arc::default()).unwrap();
            let stores = ports
                .into_iter()
                .map(|mut port| {
                    let mut own = BlockingDisk::open(&dir).unwrap();
                    CoreStore::recover(&cfg(), port.core(), &mut port.disk(&mut own))
                        .unwrap_or_else(|e| panic!("{name} {tag}, round {round}: {e:?}"))
                        .0
                })
                .collect();
            drop(log.stop());
            stores
        } else {
            let mut disk = BlockingDisk::open(&dir).unwrap();
            vec![
                CoreStore::recover(&cfg(), 0, &mut disk)
                    .unwrap_or_else(|e| panic!("{name} {tag}, round {round}: {e:?}"))
                    .0,
            ]
        };
        for (s, confirmed) in stores.iter().zip(&confirmed) {
            for &i in confirmed {
                match s.entries(G, i, i + 1, u64::MAX) {
                    // Covered by a durable snapshot: released from the WAL, which is correct.
                    Err(LogGap::Compacted { first }) if i < first => {}
                    got => assert_eq!(
                        got.ok().and_then(|v| v.first().cloned()),
                        Some(entry(i)),
                        "{tag}, round {round}, core {}, entry {i}",
                        s.core()
                    ),
                }
            }
            let last = confirmed.last().copied().unwrap_or(0);
            let have = s.group(G).map_or(0, |g| g.last_index());
            assert!(have >= last, "{tag}, round {round}, core {}", s.core());
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
