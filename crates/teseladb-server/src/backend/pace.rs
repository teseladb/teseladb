//! How many documents of a filter's batch a worker runs before it looks whether its shard has
//! other work (023 FR-044).
//!
//! A filter is run a batch at a time, and between two batches the shard's other work runs when
//! the engine asks. A batch of the register machine takes microseconds; one whose filter costs
//! each document a script, or a long expression over a large document, takes as long as the
//! script times its documents, and the commands that share the shard advance one step for each
//! batch: a `killOp` sent to stop the read is served when the read is about to end. So a batch
//! that took longer than the engine's task quota makes the next ones shorter, down to one
//! document, and they grow back once they fit.

use std::time::{Duration, Instant};

use teseladb_io::engine::wait;
use teseladb_range::view::MATCH_BATCH;
use teseladb_wire::commands::Keep;

/// The engine's task quota: what a task runs before its shard's other work.
const QUOTA: Duration = Duration::from_micros(500);

/// The documents of a filter's batch a worker runs at a time: all of them ([`MATCH_BATCH`])
/// while they fit [`QUOTA`], fewer after a batch that did not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FilterPace(usize);

impl Default for FilterPace {
    fn default() -> FilterPace {
        FilterPace(MATCH_BATCH)
    }
}

impl FilterPace {
    /// The bits of the documents of `batch` that `keep` accepts (bit `i` for the document at
    /// `i`), run at most this pace at a time; before each run the shard's other work runs if
    /// the engine asks, and after it the pace is what fits the quota at the cost it measured.
    pub(super) fn run(&mut self, batch: &[&[u8]], keep: &mut Keep<'_>) -> u64 {
        let mut bits = 0;
        let mut at = 0;
        while at < batch.len() {
            wait::yield_if_asked();
            let n = self.0.min(batch.len() - at);
            let began = Instant::now();
            bits |= keep(&batch[at..at + n]) << at;
            *self = FilterPace::fitting(n, began.elapsed());
            at += n;
        }
        bits
    }

    /// The pace at which a run fits [`QUOTA`] when `n` documents took `took`.
    fn fitting(n: usize, took: Duration) -> FilterPace {
        let fit = QUOTA.as_nanos() * n as u128 / took.as_nanos().max(1);
        FilterPace(usize::try_from(fit).unwrap_or(MATCH_BATCH).clamp(1, MATCH_BATCH))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_within_the_quota_keeps_the_pace_and_a_longer_one_shortens_it() {
        let micros = Duration::from_micros;
        assert_eq!(FilterPace::fitting(64, micros(20)), FilterPace(64));
        assert_eq!(FilterPace::fitting(64, Duration::ZERO), FilterPace(64));
        // 50 µs for each document: ten fit the quota.
        assert_eq!(FilterPace::fitting(64, micros(3_200)), FilterPace(10));
        // A script of 17 ms for each document: one at a time.
        assert_eq!(FilterPace::fitting(64, Duration::from_millis(1_088)), FilterPace(1));
        assert_eq!(FilterPace::fitting(1, Duration::from_millis(17)), FilterPace(1));
        // The filter is cheap again: whole batches.
        assert_eq!(FilterPace::fitting(1, micros(2)), FilterPace(64));
    }

    #[test]
    fn a_slow_filter_is_run_a_document_at_a_time_and_gives_the_same_bits() {
        // The filter keeps the documents whose first byte is odd and costs each 2 ms.
        let docs: Vec<[u8; 1]> = (0..8).map(|i| [i]).collect();
        let batch: Vec<&[u8]> = docs.iter().map(<[u8; 1]>::as_slice).collect();
        let mut sizes = Vec::new();
        let mut keep = |b: &[&[u8]]| {
            sizes.push(b.len());
            std::thread::sleep(Duration::from_millis(2) * u32::try_from(b.len()).unwrap());
            b.iter().enumerate().fold(0, |bits, (i, d)| bits | (u64::from(d[0] & 1) << i))
        };
        let mut pace = FilterPace::default();
        // The first batch runs whole: nothing says yet what a document costs.
        assert_eq!(pace.run(&batch, &mut keep), 0b1010_1010);
        assert_eq!(pace, FilterPace(1));
        assert_eq!(pace.run(&batch, &mut keep), 0b1010_1010);
        assert_eq!(sizes, [8, 1, 1, 1, 1, 1, 1, 1, 1]);
    }
}
