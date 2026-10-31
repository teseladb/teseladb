//! The terms of a group's log up to its snapshot, kept as runs `(first index, term)` and carried in
//! every snapshot file (005 FR-010, D-034). A replica that installs a leader's snapshot compares its
//! own entries below the snapshot's index with them: an entry whose term differs from the
//! committed log's term at its index was never committed and never will be, so it goes to the
//! rollback files; one whose term matches is the committed entry itself.

use teseladb_types::{DecodeError, Decoder, Encoder};

/// Terms of the indexes `1..=upto` of a group's log: each run starts at its first index and lasts
/// until the next one starts. A log's terms never go down, so there is one run per term.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TermRuns {
    upto: u64,
    runs: Vec<(u64, u64)>,
}

impl TermRuns {
    /// Term of `index`, if the runs cover it.
    pub(crate) fn term_of(&self, index: u64) -> Option<u64> {
        if index > self.upto {
            return None;
        }
        let after = self.runs.partition_point(|&(first, _)| first <= index);
        after.checked_sub(1).map(|k| self.runs[k].1)
    }

    /// Covers the indexes up to `to` with the terms `term` gives for `upto + 1..=to` (the log's
    /// entries above the runs), searching each change of term.
    pub(crate) fn extend(&mut self, to: u64, term: impl Fn(u64) -> u64) {
        let mut at = self.upto + 1;
        while at <= to {
            let t = term(at);
            // Last index of this term: the terms of a log never go down.
            let (mut lo, mut hi) = (at, to);
            while lo < hi {
                let mid = lo + (hi - lo).div_ceil(2);
                if term(mid) == t {
                    lo = mid;
                } else {
                    hi = mid - 1;
                }
            }
            if self.runs.last().is_none_or(|&(_, last)| last != t) {
                self.runs.push((at, t));
            }
            at = lo + 1;
        }
        self.upto = self.upto.max(to);
    }

    pub(crate) fn encode(&self, e: &mut Encoder) {
        e.var(self.upto).var(self.runs.len() as u64);
        for &(first, term) in &self.runs {
            e.var(first).var(term);
        }
    }

    /// Decodes runs, refusing ones out of order or past `upto`.
    pub(crate) fn decode(d: &mut Decoder<'_>) -> Result<TermRuns, DecodeError> {
        let upto = d.var()?;
        let n = d.var()?;
        // Each run takes at least two bytes: a count beyond what is left is a damaged file.
        if n > d.remaining() as u64 / 2 {
            return Err(DecodeError::InvalidValue { offset: d.position() });
        }
        let mut runs: Vec<(u64, u64)> = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let at = d.position();
            let (first, term) = (d.var()?, d.var()?);
            let ordered = runs.last().is_none_or(|&(f, t)| first > f && term > t);
            if !ordered || first == 0 || first > upto {
                return Err(DecodeError::InvalidValue { offset: at });
            }
            runs.push((first, term));
        }
        Ok(TermRuns { upto, runs })
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// A log whose entry `i` (from 1) has term `terms[i - 1]`.
    fn log(terms: &[u64]) -> impl Fn(u64) -> u64 + '_ {
        |i| terms[i as usize - 1]
    }

    #[test]
    fn runs_give_the_term_of_every_covered_index() {
        let terms = [1, 1, 2, 2, 2, 5, 7, 7];
        let mut r = TermRuns::default();
        r.extend(5, log(&terms));
        r.extend(8, log(&terms));
        r.extend(8, log(&terms));
        assert_eq!(r.runs, vec![(1, 1), (3, 2), (6, 5), (7, 7)]);
        for (i, t) in terms.iter().enumerate() {
            assert_eq!(r.term_of(i as u64 + 1), Some(*t));
        }
        assert_eq!(r.term_of(0), None);
        assert_eq!(r.term_of(9), None);
        assert_eq!(r.upto, 8);
    }

    #[test]
    fn runs_round_trip_and_damaged_ones_are_refused() {
        let mut r = TermRuns::default();
        r.extend(6, log(&[1, 2, 2, 4, 4, 9]));
        let mut e = Encoder::new();
        r.encode(&mut e);
        let b = e.finish();
        let mut d = Decoder::new(&b);
        assert_eq!(TermRuns::decode(&mut d), Ok(r));
        let bad = |runs: &[u64], upto: u64| {
            let mut e = Encoder::new();
            e.var(upto).var(runs.len() as u64 / 2);
            for v in runs {
                e.var(*v);
            }
            let b = e.finish();
            TermRuns::decode(&mut Decoder::new(&b)).is_err()
        };
        assert!(bad(&[0, 1], 5), "index 0");
        assert!(bad(&[3, 1, 2, 2], 5), "indexes going down");
        assert!(bad(&[1, 2, 3, 2], 5), "terms not going up");
        assert!(bad(&[6, 1], 5), "past upto");
        assert!(!bad(&[1, 1, 3, 2], 5));
        let mut e = Encoder::new();
        e.var(5).var(1_000);
        let b = e.finish();
        assert!(TermRuns::decode(&mut Decoder::new(&b)).is_err(), "count beyond the bytes");
    }
}
