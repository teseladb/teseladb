//! Bytes of the documents the aggregations running on this node read into memory (012 FR-016),
//! for `/metrics`. An aggregation runs on one connection's thread: its reads add what they take
//! to the node's count and to the thread's, and the [`Scope`] the command opens takes the
//! thread's part back off when the command ends.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

/// Bytes the running aggregations of the node hold.
static HELD: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// Bytes the aggregation of this thread holds.
    static MINE: Cell<u64> = const { Cell::new(0) };
}

/// Bytes the running aggregations of the node hold now.
pub fn held() -> u64 {
    HELD.load(Ordering::Relaxed)
}

/// Counts `bytes` of documents an aggregation of this thread read into memory.
pub fn hold(bytes: u64) {
    HELD.fetch_add(bytes, Ordering::Relaxed);
    MINE.with(|m| m.set(m.get().saturating_add(bytes)));
}

/// An aggregation command: when it ends, what its reads held stops counting.
#[derive(Debug)]
pub struct Scope(());

impl Scope {
    /// Opens the scope of an aggregation on this thread.
    pub fn open() -> Scope {
        Scope(())
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let mine = MINE.with(|m| m.replace(0));
        HELD.fetch_sub(mine, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_an_aggregation_reads_counts_until_its_command_ends() {
        let scope = Scope::open();
        hold(1_000);
        hold(24);
        assert!(held() >= 1_024);
        assert_eq!(MINE.with(Cell::get), 1_024);
        drop(scope);
        assert_eq!(MINE.with(Cell::get), 0);
    }
}
