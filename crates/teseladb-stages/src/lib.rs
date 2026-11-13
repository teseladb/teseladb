//! How long each stage of an operation takes (012 FR-015): one histogram per [`Stage`], shared by
//! the whole process, that `/metrics` gives as `teseladb_stage_seconds{stage="…"}`.
//!
//! The counters are atomics, so any thread records without a lock (012 FR-008), and the crate
//! never reads a clock: the caller measures with the time it already has and passes nanoseconds.
//! The buckets are powers of two in microseconds, from 1 µs to 32 s (2²⁵ µs), and one more for
//! what is longer.
//!
//! ```
//! use teseladb_stages::{Stage, record, render};
//!
//! record(Stage::WriteLatchWait, 3_000);
//! let mut out = String::new();
//! render(&mut out);
//! assert!(out.contains("teseladb_stage_seconds_count{stage=\"write_latch_wait\"}"));
//! ```
#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

/// The name of the histogram.
pub const NAME: &str = "teseladb_stage_seconds";
/// Buckets with a bound: the one of bucket `i` is 2ⁱ µs.
pub const BOUNDS: usize = 26;
/// Buckets with the one past the largest bound.
const SLOTS: usize = BOUNDS + 1;

/// A stage of an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// A write's wait for the latches of its keys on the range's leader.
    WriteLatchWait,
    /// From a write's proposal to its application on the leader.
    WriteProposeToApplied,
    /// A write in its range, from its arrival on the core to its answer.
    WriteRange,
    /// A sync's wait for the shared flush of its drive.
    DriveFlushWait,
    /// The busy part of one turn of a core's loop.
    CoreStep,
    /// A tick of a core: its periodic duties, with the step of its data that ends them.
    CoreTick,
    /// The result of a disk operation of a core's Raft log, with the entries it applies.
    CoreLogDone,
    /// The result of a disk operation of a core's data.
    CoreDataDone,
    /// A client's data request as its core takes it in.
    CoreRequest,
    /// A step of a core's data: its ranges' requests, duties and answers.
    CoreDataStep,
    /// A stretch of the background work of a core's LSM, until its next disk operation.
    CoreLsmWork,
    /// A transaction's `EndTxn(Begin)`.
    TxnBegin,
    /// A transaction's `PutIntents`.
    TxnIntents,
    /// A transaction's `LockKeys` or `UnlockKeys` (008 FR-022).
    TxnLock,
    /// A transaction's `CheckIntents` before its commit.
    TxnCheck,
    /// A transaction's `EndTxn(Commit)` or `EndTxn(Abort)`.
    TxnEnd,
    /// A transaction committed in one phase.
    TxnOnePhase,
    /// The resolution of a transaction's intents in one range.
    TxnResolve,
    /// A commit's wait for the clock to reach its mark.
    TxnClockWait,
    /// A statement of a transaction, from its arrival to its answer.
    TxnStatement,
    /// A transaction from its first request to its outcome.
    TxnTotal,
    /// A scan served on a connection's thread.
    ScanOutside,
    /// A scan the core served.
    ScanCore,
    /// A scan's wait on its connection's thread for a view that holds the writes answered
    /// before it began.
    ViewWait,
    /// A read's wait in its range for the node's wall clock to reach the mark it was given.
    ReadClockWait,
    /// A read's wait in its range for the core to confirm the leadership.
    ReadConfirm,
    /// A read's wait in its range for the writes in flight at or below its mark.
    ReadLatchWait,
    /// A disk operation's wait in the queue of its core's disk thread, or of its core's
    /// io_uring executor until it starts.
    DiskQueueWait,
    /// A sync on the io_uring executor, from its start to its completion.
    DiskSync,
    /// A message's wait in the mailbox of its core: a request, a disk result, another core's.
    MailboxWait,
    /// Entries of a Raft log from their append to the core's log until the core learns they are
    /// durable.
    LogAppendToDurable,
}

impl Stage {
    /// Every stage, in the order `/metrics` gives them.
    pub const ALL: [Stage; 31] = [
        Stage::WriteLatchWait,
        Stage::WriteProposeToApplied,
        Stage::WriteRange,
        Stage::DriveFlushWait,
        Stage::CoreStep,
        Stage::CoreTick,
        Stage::CoreLogDone,
        Stage::CoreDataDone,
        Stage::CoreRequest,
        Stage::CoreDataStep,
        Stage::CoreLsmWork,
        Stage::TxnBegin,
        Stage::TxnIntents,
        Stage::TxnLock,
        Stage::TxnCheck,
        Stage::TxnEnd,
        Stage::TxnOnePhase,
        Stage::TxnResolve,
        Stage::TxnClockWait,
        Stage::TxnStatement,
        Stage::TxnTotal,
        Stage::ScanOutside,
        Stage::ScanCore,
        Stage::ViewWait,
        Stage::ReadClockWait,
        Stage::ReadConfirm,
        Stage::ReadLatchWait,
        Stage::DiskQueueWait,
        Stage::DiskSync,
        Stage::MailboxWait,
        Stage::LogAppendToDurable,
    ];

    /// Its label in the metrics.
    pub fn label(self) -> &'static str {
        match self {
            Stage::WriteLatchWait => "write_latch_wait",
            Stage::WriteProposeToApplied => "write_propose_to_applied",
            Stage::WriteRange => "write_range",
            Stage::DriveFlushWait => "drive_flush_wait",
            Stage::CoreStep => "core_step",
            Stage::CoreTick => "core_tick",
            Stage::CoreLogDone => "core_log_done",
            Stage::CoreDataDone => "core_data_done",
            Stage::CoreRequest => "core_request",
            Stage::CoreDataStep => "core_data_step",
            Stage::CoreLsmWork => "core_lsm_work",
            Stage::TxnBegin => "txn_begin",
            Stage::TxnIntents => "txn_intents",
            Stage::TxnLock => "txn_lock",
            Stage::TxnCheck => "txn_check",
            Stage::TxnEnd => "txn_end",
            Stage::TxnOnePhase => "txn_one_phase",
            Stage::TxnResolve => "txn_resolve",
            Stage::TxnClockWait => "txn_clock_wait",
            Stage::TxnStatement => "txn_statement",
            Stage::TxnTotal => "txn_total",
            Stage::ScanOutside => "scan_outside",
            Stage::ScanCore => "scan_core",
            Stage::ViewWait => "view_wait",
            Stage::ReadClockWait => "read_clock_wait",
            Stage::ReadConfirm => "read_confirm",
            Stage::ReadLatchWait => "read_latch_wait",
            Stage::DiskQueueWait => "disk_queue_wait",
            Stage::DiskSync => "disk_sync",
            Stage::MailboxWait => "mailbox_wait",
            Stage::LogAppendToDurable => "log_append_to_durable",
        }
    }
}

/// The counters of one stage.
struct Counters {
    buckets: [AtomicU64; SLOTS],
    sum_nanos: AtomicU64,
}

impl Counters {
    const fn new() -> Counters {
        Counters { buckets: [const { AtomicU64::new(0) }; SLOTS], sum_nanos: AtomicU64::new(0) }
    }
}

static STAGES: [Counters; Stage::ALL.len()] = [const { Counters::new() }; Stage::ALL.len()];

/// The bucket of `nanos`: the first whose bound, 2ⁱ µs, holds it.
fn bucket(nanos: u64) -> usize {
    let micros = nanos.div_ceil(1_000);
    if micros <= 1 {
        return 0;
    }
    // ⌈log₂ micros⌉: a power of two stays in its own bucket.
    let i = (micros - 1).ilog2() as usize + 1;
    i.min(BOUNDS)
}

/// Counts a stage that took `nanos`.
pub fn record(stage: Stage, nanos: u64) {
    let c = &STAGES[stage as usize];
    c.buckets[bucket(nanos)].fetch_add(1, Ordering::Relaxed);
    c.sum_nanos.fetch_add(nanos, Ordering::Relaxed);
}

/// How many times `stage` was recorded.
pub fn count(stage: Stage) -> u64 {
    STAGES[stage as usize].buckets.iter().map(|b| b.load(Ordering::Relaxed)).sum()
}

/// `nanos` in seconds as Prometheus writes a number: `0.000001`, `1`, `33.554432`.
fn seconds(nanos: u64) -> String {
    let whole = nanos / 1_000_000_000;
    let fraction = nanos % 1_000_000_000;
    if fraction == 0 {
        return whole.to_string();
    }
    let digits = format!("{fraction:09}");
    format!("{whole}.{}", digits.trim_end_matches('0'))
}

/// Writes every stage's histogram to `out`, its buckets cumulative as Prometheus reads them.
pub fn render(out: &mut String) {
    let _ = writeln!(out, "# HELP {NAME} How long each stage of an operation took on this node.");
    let _ = writeln!(out, "# TYPE {NAME} histogram");
    for stage in Stage::ALL {
        let c = &STAGES[stage as usize];
        let label = stage.label();
        let mut total = 0u64;
        for i in 0..BOUNDS {
            total += c.buckets[i].load(Ordering::Relaxed);
            let le = seconds((1u64 << i) * 1_000);
            let _ = writeln!(out, "{NAME}_bucket{{stage=\"{label}\",le=\"{le}\"}} {total}");
        }
        total += c.buckets[BOUNDS].load(Ordering::Relaxed);
        let _ = writeln!(out, "{NAME}_bucket{{stage=\"{label}\",le=\"+Inf\"}} {total}");
        let sum = seconds(c.sum_nanos.load(Ordering::Relaxed));
        let _ = writeln!(out, "{NAME}_sum{{stage=\"{label}\"}} {sum}");
        let _ = writeln!(out, "{NAME}_count{{stage=\"{label}\"}} {total}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duration_falls_in_the_first_power_of_two_of_microseconds_that_holds_it() {
        assert_eq!(bucket(0), 0);
        assert_eq!(bucket(1_000), 0);
        assert_eq!(bucket(1_001), 1);
        assert_eq!(bucket(2_000), 1);
        assert_eq!(bucket(3_000), 2);
        assert_eq!(bucket(4_000), 2);
        assert_eq!(bucket(1_000_000), 10);
        assert_eq!(bucket((1 << 25) * 1_000), 25);
        assert_eq!(bucket((1 << 25) * 1_000 + 1_000), BOUNDS);
        assert_eq!(bucket(u64::MAX), BOUNDS);
    }

    #[test]
    fn every_stage_has_its_own_label_and_place() {
        for (i, s) in Stage::ALL.iter().enumerate() {
            assert_eq!(*s as usize, i);
        }
        let mut labels: Vec<&str> = Stage::ALL.iter().map(|s| s.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), Stage::ALL.len());
    }

    #[test]
    fn the_text_has_cumulative_buckets_a_sum_and_a_count_per_stage() {
        record(Stage::TxnClockWait, 1_500_000);
        record(Stage::TxnClockWait, 40_000_000_000);
        let mut out = String::new();
        render(&mut out);
        let line = |le: &str| {
            let key = format!("{NAME}_bucket{{stage=\"txn_clock_wait\",le=\"{le}\"}} ");
            out.lines().find_map(|l| l.strip_prefix(key.as_str())).map(str::to_owned)
        };
        let count = count(Stage::TxnClockWait);
        assert!(count >= 2);
        assert_eq!(line("0.001024").as_deref(), Some("0"));
        assert!(line("0.002048").is_some_and(|n| n.parse::<u64>().unwrap() >= 1));
        assert_eq!(line("+Inf"), Some(count.to_string()));
        assert!(out.contains("teseladb_stage_seconds_count{stage=\"txn_clock_wait\"}"));
        assert!(out.contains("teseladb_stage_seconds_sum{stage=\"txn_clock_wait\"} 4"));
        assert_eq!(out.matches("# TYPE").count(), 1);
    }

    #[test]
    fn seconds_are_written_without_trailing_zeros() {
        assert_eq!(seconds(0), "0");
        assert_eq!(seconds(1_000), "0.000001");
        assert_eq!(seconds(1_500_000_000), "1.5");
        assert_eq!(seconds(33_554_432_000), "33.554432");
    }
}
