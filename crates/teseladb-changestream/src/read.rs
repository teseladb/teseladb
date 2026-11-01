//! A cursor's read of an interval (014 FR-004, FR-005, FR-010): range by range in key order, each
//! read stopping at the end of the range the table names for its first key, so a split or a merge
//! between two reads changes only where the next one stops, and no key is read twice or skipped.
//! The rows go up to the lowest resolved mark of the ranges: above it a range may still give a
//! version below another range's.

use teseladb_hlc::Hlc;

/// A read of `[start, end)` after mark `from`, with rows of type `R`.
#[derive(Clone, Debug)]
pub struct IntervalRead<R> {
    end: Vec<u8>,
    at: Vec<u8>,
    from: Hlc,
    rows: Vec<R>,
    resolved: Option<Hlc>,
}

impl<R> IntervalRead<R> {
    /// The read of `[start, end)` after `from`.
    pub fn new(start: &[u8], end: &[u8], from: Hlc) -> IntervalRead<R> {
        IntervalRead {
            end: end.to_vec(),
            at: start.to_vec(),
            from,
            rows: Vec::new(),
            resolved: None,
        }
    }

    /// The first key of the next range to read (`None`: the interval is read).
    pub fn next(&self) -> Option<&[u8]> {
        (self.at < self.end).then_some(self.at.as_slice())
    }

    /// Where the read of the range that ends at `range_end` stops (empty: the end of the space).
    pub fn stop(&self, range_end: &[u8]) -> Vec<u8> {
        if range_end.is_empty() || self.end.as_slice() <= range_end {
            self.end.clone()
        } else {
            range_end.to_vec()
        }
    }

    /// The range read up to `stop` gave `rows`, resolved up to `to` with its oldest pending intent
    /// at `oldest_intent`.
    pub fn answered(&mut self, stop: Vec<u8>, rows: Vec<R>, to: Hlc, oldest_intent: Option<Hlc>) {
        let mark = crate::resolved::resolved(to, oldest_intent);
        self.resolved = Some(self.resolved.map_or(mark, |r| r.min(mark)));
        self.rows.extend(rows);
        self.at = stop;
    }

    /// The rows at or below the resolved mark of the read, `ts_of` giving a row's mark, and that
    /// mark (never below the start).
    pub fn finish(self, ts_of: impl Fn(&R) -> Hlc) -> (Vec<R>, Hlc) {
        let resolved = self.resolved.unwrap_or(self.from).max(self.from);
        let mut rows = self.rows;
        rows.retain(|r| ts_of(r) <= resolved);
        (rows, resolved)
    }
}
