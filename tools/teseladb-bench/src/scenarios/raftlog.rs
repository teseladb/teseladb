//! `raftlog` scenario (001 FR-009): requests per batch and sync latency with many groups, latency of
//! a `strict` append until it is pending, reads of the log from memory and recovery of the WAL. The
//! targets were set for Linux with NVMe; in other environments the numbers are indicative. The
//! warm-up (`--warmup`) runs the groups phase, the one timed by `--duration`; the other phases
//! run a number of operations (`--records`) and have none.

use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use teseladb_io::FileDisk;
use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::{CoreStore, Disk, IoDone, IoOp, LogEntry, StorageConfig, open_node};
use teseladb_types::{Bytes, Durability, GroupId, Histogram, Nanos, Rng};

use super::{DataDir, at, msg};
use crate::cli::Args;
use crate::report::{Report, Target};

/// Group of the append and read phases.
const LOG_GROUP: GroupId = GroupId(1);
/// Size of the appended entries.
const ENTRY_LEN: usize = 1024;

fn disk(path: &Path) -> Result<FileDisk, String> {
    FileDisk::open(path).map_err(at(path))
}

fn entry(index: u64, rng: &mut Rng) -> LogEntry {
    let mut data = vec![0u8; ENTRY_LEN];
    rng.fill(&mut data);
    LogEntry { index, term: 1, kind: EntryKind::Normal, data: Bytes::from_vec(data) }
}

/// Runs all the available I/O synchronously.
fn drain(s: &mut CoreStore, disk: &mut FileDisk, clock: &Instant) {
    loop {
        let now = Nanos(clock.elapsed().as_nanos() as u64);
        s.run_io(disk, now);
        match s.next_deadline() {
            None => break,
            Some(d) => {
                let wait = d.saturating_since(Nanos(clock.elapsed().as_nanos() as u64));
                thread::sleep(std::time::Duration::from_nanos(wait.0));
            }
        }
    }
}

/// Groups phase: every group always has a `strict` request in progress (closed loop) and a disk
/// thread runs the operations, like 002's core loop. Once `until` (time elapsed, requests answered)
/// says so it makes no more requests and returns when those in flight are answered.
fn groups_phase(
    s: &mut CoreStore,
    disk: FileDisk,
    groups: u64,
    until: impl Fn(std::time::Duration, u64) -> bool,
    clock: &Instant,
) -> Result<(FileDisk, u64), String> {
    let (op_tx, op_rx) = mpsc::channel::<IoOp>();
    let (done_tx, done_rx) = mpsc::channel::<IoDone>();
    let worker = thread::spawn(move || {
        let mut disk = disk;
        // A send fails only once the phase stopped listening: nothing else to execute.
        let _ = op_rx.into_iter().try_for_each(|op| done_tx.send(disk.execute(op)));
        disk
    });
    let now = || Nanos(clock.elapsed().as_nanos() as u64);
    let mut next_index = vec![1u64; groups as usize];
    let mut owner = std::collections::HashMap::new();
    let append = |s: &mut CoreStore, g: u64, next: &mut Vec<u64>| -> Result<_, String> {
        let i = next[g as usize];
        next[g as usize] += 1;
        let e = LogEntry {
            index: i,
            term: 1,
            kind: EntryKind::Normal,
            data: Bytes::from_vec(vec![g as u8; 100]),
        };
        s.append_raft(GroupId(g), &[e], None, Durability::Strict).map_err(msg)
    };
    for g in 0..groups {
        s.create_group(GroupId(g), Bytes::new(), Durability::Strict).map_err(msg)?;
    }
    for g in 0..groups {
        let t = append(s, g, &mut next_index)?;
        owner.insert(t, g);
    }
    let start = Instant::now();
    let mut outstanding = 0usize;
    let mut completed = 0u64;
    loop {
        let stop = until(start.elapsed(), completed);
        while let Some(op) = s.poll_io(now()) {
            op_tx.send(op).map_err(|_| "disk thread down")?;
            outstanding += 1;
        }
        if outstanding == 0 {
            // Nothing in flight: the store may be waiting for a timer (a group-commit window);
            // nobody would answer a `recv` now. Without one either, every append made has been
            // answered: no more come after the stop.
            let Some(d) = s.next_deadline() else { break };
            let wait = d.saturating_since(now());
            thread::sleep(std::time::Duration::from_nanos(wait.0));
            continue;
        }
        let done = done_rx.recv().map_err(|_| "disk thread down")?;
        outstanding -= 1;
        let c = s.on_io_done(done, now());
        for (t, r) in c.tickets {
            r.map_err(msg)?;
            if let Some(g) = owner.remove(&t) {
                completed += 1;
                if !stop {
                    let t = append(s, g, &mut next_index)?;
                    owner.insert(t, g);
                }
            }
        }
    }
    drop(op_tx);
    let disk = worker.join().map_err(|_| "disk thread down")?;
    Ok((disk, completed))
}

/// Warm-up of the groups phase: the phase for `--warmup` on core 1 of a node of its own under
/// `path`, deleted afterwards, so that nothing it did counts.
fn warm_groups(path: &Path, args: &Args, clock: &Instant) -> Result<(), String> {
    let mut d = disk(path)?;
    open_node(&mut d, 2, [0; 16], 0).map_err(msg)?;
    let (mut s, _) = CoreStore::recover(&StorageConfig::default(), 1, &mut d).map_err(msg)?;
    groups_phase(&mut s, d, args.groups, |elapsed, _| elapsed >= args.warmup, clock)?;
    drop(s);
    std::fs::remove_dir_all(path).map_err(at(path))
}

/// Median of three recoveries of a core: (seconds, entries in its groups, WAL bytes).
fn measure_recovery(path: &Path, core: u16) -> Result<(f64, u64, u64), String> {
    let cfg = StorageConfig::default();
    let wal_bytes: u64 = std::fs::read_dir(path.join(format!("core-{core:02}/wal")))
        .map(|rd| rd.flatten().filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum())
        .unwrap_or(0);
    let mut times = Vec::new();
    let mut entries = 0;
    for _ in 0..3 {
        let mut d = disk(path)?;
        let t0 = Instant::now();
        let (s, _) = CoreStore::recover(&cfg, core, &mut d).map_err(msg)?;
        times.push(t0.elapsed().as_secs_f64().max(1e-9));
        entries = s
            .group_ids()
            .into_iter()
            .filter_map(|g| s.group(g))
            .map(|l| l.last_index() - l.snapshot_index)
            .sum();
        drop(s);
    }
    times.sort_by(f64::total_cmp);
    Ok((times[1], entries, wal_bytes))
}

/// Runs the scenario. Core 1 holds the many groups and core 0 the single group of the append and
/// read phases, to measure batching and recovery separately.
pub fn run(args: &Args) -> Result<Report, String> {
    let dir = DataDir::new(args.data.as_deref(), "raftlog")?;
    let mut report = Report::new("raftlog");
    report.notes.push(format!("{} {}", std::env::consts::OS, std::env::consts::ARCH));
    #[cfg(not(target_os = "linux"))]
    report.notes.push("targets set for Linux with NVMe: indicative here".into());
    let cfg = StorageConfig::default();
    let mut d = disk(&dir.path)?;
    open_node(&mut d, 2, [0; 16], 0).map_err(msg)?;
    let clock = Instant::now();
    if !args.warmup.is_zero() {
        warm_groups(&dir.path.join("warmup"), args, &clock)?;
    }

    // 1. Requests per batch and sync latency with many groups (core 1, with a disk thread).
    let (mut g, _) = CoreStore::recover(&cfg, 1, &mut d).map_err(msg)?;
    let until = |elapsed, _| elapsed >= args.duration;
    let (mut d, completed) = groups_phase(&mut g, d, args.groups, until, &clock)?;
    let m = g.metrics();
    let secs = args.duration.as_secs_f64().max(1e-9);
    report.push(
        "groups.requests_per_batch",
        "req",
        m.batched_requests as f64 / m.batches.max(1) as f64,
        Some(Target::AtLeast(20.0)),
    );
    report.push("groups.syncs_per_s", "1/s", m.batches as f64 / secs, None);
    report.push("groups.requests_per_s", "1/s", completed as f64 / secs, None);
    report.latency("sync", &m.sync_latency, None, None);
    drop(g);

    // 2. A `strict` append of a 1 KiB entry until it is pending (without the sync).
    let (mut s, _) = CoreStore::recover(&cfg, 0, &mut d).map_err(msg)?;
    s.create_group(LOG_GROUP, Bytes::new(), Durability::Strict).map_err(msg)?;
    let mut rng = Rng::new(1);
    let mut append = Histogram::new();
    for i in 1..=args.records {
        let e = entry(i, &mut rng);
        let t0 = Instant::now();
        s.append_raft(LOG_GROUP, &[e], None, Durability::Strict).map_err(msg)?;
        append.record(t0.elapsed().as_nanos() as u64);
        if i % 256 == 0 {
            drain(&mut s, &mut d, &clock);
        }
    }
    drain(&mut s, &mut d, &clock);
    report.latency("append_pending", &append, Some(2000.0), None);

    // 3. Reads of single entries from the log in memory.
    let mut read = Histogram::new();
    for _ in 0..args.records.min(1_000_000) {
        let i = rng.range(1, args.records + 1);
        let t0 = Instant::now();
        let got = s.entries(LOG_GROUP, i, i + 1, u64::MAX).expect("an appended entry");
        read.record(t0.elapsed().as_nanos() as u64);
        assert_eq!(got.len(), 1, "every appended entry is in memory");
    }
    report.latency("entry_read", &read, Some(1000.0), Some(5000.0));
    drop(s);
    drop(d);

    // 4. Recovery (median of three) of each core's WAL.
    for core in [0, 1] {
        let (secs, entries, wal_bytes) = measure_recovery(&dir.path, core)?;
        report.push(&format!("recovery.core{core}.ms"), "ms", secs * 1000.0, None);
        report.push(&format!("recovery.core{core}.entries"), "n", entries as f64, None);
        report.push(
            &format!("recovery.core{core}.wal_mb_per_s"),
            "MB/s",
            wal_bytes as f64 / 1e6 / secs,
            Some(Target::AtLeast(500.0)),
        );
    }
    Ok(report)
}

/// `raftlog-recover` scenario: recovers an existing data directory (for example the one left by
/// `raftlog --data dir`) several times and measures each core's recovery. It modifies nothing that
/// normal recovery would not modify.
pub fn recover_existing(args: &Args) -> Result<Report, String> {
    let path = args.data.as_deref().ok_or("raftlog-recover needs --data")?;
    let mut report = Report::new("raftlog-recover");
    let mut core = 0u16;
    while path.join(format!("core-{core:02}")).is_dir() {
        let (secs, entries, wal_bytes) = measure_recovery(path, core)?;
        report.push(&format!("core{core}.recovery.ms"), "ms", secs * 1000.0, None);
        report.push(&format!("core{core}.entries"), "n", entries as f64, None);
        report.push(
            &format!("core{core}.wal_mb_per_s"),
            "MB/s",
            wal_bytes as f64 / 1e6 / secs,
            None,
        );
        core += 1;
    }
    if core == 0 {
        return Err(format!("{}: no cores", path.display()));
    }
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_groups_phase_waits_for_a_group_commit_window() {
        let tmp = std::env::temp_dir().join(format!("teseladb-bench-gc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let cfg =
            StorageConfig { group_commit_delay: Nanos::from_millis(1), ..StorageConfig::default() };
        let mut d = disk(&tmp).unwrap();
        let (mut s, _) = CoreStore::recover(&cfg, 0, &mut d).unwrap();
        let clock = Instant::now();
        // It returns (it used to hang waiting with nothing in flight), and once it stops it answers
        // the requests it had made and makes no more: 20 groups, a request each in flight.
        let (_, completed) = groups_phase(&mut s, d, 20, |_, n| n >= 100, &clock).unwrap();
        // The stop is read once per round of answers: at most two rounds of 20 past the 100.
        assert!((100..=140).contains(&completed), "{completed}");
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn small_run_produces_every_metric() {
        let tmp = std::env::temp_dir().join(format!("teseladb-bench-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let args = Args {
            scenario: "raftlog".into(),
            data: Some(tmp.clone()),
            records: 2000,
            groups: 50,
            duration: std::time::Duration::from_millis(200),
            warmup: std::time::Duration::from_millis(20),
            ..Args::default()
        };
        let r = run(&args).unwrap();
        // The warm-up's folder is gone: only the two cores and the node's files remain.
        assert!(!tmp.join("warmup").exists());
        for name in [
            "append_pending.p50",
            "entry_read.p99",
            "groups.requests_per_batch",
            "sync.p50",
            "recovery.core0.wal_mb_per_s",
            "recovery.core1.wal_mb_per_s",
        ] {
            assert!(r.metrics.iter().any(|m| m.name == name), "{name}: {r}");
        }
        let entries = r.metrics.iter().find(|m| m.name == "recovery.core0.entries").unwrap().value;
        assert_eq!(entries, 2000.0);
        // Recover the existing directory.
        let rec = recover_existing(&args).unwrap();
        assert_eq!(rec.metrics.iter().find(|m| m.name == "core0.entries").unwrap().value, 2000.0);
        assert!(rec.metrics.iter().any(|m| m.name == "core1.wal_mb_per_s"));
        // The requested directory is no longer empty: rejected.
        assert!(run(&args).unwrap_err().contains("is not empty"));
        let none = Args { data: Some(tmp.join("empty")), ..args.clone() };
        assert!(recover_existing(&none).unwrap_err().contains("no cores"));
        assert!(recover_existing(&Args::default()).is_err());
        // A byte changed in the first batch, with valid batches after it: recovery reports the
        // damage. (A bad header alone would be a segment a crash left half created, and a byte past
        // the last batch, in the preallocated space, a torn tail.)
        let wal = tmp.join("core-00/wal");
        let seg = std::fs::read_dir(&wal).unwrap().flatten().next().unwrap().path();
        let mut data = std::fs::read(&seg).unwrap();
        data[4096 + 64] ^= 0xff;
        std::fs::write(&seg, data).unwrap();
        assert!(recover_existing(&args).unwrap_err().contains("corrupt"));
        // A data directory that is a file.
        let file = tmp.join("a-file");
        std::fs::write(&file, b"x").unwrap();
        let bad = Args { data: Some(file.join("data")), ..args.clone() };
        assert!(run(&bad).unwrap_err().contains("a-file"));
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
