//! `lsm` scenario (004 FR-021, SC-003), on the real disk through `FileDisk`:
//! - `lsm.write`: a version into the memtable, under 1 µs at p50 (flushes run between batches,
//!   outside the timing);
//! - `lsm.flush_mb_s`: flushing a 64 MiB memtable, at least 300 MB/s on NVMe;
//! - `lsm.get_cached` and `lsm.scan_1000_cached`: a point read and a scan of 1 000 keys with
//!   every block cached, under 1.5 µs and 100 µs at p50;
//! - `lsm.get_disk`: a point read whose block comes from the disk (the page cache may serve
//!   it), without a target;
//! - `lsm.compaction_mb_s`: one compaction of every file, without a target.

use std::hint::black_box;
use std::time::Instant;

use teseladb_io::FileDisk;
use teseladb_keys::Hlc;
use teseladb_lsm::{Lookup, Lsm, LsmConfig, Step};
use teseladb_raftlog::{Disk, MemDisk};
use teseladb_types::{GroupId, Histogram, Nanos, Rng};

use super::{DataDir, at, keep_going, msg, warm_up};
use crate::cli::Args;
use crate::report::{Report, Target};

const G: GroupId = GroupId(1);
/// Keys of the read phases.
const KEYS: u64 = 200_000;
/// Value of every version: a small document.
const VALUE: [u8; 100] = [7; 100];
/// Writes timed together.
const BATCH: u64 = 1000;

fn key(i: u64) -> Vec<u8> {
    format!("user:{i:010}").into_bytes()
}

fn ts(i: u64) -> Hlc {
    Hlc { physical: i, logical: 0 }
}

/// Runs the pending I/O to the end.
fn drain(lsm: &mut Lsm, disk: &mut dyn Disk, clock: &Instant) {
    lsm.run_io(disk, Nanos(clock.elapsed().as_nanos() as u64));
}

fn get(lsm: &mut Lsm, disk: &mut dyn Disk, key: &[u8], clock: &Instant) -> bool {
    let v = lsm.view();
    loop {
        match v.get(lsm, key, ts(u64::MAX)) {
            Lookup::Hit(_) => return true,
            Lookup::Miss(_) => drain(lsm, disk, clock),
            _ => return false,
        }
    }
}

fn scan_1000(lsm: &mut Lsm, disk: &mut FileDisk, from: u64, clock: &Instant) -> usize {
    let v = lsm.view();
    let mut s = v.scan(&key(from), &key(from + 1000), ts(u64::MAX));
    let mut n = 0;
    loop {
        match s.next(lsm) {
            Step::Item(i) => {
                black_box(i);
                n += 1;
            }
            Step::Miss(_) => drain(lsm, disk, clock),
            _ => return n,
        }
    }
}

/// Writes a batch of [`BATCH`] versions after the `n` already written and returns its average in
/// nanoseconds per write; the flushes they cause run afterwards, outside the timing.
fn write_batch(
    lsm: &mut Lsm,
    disk: &mut dyn Disk,
    n: &mut u64,
    clock: &Instant,
) -> Result<u64, String> {
    let t = Instant::now();
    for _ in 0..BATCH {
        *n += 1;
        lsm.put(G, *n, &key(*n % KEYS), ts(*n), &VALUE).map_err(msg)?;
    }
    let ns = (t.elapsed().as_nanos() as u64 / BATCH).max(1);
    drain(lsm, disk, clock);
    Ok(ns)
}

/// An LSM in its own subfolder of the data folder, with its own disk executor.
fn open(dir: &DataDir, name: &str, cfg: &LsmConfig) -> Result<(Lsm, FileDisk), String> {
    let path = dir.path.join(name);
    let mut disk = FileDisk::open(&path).map_err(at(&path))?;
    let (lsm, _) = Lsm::recover(cfg, &mut disk).map_err(msg)?;
    Ok((lsm, disk))
}

/// Runs the scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    let dir = DataDir::new(args.data.as_deref(), "lsm")?;
    let cfg =
        LsmConfig { dir: "lsm".into(), compaction_bytes_per_sec: 1 << 40, ..LsmConfig::default() };
    let clock = Instant::now();
    let mut report = Report::new("lsm");
    // Writes, timed by batches after the untimed batches of the warm-up; the flushes they cause
    // run between batches, outside the timing.
    let (mut lsm, mut disk) = open(&dir, "write", &cfg)?;
    let mut n = 0;
    warm_up(args.warmup, || write_batch(&mut lsm, &mut disk, &mut n, &clock).map(drop))?;
    let mut write = Histogram::new();
    let start = Instant::now();
    while keep_going(write.count(), 20, start.elapsed() >= args.duration) {
        write.record(write_batch(&mut lsm, &mut disk, &mut n, &clock)?);
    }
    report.latency("lsm.write", &write, Some(1000.0), None);
    drop((lsm, disk));
    // One memtable of distinct keys, flushed and timed.
    let (mut lsm, mut disk) = open(&dir, "read", &cfg)?;
    let mut bytes = 0u64;
    for i in 0..KEYS {
        lsm.put(G, i + 1, &key(i), ts(1), &VALUE).map_err(msg)?;
        bytes += (key(i).len() + VALUE.len()) as u64;
    }
    let t = Instant::now();
    lsm.flush();
    drain(&mut lsm, &mut disk, &clock);
    let flush_mb_s = bytes as f64 / t.elapsed().as_secs_f64() / 1e6;
    report.push("lsm.flush_mb_s", "MB/s", flush_mb_s, Some(Target::AtLeast(300.0)));
    // Every block cached: point reads and scans.
    for i in (0..KEYS).step_by(50) {
        get(&mut lsm, &mut disk, &key(i), &clock);
    }
    let mut rng = Rng::new(7);
    let cached = super::per_op(args.warmup, args.duration, || {
        let k = key(rng.range(0, KEYS));
        black_box(get(&mut lsm, &mut disk, &k, &clock));
    });
    report.latency("lsm.get_cached", &cached, Some(1500.0), None);
    let mut from = 0;
    let scan = super::per_op(args.warmup, args.duration, || {
        from = (from + 1000) % (KEYS - 1000);
        black_box(scan_1000(&mut lsm, &mut disk, from, &clock));
    });
    report.latency("lsm.scan_1000_cached", &scan, Some(100_000.0), None);
    // Reads from the disk: the same files under a new LSM without a block cache, so every read
    // goes to the disk (the operating system's page cache may still serve it).
    drop((lsm, disk));
    let uncached = LsmConfig { cache_bytes: 0, ..cfg.clone() };
    let (mut cold, mut disk) = open(&dir, "read", &uncached)?;
    let mut next = 0;
    let from_disk = super::per_op(args.warmup, args.duration, || {
        next = (next + 997) % KEYS;
        black_box(get(&mut cold, &mut disk, &key(next), &clock));
    });
    report.latency("lsm.get_disk", &from_disk, None, None);
    // Three more files of overwrites, then one compaction of everything.
    for round in 2..5 {
        for i in 0..KEYS / 2 {
            cold.put(G, KEYS * round + i, &key(i * 2), ts(round), &VALUE).map_err(msg)?;
        }
        cold.flush();
        drain(&mut cold, &mut disk, &clock);
    }
    let before: u64 = cold.metrics().level_bytes.iter().sum();
    let t = Instant::now();
    cold.compact_all();
    drain(&mut cold, &mut disk, &clock);
    let compaction_mb_s = before as f64 / t.elapsed().as_secs_f64() / 1e6;
    report.push("lsm.compaction_mb_s", "MB/s", compaction_mb_s, None);
    Ok(report)
}

/// Shape of the sustained write.
#[derive(Clone, Debug)]
struct Sustained {
    /// LSM sizes.
    lsm: LsmConfig,
    /// Keys: every write overwrites one of them.
    live_keys: u64,
    /// Writes per GC window (`gc_ttl`): versions younger than this stay.
    ttl_writes: u64,
    /// How often the reads and the space are measured.
    check_every: std::time::Duration,
    /// Writes made before `--duration` may end the run.
    min_writes: u64,
}

/// The measurement's shape (004 SC-004): memtables, level 1 and files at 1/16 of the defaults.
fn measurement() -> Sustained {
    Sustained {
        lsm: LsmConfig {
            dir: "lsm".into(),
            memtable_bytes: 4 << 20,
            l1_bytes: 4 << 20,
            target_file_bytes: 1 << 20,
            cache_bytes: 16 << 20,
            ..LsmConfig::default()
        },
        live_keys: 1_000_000,
        ttl_writes: 500_000,
        check_every: std::time::Duration::from_secs(1),
        min_writes: 0,
    }
}

/// `lsm-sustained` scenario (004 SC-004): writes without pause for `--duration` (one hour in the
/// measurement) over a fixed set of keys, with the GC threshold `ttl_writes` writes behind, and
/// point reads every second. Reports the worst read amplification (files searched per point read,
/// at most 8) and the worst space against the live data plus what the window keeps (at most 1.5).
/// It runs on a disk in memory: it measures the shape of the LSM, not the device (the `lsm`
/// scenario does), and an hour of compactions would wear an SSD for nothing. Memtables, level 1
/// and files are 1/16 of the defaults, so the 172 MB of live data stand to them as 2.75 GB to the
/// defaults; the writer waits while the LSM asks to slow down or to reject, as the leader does
/// (FR-012). A metric the run was too short to measure (no check, or the space before two GC
/// windows of writes) is reported as not measured, a missed target. Nothing is timed, so
/// `--warmup` does not apply.
pub fn sustained(args: &Args) -> Result<Report, String> {
    sustained_with(args, measurement())
}

fn sustained_with(args: &Args, shape: Sustained) -> Result<Report, String> {
    let Sustained { lsm: cfg, live_keys, ttl_writes, check_every, min_writes } = shape;
    let mut disk = MemDisk::new();
    let (mut lsm, _) = Lsm::recover(&cfg, &mut disk).map_err(msg)?;
    let clock = Instant::now();
    let entry = (key(0).len() + VALUE.len()) as f64;
    let (mut worst_reads, mut worst_space): (Option<f64>, Option<f64>) = (None, None);
    let (mut n, mut last_check) = (0u64, Instant::now());
    let mut rng = Rng::new(11);
    while keep_going(n, min_writes, clock.elapsed() >= args.duration) {
        for _ in 0..BATCH {
            n += 1;
            lsm.put(G, n, &key(rng.range(0, live_keys)), ts(n), &VALUE).map_err(msg)?;
        }
        if n > ttl_writes {
            let gc = vec![(Vec::new(), b"\xff".to_vec(), ts(n - ttl_writes))];
            lsm.set_gc_thresholds(teseladb_lsm::GcTable::new(gc));
        }
        drain(&mut lsm, &mut disk, &clock);
        // As the leader (FR-012): no new proposals while the LSM asks to slow down or to reject
        // them, until the compactions catch up (the run may end a little after its duration).
        while lsm.pressure() != teseladb_lsm::Pressure::Normal {
            std::thread::sleep(std::time::Duration::from_millis(1));
            drain(&mut lsm, &mut disk, &clock);
        }
        if last_check.elapsed() >= check_every {
            last_check = Instant::now();
            let before = lsm.metrics();
            for _ in 0..1000 {
                get(&mut lsm, &mut disk, &key(rng.range(0, live_keys)), &clock);
            }
            let after = lsm.metrics();
            let gets = (after.gets_total - before.gets_total) as f64;
            let files = (after.get_files_total - before.get_files_total) as f64;
            worst_reads = Some(worst_reads.unwrap_or(0.0).max(files / gets));
            let live = live_keys.min(n) as f64 * entry + ttl_writes.min(n) as f64 * entry;
            let on_disk: u64 = after.level_bytes.iter().sum();
            if n > 2 * ttl_writes {
                worst_space = Some(worst_space.unwrap_or(0.0).max(on_disk as f64 / live));
            }
        }
    }
    let mut report = Report::new("lsm-sustained");
    report.push("lsm.sustained_writes", "writes", n as f64, None);
    let mut measured = |name: &str, unit, v: Option<f64>, target| {
        if v.is_none() {
            report.notes.push(format!("{name} not measured: the run was too short"));
        }
        report.push(name, unit, v.unwrap_or(f64::NAN), Some(target));
    };
    measured("lsm.sustained_read_files_max", "files", worst_reads, Target::AtMost(8.0));
    measured("lsm.sustained_space_max", "ratio", worst_space, Target::AtMost(1.5));
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_short_run_reports_every_metric() {
        let args = Args {
            scenario: "lsm".into(),
            duration: std::time::Duration::from_millis(30),
            warmup: std::time::Duration::from_millis(5),
            ..Args::default()
        };
        let r = run(&args).unwrap();
        for name in [
            "lsm.write.p50",
            "lsm.flush_mb_s",
            "lsm.get_cached.p50",
            "lsm.scan_1000_cached.p50",
            "lsm.get_disk.p50",
            "lsm.compaction_mb_s",
        ] {
            assert!(r.metrics.iter().any(|m| m.name == name), "{name}");
        }
    }

    #[test]
    fn a_small_sustained_run_measures_reads_and_space() {
        // The measurement's shape scaled down: an LSM of 64 KiB memtables, 2,000 keys, a GC window
        // of 1,000 writes, a check after every batch and 20,000 writes, whatever the machine's
        // speed.
        let shape = Sustained {
            lsm: LsmConfig {
                dir: "lsm".into(),
                memtable_bytes: 64 << 10,
                l1_bytes: 64 << 10,
                target_file_bytes: 16 << 10,
                cache_bytes: 1 << 20,
                ..LsmConfig::default()
            },
            live_keys: 2_000,
            ttl_writes: 1_000,
            check_every: std::time::Duration::ZERO,
            min_writes: 20_000,
        };
        let args = Args {
            scenario: "lsm-sustained".into(),
            duration: std::time::Duration::ZERO,
            ..Args::default()
        };
        let r = sustained_with(&args, shape).unwrap();
        let value = |name: &str| r.metrics.iter().find(|m| m.name == name).unwrap().value;
        assert_eq!(value("lsm.sustained_writes"), 20_000.0);
        assert!((1.0..=8.0).contains(&value("lsm.sustained_read_files_max")), "{r}");
        assert!(value("lsm.sustained_space_max") > 0.0, "{r}");
        assert!(r.notes.is_empty(), "{r}");
    }

    #[test]
    fn the_writer_waits_while_the_lsm_asks_to_slow_down() {
        // One file in level 0 slows the writes down until its compaction ends; no check falls
        // within the run, so nothing is measured.
        let shape = Sustained {
            lsm: LsmConfig {
                dir: "lsm".into(),
                memtable_bytes: 16 << 10,
                l1_bytes: 64 << 10,
                target_file_bytes: 16 << 10,
                cache_bytes: 1 << 20,
                l0_trigger: 1,
                l0_slow: 1,
                // Compactions slow enough that the writer finds the LSM asking it to wait.
                compaction_bytes_per_sec: 1 << 20,
                ..LsmConfig::default()
            },
            live_keys: 500,
            ttl_writes: 1_000,
            check_every: std::time::Duration::from_secs(3600),
            min_writes: 5_000,
        };
        let args = Args {
            scenario: "lsm-sustained".into(),
            duration: std::time::Duration::ZERO,
            ..Args::default()
        };
        let r = sustained_with(&args, shape).unwrap();
        let value = |name: &str| r.metrics.iter().find(|m| m.name == name).unwrap().value;
        assert_eq!(value("lsm.sustained_writes"), 5_000.0);
        assert_eq!(r.missed().len(), 2, "{r}");
    }

    #[test]
    fn a_run_too_short_to_measure_misses_its_targets() {
        let args = Args {
            scenario: "lsm-sustained".into(),
            duration: std::time::Duration::ZERO,
            ..Args::default()
        };
        let r = sustained(&args).unwrap();
        assert_eq!(r.missed().len(), 2, "{r}");
        assert_eq!(r.notes.len(), 2, "{r}");
        assert!(r.to_json().contains("\"value\":null"), "{}", r.to_json());
    }
}
