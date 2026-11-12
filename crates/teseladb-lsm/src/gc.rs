//! Garbage collection of versions in compaction (FR-007, FR-008; data-model §4).
//!
//! 005 hands each range's GC threshold: reads below it are not allowed any more, so of the
//! versions of a key at or below the threshold only the newest one still answers a read. For
//! each key, with its versions from the newest:
//! - versions above the threshold stay;
//! - intents stay: the LSM cannot know whether their transaction is resolved, and a commit reads
//!   the value from its intent. A clearing (007) is the exception: its tombstone, above every
//!   version, empties the interval for a snapshot that brings the intents still alive, so the
//!   intents it hides and their `Retract`s go;
//! - at or below the threshold, the newest version that is not an intent stays, unless a range
//!   tombstone at or below the threshold hides it, or it is a deletion and nothing older is left
//!   below the output (bottommost); every older one goes;
//! - a version that only serves the history ([`crate::HISTORY_VALUE`]) goes at or below the
//!   threshold, also the newest: it is read only at its own mark, above the threshold;
//! - the intent a `Retract` hides goes when they meet; the `Retract` stays until the bottommost
//!   level, because an older version with the same timestamp (one the intent replaced) may still
//!   be below and must stay hidden.

use teseladb_keys::Hlc;

use crate::entry::{Kind, RangeTombstone};
use crate::lsm::CLEARED;

/// GC thresholds by key range (005 gives them per range).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcTable {
    ranges: Vec<(Vec<u8>, Vec<u8>, Hlc)>,
}

impl GcTable {
    /// A table from `(start, end, threshold)` entries over disjoint ranges.
    pub fn new(mut ranges: Vec<(Vec<u8>, Vec<u8>, Hlc)>) -> GcTable {
        ranges.sort_by(|a, b| a.0.cmp(&b.0));
        GcTable { ranges }
    }

    /// This table with `ts` as the threshold of `intervals`: the entries they overlap keep their
    /// threshold over the parts outside them.
    pub(crate) fn with_intervals(&self, intervals: &[(Vec<u8>, Vec<u8>)], ts: Hlc) -> GcTable {
        let mut ranges: Vec<_> = self
            .ranges
            .iter()
            .flat_map(|(s, e, t)| outside(s, e, intervals).into_iter().map(|(s, e)| (s, e, *t)))
            .collect();
        ranges.extend(
            intervals.iter().filter(|(s, e)| s < e).map(|(s, e)| (s.clone(), e.clone(), ts)),
        );
        GcTable::new(ranges)
    }

    /// The lowest threshold of the table, if it has any.
    pub fn lowest(&self) -> Option<Hlc> {
        self.ranges.iter().map(|(_, _, t)| *t).min()
    }

    /// The highest threshold of the table, if it has any.
    pub fn highest(&self) -> Option<Hlc> {
        self.ranges.iter().map(|(_, _, t)| *t).max()
    }

    /// The threshold of `key`, if its range has one.
    pub fn threshold(&self, key: &[u8]) -> Option<Hlc> {
        let i = self.ranges.partition_point(|(start, _, _)| start.as_slice() <= key);
        let (_, end, t) = self.ranges.get(i.checked_sub(1)?)?;
        (key < end.as_slice()).then_some(*t)
    }

    /// The threshold that holds over all of `[start, end)`: the lowest of the ranges that cover
    /// it, or `None` if some part has no threshold.
    pub fn threshold_over(&self, start: &[u8], end: &[u8]) -> Option<Hlc> {
        let mut at = start.to_vec();
        let mut lowest: Option<Hlc> = None;
        while at.as_slice() < end {
            let i = self.ranges.partition_point(|(s, _, _)| s.as_slice() <= at.as_slice());
            let (_, e, t) = self.ranges.get(i.checked_sub(1)?)?;
            if at.as_slice() >= e.as_slice() {
                return None;
            }
            lowest = Some(lowest.map_or(*t, |l| l.min(*t)));
            at = e.clone();
        }
        lowest
    }
}

/// The parts of `[start, end)` outside every interval of `cuts`, in order.
pub(crate) fn outside(
    start: &[u8],
    end: &[u8],
    cuts: &[(Vec<u8>, Vec<u8>)],
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut parts = vec![(start.to_vec(), end.to_vec())];
    for (cs, ce) in cuts.iter().filter(|(cs, ce)| cs < ce) {
        parts = parts
            .into_iter()
            .flat_map(|(s, e)| {
                let before = (s.clone(), e.as_slice().min(cs.as_slice()).to_vec());
                let after = (s.as_slice().max(ce.as_slice()).to_vec(), e);
                [before, after]
            })
            .filter(|(s, e)| s < e)
            .collect();
    }
    parts
}

/// One version of a key in a compaction, once copies of the same timestamp are merged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) ts: Hlc,
    pub(crate) kind: Kind,
    /// An older input had a copy at the same timestamp (for a `Retract`: its intent).
    pub(crate) shadowed: bool,
    /// A value that only serves the history ([`crate::HISTORY_VALUE`]).
    pub(crate) history: bool,
}

impl Candidate {
    /// A version of kind `kind` at `ts` with stored value `value`.
    pub(crate) fn new(ts: Hlc, kind: Kind, value: &[u8]) -> Candidate {
        let history = kind == Kind::Put && value.first() == Some(&crate::HISTORY_VALUE);
        Candidate { ts, kind, shadowed: false, history }
    }
}

/// Which of a key's versions (newest first) stay.
pub(crate) fn keep(
    key: &[u8],
    versions: &[Candidate],
    threshold: Option<Hlc>,
    tombstones: &[RangeTombstone],
    bottommost: bool,
) -> Vec<bool> {
    let mut answered_below = false;
    let cleared = |ts: Hlc| {
        threshold == Some(CLEARED)
            && tombstones.iter().any(|t| t.ts == CLEARED && t.hides(key, ts, CLEARED))
    };
    versions
        .iter()
        .map(|v| match v.kind {
            Kind::Retract => !bottommost && !cleared(v.ts),
            Kind::Intent => !cleared(v.ts),
            Kind::Put | Kind::Delete => {
                let Some(g) = threshold.filter(|g| v.ts <= *g) else {
                    return true;
                };
                if v.history {
                    return false;
                }
                if answered_below {
                    return false;
                }
                answered_below = true;
                let hidden = tombstones.iter().any(|t| t.ts <= g && t.hides(key, v.ts, g));
                !hidden && !(v.kind == Kind::Delete && bottommost)
            }
        })
        .collect()
}

/// Whether a range tombstone can go: at the bottommost level, at or below the threshold of every
/// key it covers. A clearing's tombstone (at [`CLEARED`]) hides nothing from reads; at the
/// bottommost level it goes even when its clearing is no longer in memory (a restart before the
/// snapshot landed): the replica clears its bounds again with a tombstone of its own.
pub(crate) fn drop_tombstone(t: &RangeTombstone, gc: &GcTable, bottommost: bool) -> bool {
    bottommost
        && (t.ts == CLEARED || gc.threshold_over(&t.start, &t.end).is_some_and(|g| t.ts <= g))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn ts(p: u64) -> Hlc {
        Hlc { physical: p, logical: 0 }
    }

    fn c(p: u64, kind: Kind) -> Candidate {
        Candidate { ts: ts(p), kind, shadowed: false, history: false }
    }

    #[test]
    fn thresholds_by_range() {
        let gc = GcTable::new(vec![
            (b"m".to_vec(), b"t".to_vec(), ts(50)),
            (b"a".to_vec(), b"f".to_vec(), ts(10)),
            (b"f".to_vec(), b"m".to_vec(), ts(30)),
        ]);
        assert_eq!(gc.threshold(b"a"), Some(ts(10)));
        assert_eq!(gc.threshold(b"ezz"), Some(ts(10)));
        assert_eq!(gc.threshold(b"f"), Some(ts(30)));
        assert_eq!(gc.threshold(b"t"), None);
        assert_eq!(gc.threshold(b"0"), None);
        assert_eq!(gc.threshold_over(b"b", b"n"), Some(ts(10)));
        assert_eq!(gc.threshold_over(b"g", b"s"), Some(ts(30)));
        assert_eq!(gc.threshold_over(b"g", b"z"), None, "past the last range");
        assert_eq!(gc.threshold_over(b"0", b"b"), None, "before the first range");
        let gap = GcTable::new(vec![
            (b"a".to_vec(), b"c".to_vec(), ts(5)),
            (b"d".to_vec(), b"f".to_vec(), ts(5)),
        ]);
        assert_eq!(gap.threshold_over(b"a", b"e"), None, "a hole in the middle");
        assert_eq!(gap.threshold_over(b"b", b"b"), None, "empty");
    }

    fn iv(s: &[u8], e: &[u8]) -> (Vec<u8>, Vec<u8>) {
        (s.to_vec(), e.to_vec())
    }

    #[test]
    fn the_parts_outside_some_intervals() {
        assert_eq!(outside(b"c", b"m", &[]), vec![iv(b"c", b"m")]);
        assert_eq!(outside(b"c", b"m", &[iv(b"e", b"g")]), vec![iv(b"c", b"e"), iv(b"g", b"m")]);
        assert_eq!(outside(b"c", b"m", &[iv(b"a", b"e"), iv(b"k", b"z")]), vec![iv(b"e", b"k")]);
        assert_eq!(outside(b"c", b"m", &[iv(b"a", b"z")]), vec![], "covered");
        assert_eq!(outside(b"c", b"m", &[iv(b"n", b"z")]), vec![iv(b"c", b"m")], "apart");
        assert_eq!(outside(b"c", b"m", &[iv(b"g", b"e")]), vec![iv(b"c", b"m")], "an empty cut");
    }

    #[test]
    fn new_intervals_trim_the_entries_they_overlap() {
        let gc = GcTable::new(vec![
            (b"a".to_vec(), b"f".to_vec(), ts(10)),
            (b"f".to_vec(), b"z".to_vec(), ts(20)),
        ]);
        let set = gc.with_intervals(&[iv(b"d", b"h"), iv(b"q", b"q")], ts(5));
        let expected = GcTable::new(vec![
            (b"a".to_vec(), b"d".to_vec(), ts(10)),
            (b"d".to_vec(), b"h".to_vec(), ts(5)),
            (b"h".to_vec(), b"z".to_vec(), ts(20)),
        ]);
        assert_eq!(set, expected, "the empty interval adds nothing");
    }

    #[test]
    fn keeps_the_newest_version_at_or_below_the_threshold() {
        let v = [c(120, Kind::Put), c(100, Kind::Put), c(90, Kind::Put), c(80, Kind::Delete)];
        assert_eq!(keep(b"k", &v, Some(ts(100)), &[], false), vec![true, true, false, false]);
        // The newest at or below 95 is the one at 90.
        assert_eq!(keep(b"k", &v, Some(ts(95)), &[], false), vec![true, true, true, false]);
        // The newest at or below 85 is the deletion at 80, kept above the last level.
        assert_eq!(keep(b"k", &v, Some(ts(85)), &[], false), vec![true; 4]);
        assert_eq!(keep(b"k", &v, None, &[], true), vec![true; 4], "no threshold, no GC");
    }

    #[test]
    fn a_deletion_goes_only_where_nothing_older_is_left() {
        let v = [c(100, Kind::Delete), c(90, Kind::Put)];
        assert_eq!(keep(b"k", &v, Some(ts(100)), &[], false), vec![true, false]);
        assert_eq!(keep(b"k", &v, Some(ts(100)), &[], true), vec![false, false]);
        assert_eq!(
            keep(b"k", &v, Some(ts(99)), &[], true),
            vec![true, true],
            "above the threshold"
        );
    }

    #[test]
    fn intents_always_stay_and_retracts_go_with_their_intent() {
        let v = [c(100, Kind::Intent), c(90, Kind::Put), c(80, Kind::Put)];
        assert_eq!(keep(b"k", &v, Some(ts(200)), &[], true), vec![true, true, false]);
        // Having met its intent, it still hides older versions of that timestamp below.
        let met = Candidate { shadowed: true, ..c(100, Kind::Retract) };
        assert_eq!(keep(b"k", std::slice::from_ref(&met), Some(ts(200)), &[], false), vec![true]);
        assert_eq!(keep(b"k", &[met], Some(ts(200)), &[], true), vec![false]);
        let alone = c(100, Kind::Retract);
        assert_eq!(keep(b"k", std::slice::from_ref(&alone), None, &[], false), vec![true]);
        assert_eq!(keep(b"k", &[alone], None, &[], true), vec![false]);
    }

    /// A clearing (its tombstone and threshold above every version) empties its intervals for a
    /// snapshot that brings the intents still alive: the old ones and their retractions go too,
    /// or the snapshot's ingestion finds them and never lands.
    #[test]
    fn a_clearing_drops_intents_and_retractions() {
        let top = Hlc { physical: u64::MAX, logical: u32::MAX };
        let t = RangeTombstone { start: b"a".to_vec(), end: b"z".to_vec(), ts: top };
        let met = Candidate { shadowed: true, ..c(100, Kind::Retract) };
        let v = [c(120, Kind::Intent), met, c(60, Kind::Put)];
        let cleared = std::slice::from_ref(&t);
        assert_eq!(keep(b"k", &v, Some(top), cleared, false), vec![false, false, false]);
        assert_eq!(
            keep(b"zz", &v, Some(top), cleared, false),
            vec![true, true, true],
            "outside it"
        );
        // Under an ordinary tombstone an intent stays: its resolution reads the value from it.
        let ordinary = RangeTombstone { ts: ts(130), ..t };
        assert_eq!(keep(b"k", &v, Some(ts(200)), &[ordinary], false), vec![true, true, false]);
    }

    /// A clearing's tombstone outlives the clearing when the node restarts before the snapshot
    /// lands (the clearings are not on disk): over the range's new bounds the replica clears again,
    /// and the old one, which hides nothing from reads, would keep every later ingestion over it
    /// from landing. At the bottommost level it goes whatever the thresholds.
    #[test]
    fn a_clearing_tombstone_goes_at_the_bottom_even_without_its_clearing() {
        let top = Hlc { physical: u64::MAX, logical: u32::MAX };
        let t = RangeTombstone { start: b"a".to_vec(), end: b"z".to_vec(), ts: top };
        let gc = GcTable::new(vec![(b"a".to_vec(), b"zz".to_vec(), ts(100))]);
        assert!(drop_tombstone(&t, &gc, true));
        assert!(drop_tombstone(&t, &GcTable::default(), true));
        assert!(!drop_tombstone(&t, &gc, false));
    }

    #[test]
    fn range_tombstones_at_or_below_the_threshold_drop_what_they_hide() {
        let t = |p| RangeTombstone { start: b"a".to_vec(), end: b"z".to_vec(), ts: ts(p) };
        let v = [c(120, Kind::Put), c(60, Kind::Put), c(40, Kind::Intent)];
        assert_eq!(keep(b"k", &v, Some(ts(100)), &[t(70)], false), vec![true, false, true]);
        assert_eq!(
            keep(b"k", &v, Some(ts(100)), &[t(110)], false),
            vec![true, true, true],
            "above the threshold"
        );
        assert_eq!(
            keep(b"zz", &v, Some(ts(100)), &[t(70)], false),
            vec![true, true, true],
            "outside it"
        );
        let gc = GcTable::new(vec![(b"a".to_vec(), b"zz".to_vec(), ts(100))]);
        assert!(drop_tombstone(&t(70), &gc, true));
        assert!(!drop_tombstone(&t(70), &gc, false));
        assert!(!drop_tombstone(&t(110), &gc, true));
        assert!(!drop_tombstone(&t(70), &GcTable::default(), true));
    }
}
