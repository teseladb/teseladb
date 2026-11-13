//! Deltas between two versions of a key (FR-035): an old version in a history file is kept as
//! the edits that turn the next newer version of its key into it, when they take fewer bytes
//! than the version itself.
//!
//! ```text
//! delta   = segment*
//! segment = gap(varint) · removed(varint) · inserted(varint) · inserted bytes
//! ```
//!
//! Each segment skips `gap` bytes of the newer version, copying them, drops the next `removed`
//! and writes `inserted` bytes in their place; what follows the last segment is copied.

use super::{Bad, get_varint, put_varint};

/// Equal bytes between two changed runs below which the runs go in one segment: a segment
/// costs about three bytes of header.
const MERGE_GAP: usize = 4;

/// The delta that turns `base` into `target`, or `None` when it would not be smaller than
/// `target`.
pub(crate) fn encode(base: &[u8], target: &[u8]) -> Option<Vec<u8>> {
    let prefix = base.iter().zip(target).take_while(|(a, b)| a == b).count();
    let room = base.len().min(target.len()) - prefix;
    let suffix =
        base.iter().rev().zip(target.iter().rev()).take(room).take_while(|(a, b)| a == b).count();
    let (old, new) = (&base[prefix..base.len() - suffix], &target[prefix..target.len() - suffix]);
    let mut out = Vec::new();
    match old.len() == new.len() {
        // The same length: one segment per run of changed bytes.
        true => {
            let mut copied = 0;
            for (start, end) in changed_runs(old, new) {
                put_segment(&mut out, prefix + start - copied, end - start, &new[start..end]);
                copied = prefix + end;
            }
        }
        false => put_segment(&mut out, prefix, old.len(), new),
    }
    (out.len() < target.len()).then_some(out)
}

/// The runs `[start, end)` where `old` and `new`, of one length, differ, joined across fewer
/// than [`MERGE_GAP`] equal bytes.
fn changed_runs(old: &[u8], new: &[u8]) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < old.len() {
        if old[i] == new[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < old.len() && old[i] != new[i] {
            i += 1;
        }
        match runs.last_mut() {
            Some(last) if start - last.1 < MERGE_GAP => last.1 = i,
            _ => runs.push((start, i)),
        }
    }
    runs
}

fn put_segment(out: &mut Vec<u8>, gap: usize, removed: usize, inserted: &[u8]) {
    put_varint(out, gap as u64);
    put_varint(out, removed as u64);
    put_varint(out, inserted.len() as u64);
    out.extend_from_slice(inserted);
}

/// The version that `delta` makes of `base`.
pub(crate) fn apply(base: &[u8], delta: &[u8]) -> Result<Vec<u8>, Bad> {
    let mut out = Vec::with_capacity(base.len());
    let (mut at, mut from) = (0usize, 0usize);
    let number = |at: &mut usize| usize::try_from(get_varint(delta, at)?).map_err(|_| Bad);
    while at < delta.len() {
        let gap = number(&mut at)?;
        let removed = number(&mut at)?;
        let inserted = number(&mut at)?;
        let keep = from.checked_add(gap).filter(|k| *k <= base.len()).ok_or(Bad)?;
        out.extend_from_slice(&base[from..keep]);
        from = keep.checked_add(removed).filter(|f| *f <= base.len()).ok_or(Bad)?;
        let bytes = at.checked_add(inserted).filter(|e| *e <= delta.len()).ok_or(Bad)?;
        out.extend_from_slice(&delta[at..bytes]);
        at = bytes;
    }
    out.extend_from_slice(&base[from..]);
    Ok(out)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn round_trip(base: &[u8], target: &[u8]) -> Option<usize> {
        let d = encode(base, target)?;
        assert_eq!(apply(base, &d).unwrap(), target, "{base:?} -> {target:?}");
        Some(d.len())
    }

    #[test]
    fn a_few_changed_bytes_take_a_few_bytes() {
        let base: Vec<u8> = (0..300u32).map(|i| (i % 251) as u8).collect();
        let mut target = base.clone();
        target[10] ^= 1;
        target[11] ^= 1;
        target[200] = 7;
        assert!(round_trip(&base, &target).unwrap() <= 10);
        // A field that grows and one that shrinks: one segment between them.
        let mut grown = base.clone();
        grown.splice(50..52, [1, 2, 3, 4, 5]);
        assert!(round_trip(&base, &grown).unwrap() < 20);
        let shrunk = [&base[..100], &base[140..]].concat();
        assert!(round_trip(&base, &shrunk).unwrap() <= 4);
        // Runs a few bytes apart go together.
        let mut near = base.clone();
        near[30] ^= 9;
        near[32] ^= 9;
        let d = encode(&base, &near).unwrap();
        assert_eq!(d.len(), 3 + 3);
        assert_eq!(apply(&base, &d).unwrap(), near);
    }

    #[test]
    fn a_delta_that_does_not_save_bytes_is_not_used() {
        assert_eq!(encode(&[1; 300], &[2; 300]), None);
        assert_eq!(encode(b"abc", b"xyz"), None);
        assert_eq!(encode(b"", b""), None);
        assert_eq!(round_trip(&[5; 64], &[5; 64]), Some(0));
        // An empty delta is the newer version unchanged.
        assert_eq!(apply(b"same", &[]).unwrap(), b"same");
    }

    #[test]
    fn edge_versions_round_trip() {
        let long: Vec<u8> = (0..500u32).map(|i| (i * 7 % 256) as u8).collect();
        round_trip(&long, &long[..10]);
        round_trip(&long[..400], &long);
        round_trip(&long, &[&long[..250], &long[260..]].concat());
        round_trip(&[&[0u8; 100][..], &long[..]].concat(), &long);
    }

    #[test]
    fn a_damaged_delta_is_refused() {
        // A gap past the newer version.
        let mut d = Vec::new();
        put_segment(&mut d, 10, 0, b"x");
        assert_eq!(apply(b"short", &d), Err(Bad));
        // Removed bytes past it.
        let mut d = Vec::new();
        put_segment(&mut d, 1, 10, b"");
        assert_eq!(apply(b"short", &d), Err(Bad));
        // Inserted bytes the delta does not have.
        let mut d = Vec::new();
        put_segment(&mut d, 0, 0, b"abc");
        d.truncate(d.len() - 1);
        assert_eq!(apply(b"short", &d), Err(Bad));
        // A varint cut short.
        assert_eq!(apply(b"short", &[0x80]), Err(Bad));
    }
}
