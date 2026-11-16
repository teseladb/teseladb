//! Counters of the node's real I/O that the engine does not keep, for its metrics: the syncs of the
//! device log and the links to other nodes that went down. The disk's syncs are the engine's
//! (`fsyncs` of its reactor, 023 FR-042).

use std::sync::atomic::{AtomicU64, Ordering};

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

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
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
