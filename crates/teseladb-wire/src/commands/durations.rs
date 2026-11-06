//! How long the clients' commands took on this node (007 FR-038), as the Prometheus histogram
//! `teseladb_command_duration_seconds` of `/metrics`. The counts are the process's: every core
//! adds to them, and the administration port reads them without asking the cores.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

/// The name of the histogram.
pub const NAME: &str = "teseladb_command_duration_seconds";
/// The upper bounds of the buckets, in microseconds: 1 ms, 5 ms, 10 ms, 50 ms, 100 ms, 500 ms, 1 s
/// and 5 s; one more bucket holds what is longer.
pub const BUCKETS_MICROS: [u64; 8] =
    [1_000, 5_000, 10_000, 50_000, 100_000, 500_000, 1_000_000, 5_000_000];
/// The buckets with the one past the largest bound.
const SLOTS: usize = BUCKETS_MICROS.len() + 1;

/// The commands of each bucket, not cumulative; the last one is past the largest bound.
static COUNTS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
/// The microseconds of every command.
static SUM_MICROS: AtomicU64 = AtomicU64::new(0);

/// The bucket of a command that took `micros`.
fn bucket(micros: u64) -> usize {
    BUCKETS_MICROS.iter().position(|b| micros <= *b).unwrap_or(BUCKETS_MICROS.len())
}

/// Counts a command that took `micros`.
pub fn record(micros: u64) {
    COUNTS[bucket(micros)].fetch_add(1, Ordering::Relaxed);
    SUM_MICROS.fetch_add(micros, Ordering::Relaxed);
}

/// `micros` in seconds as Prometheus writes a number: `0.001`, `1`, `5`.
fn seconds(micros: u64) -> String {
    let whole = micros / 1_000_000;
    let fraction = micros % 1_000_000;
    if fraction == 0 {
        return whole.to_string();
    }
    let digits = format!("{fraction:06}");
    format!("{whole}.{}", digits.trim_end_matches('0'))
}

/// Writes the histogram to `out`, its buckets cumulative as Prometheus reads them.
pub fn render(out: &mut String) {
    let _ = writeln!(out, "# HELP {NAME} How long the clients' commands took on this node.");
    let _ = writeln!(out, "# TYPE {NAME} histogram");
    let mut total = 0u64;
    for (i, bound) in BUCKETS_MICROS.iter().enumerate() {
        total += COUNTS[i].load(Ordering::Relaxed);
        let _ = writeln!(out, "{NAME}_bucket{{le=\"{}\"}} {total}", seconds(*bound));
    }
    total += COUNTS[BUCKETS_MICROS.len()].load(Ordering::Relaxed);
    let _ = writeln!(out, "{NAME}_bucket{{le=\"+Inf\"}} {total}");
    let sum = SUM_MICROS.load(Ordering::Relaxed);
    let _ = writeln!(out, "{NAME}_sum {}", seconds(sum));
    let _ = writeln!(out, "{NAME}_count {total}");
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn each_duration_falls_in_its_bucket_and_the_text_is_cumulative() {
        assert_eq!(bucket(0), 0);
        assert_eq!(bucket(1_000), 0);
        assert_eq!(bucket(1_001), 1);
        assert_eq!(bucket(5_000_001), BUCKETS_MICROS.len());
        assert_eq!(seconds(1_000), "0.001");
        assert_eq!(seconds(5_000_000), "5");
        assert_eq!(seconds(1_250_000), "1.25");
        record(2_000);
        record(9_000_000);
        let mut out = String::new();
        render(&mut out);
        assert!(out.contains("# TYPE teseladb_command_duration_seconds histogram"), "{out}");
        assert!(out.contains("teseladb_command_duration_seconds_bucket{le=\"+Inf\"} "), "{out}");
        let count: u64 = out
            .lines()
            .find_map(|l| l.strip_prefix("teseladb_command_duration_seconds_count ")?.parse().ok())
            .unwrap_or_default();
        assert!(count >= 2, "{out}");
    }
}
