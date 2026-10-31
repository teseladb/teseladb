//! Range tombstones across the files of a compaction (FR-008).
//!
//! A tombstone lives in the memtable and in the level 0 file of its flush whole. When a
//! compaction writes several files into a level, each file takes the piece of every surviving
//! tombstone that falls in its span: from its first key to the first key of the next file (the
//! first file from the start of the key space, the last one to its end). So the files of a level
//! keep apart, and every key still finds the tombstones that cover it in the file that holds it.

use crate::entry::RangeTombstone;

/// The piece of `t` inside `[lo, hi)` (`None` for an open end), if any.
pub(crate) fn piece(
    t: &RangeTombstone,
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
) -> Option<RangeTombstone> {
    let start = match lo {
        Some(l) if l > t.start.as_slice() => l.to_vec(),
        _ => t.start.clone(),
    };
    let end = match hi {
        Some(h) if h < t.end.as_slice() => h.to_vec(),
        _ => t.end.clone(),
    };
    (start < end).then_some(RangeTombstone { start, end, ts: t.ts })
}

/// The pieces of `tombstones` inside `[lo, hi)`.
pub(crate) fn pieces(
    tombstones: &[RangeTombstone],
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
) -> Vec<RangeTombstone> {
    tombstones.iter().filter_map(|t| piece(t, lo, hi)).collect()
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use teseladb_keys::Hlc;

    use super::*;

    fn t(s: &str, e: &str) -> RangeTombstone {
        RangeTombstone { start: s.into(), end: e.into(), ts: Hlc { physical: 7, logical: 0 } }
    }

    #[test]
    fn pieces_cover_the_tombstone_exactly_once() {
        let whole = t("c", "p");
        let spans: [(Option<&[u8]>, Option<&[u8]>); 4] = [
            (None, Some(b"e")),
            (Some(b"e"), Some(b"k")),
            (Some(b"k"), Some(b"x")),
            (Some(b"x"), None),
        ];
        let got: Vec<Option<RangeTombstone>> =
            spans.iter().map(|(lo, hi)| piece(&whole, *lo, *hi)).collect();
        assert_eq!(got, vec![Some(t("c", "e")), Some(t("e", "k")), Some(t("k", "p")), None]);
        assert_eq!(piece(&whole, None, None), Some(whole.clone()));
        assert_eq!(pieces(&[whole.clone(), t("a", "b")], Some(b"d"), None), vec![t("d", "p")]);
    }
}
