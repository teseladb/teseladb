//! What the routing table says of a span of keys: the voters of the ranges that hold it, once
//! the table holds every one of them.

use teseladb_router::table::RouteEntry;

/// The fewest voters among the ranges that hold `[start, end)` (an empty `end`: no end), or
/// `None` while the table, still filling on a node that is starting, misses part of the span or
/// knows a range without voters.
pub(super) fn fewest_voters(entries: &[RouteEntry], start: &[u8], end: &[u8]) -> Option<usize> {
    let holds = |e: &RouteEntry, k: &[u8]| {
        e.start.as_slice() <= k && (e.end.is_empty() || k < e.end.as_slice())
    };
    let mut at = start.to_vec();
    let mut fewest: Option<usize> = None;
    loop {
        let e = entries.iter().find(|e| holds(e, &at))?;
        if e.voters.is_empty() {
            return None;
        }
        fewest = Some(fewest.map_or(e.voters.len(), |f| f.min(e.voters.len())));
        let last = e.end.is_empty() || (!end.is_empty() && end <= e.end.as_slice());
        if last {
            return fewest;
        }
        at.clone_from(&e.end);
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_types::{GroupId, NodeId};

    fn entry(start: &[u8], end: &[u8], voters: u64) -> RouteEntry {
        RouteEntry {
            group: GroupId(1),
            start: start.to_vec(),
            end: end.to_vec(),
            core: 0,
            voters: (1..=voters).map(NodeId).collect(),
            generation: 1,
            leader_hint: None,
        }
    }

    #[test]
    fn an_empty_table_knows_no_voters_yet() {
        assert_eq!(fewest_voters(&[], b"a", b"b"), None);
    }

    #[test]
    fn a_span_inside_one_range_has_its_voters() {
        assert_eq!(fewest_voters(&[entry(b"", b"", 3)], b"a", b"b"), Some(3));
        assert_eq!(fewest_voters(&[entry(b"a", b"c", 3)], b"a", b"c"), Some(3));
    }

    #[test]
    fn a_span_over_several_ranges_has_the_fewest() {
        let t = [entry(b"", b"b", 3), entry(b"b", b"d", 1), entry(b"d", b"", 5)];
        assert_eq!(fewest_voters(&t, b"a", b"c"), Some(1));
        assert_eq!(fewest_voters(&t, b"d", b""), Some(5));
        assert_eq!(fewest_voters(&t, b"a", b""), Some(1));
    }

    #[test]
    fn a_gap_or_a_range_without_voters_is_not_known_yet() {
        let gap = [entry(b"", b"b", 3), entry(b"c", b"", 3)];
        assert_eq!(fewest_voters(&gap, b"a", b"d"), None);
        assert_eq!(fewest_voters(&gap, b"c", b"d"), Some(3));
        assert_eq!(fewest_voters(&[entry(b"", b"", 0)], b"a", b"b"), None);
    }
}
