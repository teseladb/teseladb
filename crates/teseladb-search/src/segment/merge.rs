//! Merging segments (FR-012): a size-tiered policy picks which segments to merge, and a merge
//! writes one segment with every document of its inputs except those deleted at or below the GC
//! threshold, which no read can see any more.
//!
//! Tiers grow by [`TIER_FACTOR`] from [`TIER_FLOOR`] bytes; a tier with [`MERGE_WIDTH`] segments
//! merges its smallest ones. The indexer runs the merge within its I/O budget; deletions above the
//! threshold move to the new segment's deletion map with the new document numbers.

use std::collections::BTreeMap;

use teseladb_keys::Hlc;

use crate::segment::data::{FieldData, SegmentData, VectorData};

/// Size of the smallest tier: smaller segments count as this size.
pub const TIER_FLOOR: u64 = 1 << 20;
/// Size ratio between tiers.
pub const TIER_FACTOR: u64 = 4;
/// Segments of one tier merged at once.
pub const MERGE_WIDTH: usize = 8;

fn tier(bytes: u64) -> u32 {
    let mut t = 0;
    let mut top = TIER_FLOOR;
    while bytes >= top.saturating_mul(TIER_FACTOR) && top < u64::MAX / TIER_FACTOR {
        top *= TIER_FACTOR;
        t += 1;
    }
    t
}

/// The segments to merge among `(number, bytes)` of one range and index: the [`MERGE_WIDTH`]
/// smallest of the lowest tier that has that many, or `None`.
pub fn pick(segments: &[(u64, u64)]) -> Option<Vec<u64>> {
    let mut tiers: BTreeMap<u32, Vec<(u64, u64)>> = BTreeMap::new();
    for &(number, bytes) in segments {
        tiers.entry(tier(bytes)).or_default().push((bytes, number));
    }
    let (_, mut members) = tiers.into_iter().find(|(_, m)| m.len() >= MERGE_WIDTH)?;
    members.sort_unstable();
    Some(members.into_iter().take(MERGE_WIDTH).map(|(_, n)| n).collect())
}

/// Merges `inputs` (each with its deletion map), in order, into one segment with its deletion
/// map. Documents deleted at or below `gc_threshold` go.
pub fn merge(
    inputs: &[(SegmentData, BTreeMap<u32, Hlc>)],
    gc_threshold: Hlc,
) -> (SegmentData, BTreeMap<u32, Hlc>) {
    let first = &inputs.first().expect("a merge has inputs").0;
    assert!(
        inputs.iter().all(|(s, _)| s.range == first.range
            && s.index == first.index
            && s.analyzer_version == first.analyzer_version),
        "merged segments share range, index and analyzer version"
    );
    let mut out = SegmentData::new(first.range, &first.index, first.analyzer_version);
    out.applied_index = inputs.iter().map(|(s, _)| s.applied_index).max().unwrap_or(0);
    let mut deletes = BTreeMap::new();
    for (seg, dels) in inputs {
        // New number of each kept document.
        let base = out.docs.len();
        let mut renumber = vec![None; seg.docs.len()];
        for (old, doc) in seg.docs.iter().enumerate() {
            let del = dels.get(&(old as u32));
            if del.is_some_and(|ts| *ts <= gc_threshold) {
                continue;
            }
            let new = out.docs.len() as u32;
            renumber[old] = Some(new);
            out.docs.push(doc.clone());
            out.stored.push(seg.stored[old].clone());
            if let Some(ts) = del {
                deletes.insert(new, *ts);
            }
        }
        let kept: Vec<usize> = (0..seg.docs.len()).filter(|&d| renumber[d].is_some()).collect();
        let added = out.docs.len() - base;
        for (name, field) in &seg.fields {
            let f = out
                .fields
                .entry(name.clone())
                .or_insert_with(|| FieldData { lengths: vec![0; base], ..FieldData::default() });
            f.lengths.extend(kept.iter().map(|&d| field.lengths[d]));
            for (term, list) in &field.terms {
                let target = f.terms.entry(term.clone()).or_default();
                for p in list {
                    if let Some(doc) = renumber[p.doc as usize] {
                        let mut p = p.clone();
                        p.doc = doc;
                        target.push(p);
                    }
                }
            }
        }
        for (name, col) in &seg.columns {
            let c = out.columns.entry(name.clone()).or_insert_with(|| vec![None; base]);
            c.extend(kept.iter().map(|&d| col[d].clone()));
        }
        for (name, cells) in &seg.geo {
            let g = out.geo.entry(name.clone()).or_insert_with(|| vec![Vec::new(); base]);
            g.extend(kept.iter().map(|&d| cells[d].clone()));
        }
        for (name, v) in &seg.vectors {
            let o = out.vectors.entry(name.clone()).or_insert_with(|| VectorData {
                dims: v.dims,
                similarity: v.similarity,
                quantization: v.quantization,
                values: vec![None; base],
            });
            o.values.extend(kept.iter().map(|&d| v.values[d].clone()));
        }
        // Fields, columns, cells and vectors this segment lacks.
        let total = base + added;
        for f in out.fields.values_mut() {
            f.lengths.resize(total, 0);
        }
        for c in out.columns.values_mut() {
            c.resize(total, None);
        }
        for g in out.geo.values_mut() {
            g.resize(total, Vec::new());
        }
        for v in out.vectors.values_mut() {
            v.values.resize(total, None);
        }
    }
    for f in out.fields.values_mut() {
        f.terms.retain(|_, list| !list.is_empty());
    }
    (out, deletes)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::segment::data::{DocEntry, Occurrence, PostingEntry};
    use crate::segment::file::{Segment, build};
    use teseladb_types::GroupId;

    fn ts(p: u64) -> Hlc {
        Hlc { physical: p, logical: 0 }
    }

    /// A segment of `docs` documents named `prefix-i`, each with term `t` once and `only` on even
    /// documents, a column only in this segment, and cells.
    fn seg(prefix: &str, docs: u32, column: &str) -> SegmentData {
        let mut s = SegmentData::new(GroupId(1), "i", 2);
        s.applied_index = u64::from(docs);
        let occ = vec![Occurrence { position: 0, start: 0, end: 1 }];
        let mut f = FieldData::default();
        for d in 0..docs {
            s.docs
                .push(DocEntry { key: format!("{prefix}-{d}").into_bytes(), ts: ts(u64::from(d)) });
            s.stored.push(Some(vec![d as u8]));
            f.lengths.push(d + 1);
            f.terms
                .entry(b"t".to_vec())
                .or_default()
                .push(PostingEntry { doc: d, occurrences: occ.clone() });
            if d % 2 == 0 {
                f.terms
                    .entry(prefix.as_bytes().to_vec())
                    .or_default()
                    .push(PostingEntry { doc: d, occurrences: occ.clone() });
            }
        }
        s.fields.insert(prefix.to_owned(), f);
        s.columns.insert(column.to_owned(), (0..docs).map(|d| Some(vec![d as u8])).collect());
        s.geo.insert("g".to_owned(), (0..docs).map(|d| vec![u64::from(d)]).collect());
        s
    }

    #[test]
    fn merge_drops_collected_deletions_and_renumbers_the_rest() {
        let a = seg("a", 4, "ca");
        let b = seg("b", 3, "cb");
        // a-1 deleted below the threshold goes; a-2 deleted above it stays with its deletion.
        let dels_a = BTreeMap::from([(1, ts(10)), (2, ts(50))]);
        let dels_b = BTreeMap::from([(0, ts(20))]);
        let (m, dels) = merge(&[(a, dels_a), (b, dels_b)], ts(20));
        let keys: Vec<_> =
            m.docs.iter().map(|d| String::from_utf8(d.key.clone()).unwrap()).collect();
        assert_eq!(keys, ["a-0", "a-2", "a-3", "b-1", "b-2"]);
        assert_eq!(dels, BTreeMap::from([(1, ts(50))]));
        assert_eq!(m.applied_index, 4);
        assert_eq!(m.fields["a"].lengths, [1, 3, 4, 0, 0]);
        assert_eq!(m.fields["b"].lengths, [0, 0, 0, 2, 3]);
        let docs =
            |f: &str, t: &[u8]| m.fields[f].terms[t].iter().map(|p| p.doc).collect::<Vec<_>>();
        assert_eq!(docs("a", b"t"), [0, 1, 2]);
        assert_eq!(docs("a", b"a"), [0, 1]);
        assert_eq!(docs("b", b"b"), [4]);
        assert_eq!(m.columns["ca"], [Some(vec![0]), Some(vec![2]), Some(vec![3]), None, None]);
        assert_eq!(m.columns["cb"], [None, None, None, Some(vec![1]), Some(vec![2])]);
        assert_eq!(m.geo["g"], [vec![0], vec![2], vec![3], vec![1], vec![2]]);
        assert_eq!(m.stored.len(), 5);
        // The merged data writes a valid segment that reads back the same.
        let bytes = build(&m);
        assert_eq!(Segment::open(&bytes).expect("valid").to_data(), Ok(m));
    }

    #[test]
    fn terms_left_without_documents_go() {
        let a = seg("a", 1, "c");
        let (m, _) = merge(&[(a, BTreeMap::from([(0, ts(1))]))], ts(5));
        assert!(m.docs.is_empty());
        assert!(m.fields["a"].terms.is_empty());
    }

    #[test]
    #[should_panic(expected = "share range")]
    fn segments_of_different_indexes_do_not_merge() {
        let a = seg("a", 1, "c");
        let mut b = seg("b", 1, "c");
        b.index = "other".into();
        merge(&[(a, BTreeMap::new()), (b, BTreeMap::new())], ts(0));
    }

    #[test]
    fn policy_merges_the_smallest_of_the_lowest_full_tier() {
        assert_eq!(tier(0), 0);
        assert_eq!(tier(TIER_FLOOR * TIER_FACTOR - 1), 0);
        assert_eq!(tier(TIER_FLOOR * TIER_FACTOR), 1);
        assert_eq!(tier(u64::MAX), 21); // 2^20 · 4^21 = 2^62
        let small: Vec<(u64, u64)> = (0..7).map(|n| (n, 1000 + n)).collect();
        assert_eq!(pick(&small), None);
        let mut segs = small.clone();
        segs.push((7, 500));
        segs.push((8, 900_000));
        let big: Vec<(u64, u64)> = (100..110).map(|n| (n, TIER_FLOOR * 5)).collect();
        segs.extend(big);
        assert_eq!(pick(&segs), Some(vec![7, 0, 1, 2, 3, 4, 5, 6]));
        let only_big: Vec<(u64, u64)> = (100..110).map(|n| (n, TIER_FLOOR * 5 + n)).collect();
        assert_eq!(pick(&only_big), Some((100..108).collect()));
    }
}
