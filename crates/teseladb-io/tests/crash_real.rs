//! Real crash (001 FR-004, 012 FR-004). A child process appends Raft entries with each disk
//! executor (the disk thread's `FileDisk` and, on Linux, io_uring, with `O_DIRECT` where the file
//! system takes it) and prints every acknowledgement; the parent kills it with `SIGKILL` at an arbitrary point, recovers the
//! directory and checks that every acknowledged entry is there and correct.
//!
//! In the `pipelined` mode (012 FR-014, D-159) the child writes through a real disk thread with
//! several batches in flight, whose syncs run on its second thread and end out of order with the
//! writes of the next batches.
//!
//! In the `shared` mode (012 FR-035, D-154) four cores write at once through the node's device log,
//! each from its own thread, and the parent checks every core's entries after each kill.
//!
//! Without the test harness: the same binary is the parent and, with `TESELADB_CHILD_DIR`, the
//! child. `TESELADB_KILL_ROUNDS` (10 by default) sets the number of kills.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use std::collections::VecDeque;
use std::sync::Arc;

use teseladb_io::device_log::{DeviceLog, DeviceLogConfig};
use teseladb_io::disk_thread::DiskThread;
use teseladb_io::mailbox::{Notify, channel};
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

/// Nobody waits on the mailbox: the child looks at it in a loop.
struct Polled;

impl Notify for Polled {
    fn notify(&self) {}
}

/// Child process of the `pipelined` mode: appends through a disk thread, several entries and
/// batches at once, and prints "ok <index>" as each is acknowledged.
fn child_pipelined(dir: &str) {
    let cfg = StorageConfig { max_inflight_batches: INFLIGHT, ..cfg() };
    let mut disk = disks::open(dir, teseladb_io::node_disk::IoExecutor::Threads);
    let mut s = CoreStore::recover(&cfg, 0, &mut disk).unwrap().0;
    if s.group(G).is_none() {
        s.create_group(G, Bytes::new(), Durability::Strict).unwrap();
        s.run_io(&mut disk, Nanos::ZERO);
    }
    let (tx, mut rx) = channel(Arc::new(Polled));
    let thread = DiskThread::spawn("disk-crash".into(), disk, tx, |d| d).unwrap();
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
            thread.submit(op).unwrap();
        }
        rx.drain_into(&mut done);
        for d in done.drain(..) {
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

/// Child process of the `shared` mode: every core appends from its own thread through the device
/// log and prints "ok <core> <index>" for every acknowledged entry.
fn child_shared(dir: &str) {
    let exec = disks::from_env();
    let (_log, ports) = DeviceLog::start(
        disks::open(dir, exec),
        SHARED_CORES,
        DeviceLogConfig { file_bytes: 1 << 20 },
    )
    .unwrap();
    let _writers: Vec<_> = ports
        .into_iter()
        .map(|mut port| {
            let dir = dir.to_string();
            std::thread::spawn(move || {
                let core = port.core();
                let mut own = disks::open(&dir, exec);
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
            })
        })
        .collect();
    // Until the parent kills it.
    loop {
        std::thread::park();
    }
}

#[path = "common/disks.rs"]
mod disks;

fn main() {
    match std::env::var("TESELADB_CHILD_DIR") {
        Ok(dir) if std::env::var(MODE_VAR).as_deref() == Ok(PIPELINED) => child_pipelined(&dir),
        Ok(dir) if std::env::var(MODE_VAR).as_deref() == Ok(SHARED) => child_shared(&dir),
        Ok(dir) => child(&dir),
        Err(_) => {
            for (name, exec) in disks::executors() {
                sigkill_never_loses_confirmed_entries(name, exec, None);
            }
            let threads = teseladb_io::node_disk::IoExecutor::Threads;
            sigkill_never_loses_confirmed_entries("threads", threads, Some(PIPELINED));
            for (name, exec) in disks::executors() {
                sigkill_never_loses_confirmed_entries(name, exec, Some(SHARED));
            }
            println!("crash_real: ok");
        }
    }
}

fn sigkill_never_loses_confirmed_entries(
    name: &str,
    exec: teseladb_io::node_disk::IoExecutor,
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
            let (log, ports) = DeviceLog::start(disks::open(&dir, exec), cores, cfg_log).unwrap();
            let stores = ports
                .into_iter()
                .map(|mut port| {
                    let mut own = disks::open(&dir, exec);
                    CoreStore::recover(&cfg(), port.core(), &mut port.disk(&mut own))
                        .unwrap_or_else(|e| panic!("{name} {tag}, round {round}: {e:?}"))
                        .0
                })
                .collect();
            drop(log);
            stores
        } else {
            let mut disk = disks::open(&dir, exec);
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
