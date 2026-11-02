//! Scenarios (contract `bench-cli.md` of 002): 001 registers `raftlog`, its `raftlog-recover`
//! variant and the copied `deflate`; 003 adds `bson` and `keys`; 004 adds `lsm` and `lsm-compression`; 005 adds `write`, `read` and
//! `failover`; 006 adds `wire` and `crud`; 007 adds `follower-read`, `group0-limits`, `split`, `add-node` and `rebalance`; 008 adds `txn`, `txn-readers` and `contention`; 013 adds
//! `tls-handshake` and `scram`; 010 adds `aggregate`, `sort` and `group-large`; 015 adds
//! `timeseries`; 014 adds `changestream-lag`; 016 adds `js`; every specification
//! adds its own.

pub mod bson;
pub mod changestream;
pub mod cluster;
pub mod crud;
pub mod deflate;
pub mod failover;
pub mod follower_read;
pub mod group0_limits;
pub mod index;
pub mod js;
pub mod keys;
pub mod lsm;
pub mod lsm_compression;
pub mod query;
pub mod raftlog;
pub mod read;
pub mod reference;
pub mod security;
pub mod timeseries;
pub mod txn;
pub mod wire;
pub mod write;
pub mod ycsb;
pub mod ycsb_gen;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use teseladb_types::Histogram;

use crate::cli::Args;
use crate::report::Report;

/// Runs of an operation per timed batch.
const BATCH: u32 = 1000;
/// At least this many batches, however short the duration.
const MIN_BATCHES: u64 = 20;
/// Temporary data folders made by this process so far: each one gets the next number, so that
/// scenarios running at once in one process (the tests) never share one.
static TEMPORARY_DIRS: AtomicU64 = AtomicU64::new(0);

/// Whether a measuring loop goes on: fewer than `min` operations, or its end not `reached`.
pub(crate) fn keep_going(done: u64, min: u64, reached: bool) -> bool {
    done < min || !reached
}

/// Data directory: the requested one (it has to be empty) or a temporary one deleted at the end.
#[derive(Debug)]
pub(crate) struct DataDir {
    pub(crate) path: PathBuf,
    temporary: bool,
}

/// The text of an error.
pub(crate) fn msg(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// An I/O error with the path it happened on.
pub(crate) fn at(p: &Path) -> impl Fn(std::io::Error) -> String + '_ {
    move |e| format!("{}: {e}", p.display())
}

impl DataDir {
    /// The requested folder, or a temporary one named after the scenario `name`, the process and
    /// a number no other temporary folder of the process has.
    pub(crate) fn new(requested: Option<&Path>, name: &str) -> Result<DataDir, String> {
        match requested {
            Some(p) => {
                std::fs::create_dir_all(p).map_err(at(p))?;
                let empty = std::fs::read_dir(p).map_err(at(p))?.next().is_none();
                if !empty {
                    return Err(format!("{} is not empty", p.display()));
                }
                Ok(DataDir { path: p.to_path_buf(), temporary: false })
            }
            None => {
                let n = TEMPORARY_DIRS.fetch_add(1, Ordering::Relaxed);
                let p = std::env::temp_dir()
                    .join(format!("teseladb-bench-{name}-{}-{n}", std::process::id()));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).map_err(at(&p))?;
                Ok(DataDir { path: p, temporary: true })
            }
        }
    }
}

impl Drop for DataDir {
    fn drop(&mut self) {
        if self.temporary {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// Warm-up (`--warmup`): runs `op` for about `warmup` without timing it, at least once unless
/// `warmup` is zero, and returns how many times it ran; an error of `op` ends it.
pub(crate) fn warm_up<E>(
    warmup: Duration,
    mut op: impl FnMut() -> Result<(), E>,
) -> Result<u64, E> {
    let start = Instant::now();
    let mut runs = 0;
    while keep_going(runs, u64::from(!warmup.is_zero()), start.elapsed() >= warmup) {
        op()?;
        runs += 1;
    }
    Ok(runs)
}

/// Times `op` in batches for about `duration`, after running it untimed for `warmup`: each
/// batch's average in nanoseconds per run is one sample, so that the clock's own cost does not
/// count against operations of tens of nanoseconds.
pub(crate) fn per_op(warmup: Duration, duration: Duration, mut op: impl FnMut()) -> Histogram {
    let Ok(_) = warm_up(warmup, || {
        op();
        Ok::<(), std::convert::Infallible>(())
    });
    let mut h = Histogram::new();
    let start = Instant::now();
    let mut batches = 0;
    while keep_going(batches, MIN_BATCHES, start.elapsed() >= duration) {
        let t0 = Instant::now();
        for _ in 0..BATCH {
            op();
        }
        h.record(t0.elapsed().as_nanos() as u64 / u64::from(BATCH));
        batches += 1;
    }
    h
}

/// Runs the requested scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    match args.scenario.as_str() {
        "raftlog" => raftlog::run(args),
        "raftlog-recover" => raftlog::recover_existing(args),
        "deflate" => deflate::run(args),
        "bson" => bson::run(args),
        "js" => js::run(args),
        "keys" => keys::run(args),
        "lsm" => lsm::run(args),
        "lsm-sustained" => lsm::sustained(args),
        "lsm-compression" => lsm_compression::run(args),
        "write" => write::run(args),
        "read" => read::run(args),
        "failover" => failover::run(args),
        "follower-read" => follower_read::run(args),
        "group0-limits" => group0_limits::run(args),
        "split" => cluster::split(args),
        "add-node" => cluster::add_node(args),
        "rebalance" => cluster::rebalance(args),
        "reference" => reference::run(args),
        "wire" => wire::run(args),
        "crud" => crud::run(args),
        "index-write" => index::run_write(args),
        "index-read" => index::run_read(args),
        "aggregate" => query::run_aggregate(args),
        "sort" => query::run_sort(args),
        "group-large" => query::run_group_large(args),
        "txn" => txn::run(args),
        "txn-readers" => txn::readers_run(args),
        "contention" => txn::contention(args),
        "tls-handshake" => security::tls_handshake(args),
        "scram" => security::scram(args),
        "timeseries" => timeseries::run(args),
        "changestream-lag" => changestream::run(args),
        s if s.len() == 6 && s.starts_with("ycsb-") => {
            ycsb::run(args, s.chars().last().unwrap_or('?'))
        }
        other => Err(format!(
            "unknown scenario: {other} (available: raftlog, raftlog-recover, deflate, bson, keys, lsm, lsm-sustained, lsm-compression, \
             write, read, failover, follower-read, group0-limits, split, add-node, rebalance, reference, wire, crud, index-write, index-read, aggregate, sort, group-large, txn, txn-readers, contention, tls-handshake, scram, timeseries, changestream-lag, ycsb-a … ycsb-f)"
        )),
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_loop_goes_on_until_its_minimum_and_its_end() {
        assert!(keep_going(0, 2, true), "below the minimum");
        assert!(keep_going(2, 2, false), "the end not reached");
        assert!(!keep_going(2, 2, true));
    }

    #[test]
    fn two_temporary_folders_at_once_differ_and_are_deleted() {
        let a = DataDir::new(None, "lsm").unwrap();
        let b = DataDir::new(None, "lsm").unwrap();
        assert_ne!(a.path, b.path);
        assert!(a.path.is_dir() && b.path.is_dir());
        assert!(a.path.file_name().unwrap().to_string_lossy().contains("-lsm-"), "{a:?}");
        let (pa, pb) = (a.path.clone(), b.path.clone());
        drop(a);
        assert!(!pa.exists() && pb.is_dir());
        drop(b);
        assert!(!pb.exists());
    }

    #[test]
    fn a_requested_folder_is_kept() {
        let p = std::env::temp_dir().join(format!("teseladb-bench-kept-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        drop(DataDir::new(Some(&p), "lsm").unwrap());
        assert!(p.is_dir());
        std::fs::write(p.join("f"), b"x").unwrap();
        assert!(DataDir::new(Some(&p), "lsm").unwrap_err().contains("is not empty"));
        std::fs::remove_dir_all(&p).unwrap();
    }

    #[test]
    fn without_a_warm_up_nothing_runs() {
        let mut runs = 0;
        let warmed = warm_up(Duration::ZERO, || {
            runs += 1;
            Ok::<(), String>(())
        });
        assert_eq!((warmed, runs), (Ok(0), 0));
    }

    #[test]
    fn a_warm_up_runs_at_least_once_and_stops_at_an_error() {
        let mut runs = 0u64;
        let warmed = warm_up(Duration::from_nanos(1), || {
            runs += 1;
            Ok::<(), String>(())
        });
        assert!(warmed.unwrap() >= 1);
        let failed = warm_up(Duration::from_secs(3600), || Err::<(), _>("broken".to_string()));
        assert_eq!(failed, Err("broken".to_string()));
    }

    #[test]
    fn per_op_discards_the_warm_up_runs() {
        let mut runs = 0u64;
        let h = per_op(Duration::from_nanos(1), Duration::ZERO, || runs += 1);
        // A zero duration times exactly the minimum of batches; the warm-up ran on top of them.
        assert_eq!(h.count(), MIN_BATCHES);
        assert!(runs > MIN_BATCHES * u64::from(BATCH), "{runs}");
        let mut runs = 0u64;
        let h = per_op(Duration::ZERO, Duration::ZERO, || runs += 1);
        assert_eq!((h.count(), runs), (MIN_BATCHES, MIN_BATCHES * u64::from(BATCH)));
    }

    #[test]
    fn unknown_scenario_is_an_error() {
        let err = run(&Args { scenario: "nope".into(), ..Args::default() }).unwrap_err();
        assert!(err.contains("unknown scenario"));
    }
}
