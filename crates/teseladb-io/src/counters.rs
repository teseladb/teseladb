//! Counters of the node's real I/O that the engine does not keep, for its metrics: the syncs of the
//! device log and the links to other nodes that went down. The disk's syncs are the engine's
//! (`fsyncs` of its reactor, 023 FR-042).

use std::sync::atomic::{AtomicU64, Ordering};

pub use teseladb_devlog::state::LogStats;

/// A counter on a cache line of its own (012 FR-031): the cores that bump one do not pull the
/// line of another back and forth.
#[repr(align(64))]
struct Line(AtomicU64);

static LOG_SYNCS: Line = Line(AtomicU64::new(0));
static PEER_LINKS_FAILED: Line = Line(AtomicU64::new(0));

/// Counts an outbound link to another node that went down (012 FR-015): what was queued on it
/// is dropped and waits for the next connection.
pub(crate) fn record_peer_link_failed() {
    PEER_LINKS_FAILED.0.fetch_add(1, Ordering::Relaxed);
}

/// The outbound links to other nodes that went down since the process started.
pub fn peer_links_failed() -> u64 {
    PEER_LINKS_FAILED.0.load(Ordering::Relaxed)
}

/// Counts a sync of the node's Raft log (012 FR-035): one for every core's writes of a turn.
pub(crate) fn record_log_sync() {
    LOG_SYNCS.0.fetch_add(1, Ordering::Relaxed);
}

/// The syncs of the Raft logs since the process started.
pub fn log_syncs() -> u64 {
    LOG_SYNCS.0.load(Ordering::Relaxed)
}

/// What the device logs wrote ([`LogStats`]), one counter per field in its order: only the
/// writers bump them, together, so they share a line.
#[repr(align(64))]
struct LogLines([AtomicU64; LOG_FIELDS]);

const LOG_FIELDS: usize = 9;

static LOG_WRITES: LogLines = LogLines([const { AtomicU64::new(0) }; LOG_FIELDS]);

fn log_fields(s: &LogStats) -> [u64; LOG_FIELDS] {
    [
        s.records,
        s.record_bytes,
        s.padding_bytes,
        s.filled_bytes,
        s.copied_bytes,
        s.copy_read_bytes,
        s.files_made,
        s.files_made_waiting,
        s.synced_requests,
    ]
}

/// Adds what a device log wrote between two of its [`LogStats`], `before` and `now`.
pub(crate) fn record_log_stats(before: &LogStats, now: &LogStats) {
    for ((line, b), n) in LOG_WRITES.0.iter().zip(log_fields(before)).zip(log_fields(now)) {
        if n > b {
            line.fetch_add(n - b, Ordering::Relaxed);
        }
    }
}

/// What the node's device logs wrote since the process started.
pub fn log_stats() -> LogStats {
    let v: Vec<u64> = LOG_WRITES.0.iter().map(|a| a.load(Ordering::Relaxed)).collect();
    LogStats {
        records: v[0],
        record_bytes: v[1],
        padding_bytes: v[2],
        filled_bytes: v[3],
        copied_bytes: v[4],
        copy_read_bytes: v[5],
        files_made: v[6],
        files_made_waiting: v[7],
        synced_requests: v[8],
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #[test]
    fn the_log_stats_add_up_what_each_writer_wrote() {
        let start = super::log_stats();
        let zero = super::LogStats::default();
        let one = super::LogStats { records: 2, padding_bytes: 100, synced_requests: 5, ..zero };
        let two = super::LogStats { records: 3, padding_bytes: 150, synced_requests: 9, ..one };
        super::record_log_stats(&zero, &one);
        super::record_log_stats(&one, &two);
        let now = super::log_stats();
        assert!(now.records >= start.records + 3);
        assert!(now.padding_bytes >= start.padding_bytes + 150);
        assert!(now.synced_requests >= start.synced_requests + 9);
    }

    #[test]
    fn each_counter_has_its_own_cache_line() {
        assert_eq!(std::mem::align_of::<super::Line>(), 64);
        let a = std::ptr::addr_of!(super::LOG_SYNCS) as usize;
        let b = std::ptr::addr_of!(super::PEER_LINKS_FAILED) as usize;
        assert_eq!(a % 64, 0);
        assert!(a.abs_diff(b) >= 64);
    }

    #[test]
    fn the_counters_only_go_up() {
        let (s, l) = (super::log_syncs(), super::peer_links_failed());
        super::record_log_sync();
        super::record_peer_link_failed();
        assert!(super::log_syncs() > s);
        assert!(super::peer_links_failed() > l);
    }
}
