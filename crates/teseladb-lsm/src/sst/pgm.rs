//! A piecewise-linear model of a file's block index with bounded error (FR-041): over keys that
//! grow regularly (ObjectId, timestamps, increasing numbers) a few segments predict, for a key,
//! the block that holds it within [`EPSILON`] blocks, and the search reads only the index entries
//! in that window.
//!
//! A key is placed on a line by the 8 bytes of its logical key after the prefix the logical keys
//! of every block's last version share, read as a big-endian number. The version is left out:
//! internal keys sort newest version first, so the versions of one key that cross a block
//! boundary would make numbers that go down. Logical keys sort by their bytes, and the first 8
//! of them, padded with zeros, never go down; different keys may get one number. The segments come from the shrinking cone: a segment starts at a point and takes
//! the next ones while one slope keeps every point within `EPSILON` of its line.

/// Most blocks between where a segment puts a key and its block.
pub(crate) const EPSILON: usize = 8;
/// Fewest blocks per segment for the model to replace the search of the whole index.
const BLOCKS_PER_SEGMENT: usize = 64;

/// One segment: from key number `start` on, block ≈ `first + slope · (x − start)`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Segment {
    start: u64,
    first: usize,
    slope: f64,
}

/// The model of an index.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Pgm {
    prefix: usize,
    segments: Vec<Segment>,
}

/// The number a key's 8 bytes after `prefix` make.
fn project(key: &[u8], prefix: usize) -> u64 {
    let mut b = [0u8; 8];
    let rest = key.get(prefix..).unwrap_or(&[]);
    let n = rest.len().min(8);
    b[..n].copy_from_slice(&rest[..n]);
    u64::from_be_bytes(b)
}

impl Pgm {
    /// The model of the index whose last keys have the logical keys `keys`, in order; `None` when
    /// the keys are not regular enough for it to pay: more than one segment per
    /// [`BLOCKS_PER_SEGMENT`] blocks.
    pub(crate) fn build<'a>(keys: impl Iterator<Item = &'a [u8]> + Clone) -> Option<Pgm> {
        let first = keys.clone().next()?;
        let prefix = keys
            .clone()
            .map(|k| k.iter().zip(first).take_while(|(a, b)| a == b).count())
            .min()
            .unwrap_or(0);
        let xs: Vec<u64> = keys.map(|k| project(k, prefix)).collect();
        // Logical keys in order make numbers that never go down, which the segments need; keys
        // out of order get no model and are searched whole.
        if !xs.is_sorted() {
            return None;
        }
        let mut segments = Vec::new();
        let mut i = 0;
        while i < xs.len() {
            let (start, first) = (xs[i], i);
            let (mut lo, mut hi) = (0.0f64, f64::INFINITY);
            let mut j = i + 1;
            while j < xs.len() {
                let dx = (xs[j] - start) as f64;
                let dy = (j - first) as f64;
                if dx == 0.0 {
                    // One number for several blocks: the line must be within reach of each.
                    if dy > EPSILON as f64 {
                        break;
                    }
                    j += 1;
                    continue;
                }
                let (l, h) = ((dy - EPSILON as f64) / dx, (dy + EPSILON as f64) / dx);
                if l > hi || h < lo {
                    break;
                }
                (lo, hi) = (lo.max(l), hi.min(h));
                j += 1;
            }
            let slope = if hi.is_finite() { (lo + hi) / 2.0 } else { lo };
            segments.push(Segment { start, first, slope });
            i = j;
        }
        (segments.len() * BLOCKS_PER_SEGMENT <= xs.len()).then_some(Pgm { prefix, segments })
    }

    /// The window of blocks `[lo, hi)` that holds the first block whose last key is at or after
    /// a key of logical key `logical`, of an index of `blocks` blocks; the caller checks its
    /// edges.
    pub(crate) fn window(&self, logical: &[u8], blocks: usize) -> (usize, usize) {
        let x = project(logical, self.prefix);
        let s = self.segments.partition_point(|s| s.start <= x).saturating_sub(1);
        let seg = self.segments[s];
        let guess = seg.first as f64 + seg.slope * x.saturating_sub(seg.start) as f64;
        let guess = guess.max(0.0).min(blocks as f64) as usize;
        (guess.saturating_sub(EPSILON + 1), (guess + EPSILON + 2).min(blocks))
    }

    #[cfg(test)]
    /// Segments of the model.
    pub(crate) fn segments(&self) -> usize {
        self.segments.len()
    }
}

/// The first position of `last_keys` (sorted under `before`) whose key is not before `key`,
/// whose logical key is `logical`: in the window `pgm` gives when its edges hold, else by a
/// search of all of them.
pub(crate) fn find<K: AsRef<[u8]>>(
    pgm: Option<&Pgm>,
    last_keys: &[K],
    (key, logical): (&[u8], &[u8]),
    before: impl Fn(&[u8], &[u8]) -> bool,
) -> usize {
    let n = last_keys.len();
    if let Some(p) = pgm {
        let (lo, hi) = p.window(logical, n);
        let at = lo + last_keys[lo..hi].partition_point(|k| before(k.as_ref(), key));
        let left_holds = lo == 0 || before(last_keys[lo - 1].as_ref(), key);
        let right_holds = hi == n || at < hi;
        if left_holds && right_holds {
            return at;
        }
    }
    last_keys.partition_point(|k| before(k.as_ref(), key))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn object_id(i: u64) -> Vec<u8> {
        let mut k = b"coll/".to_vec();
        k.extend_from_slice(&(1_700_000_000 + i / 4).to_be_bytes()[4..]);
        k.extend_from_slice(&i.to_be_bytes()[2..]);
        k
    }

    /// Keys out of order get no model, and the subtraction of the segments never wraps.
    #[test]
    fn keys_out_of_order_get_no_model() {
        let keys: Vec<Vec<u8>> = (0..200u64).rev().map(object_id).collect();
        assert!(Pgm::build(keys.iter().map(|k| k.as_slice())).is_none());
    }

    /// The model of an index of internal keys (FR-041): one key's versions span several blocks,
    /// newest first, and some logical keys are the start of the next one. With the version in
    /// the number both would go down; on the logical keys alone every key finds its block.
    #[test]
    fn versions_across_blocks_and_keys_that_start_others_find_their_block() {
        use crate::entry;
        use teseladb_keys::Hlc;
        let at = |p: u64| Hlc { physical: p, logical: 0 };
        let mut last_keys: Vec<Vec<u8>> = Vec::new();
        for i in 0..4000u64 {
            let logical = object_id(i * 3);
            match i % 10 {
                // Five blocks end in versions of this key, from the newest down.
                0 => last_keys.extend((0..5).rev().map(|v| entry::internal_key(&logical, at(v)))),
                // The key, then a key it is the start of.
                5 => {
                    last_keys.push(entry::internal_key(&logical, at(1)));
                    let longer = [logical.as_slice(), b"x"].concat();
                    last_keys.push(entry::internal_key(&longer, at(u64::MAX >> 1)));
                }
                _ => last_keys.push(entry::internal_key(&logical, at(1))),
            }
        }
        let before = |a: &[u8], b: &[u8]| entry::compare(a, b).is_lt();
        assert!(last_keys.windows(2).all(|w| before(&w[0], &w[1])));
        let pgm = Pgm::build(last_keys.iter().map(|k| entry::logical(k))).expect("a model");
        let mut probes: Vec<Vec<u8>> = last_keys.clone();
        probes.extend((0..12_010).map(|i| entry::first_version(&object_id(i))));
        probes.extend((0..12_010).map(|i| entry::internal_key(&object_id(i), at(2))));
        for probe in &probes {
            let want = last_keys.partition_point(|k| before(k, probe));
            let got = find(Some(&pgm), &last_keys, (probe, entry::logical(probe)), before);
            assert_eq!(got, want);
        }
    }

    #[test]
    fn regular_keys_take_a_few_segments_and_find_every_block() {
        let keys: Vec<Vec<u8>> = (0..20_000).step_by(7).map(object_id).collect();
        let pgm = Pgm::build(keys.iter().map(|k| k.as_slice())).expect("regular keys");
        assert!(pgm.segments() * 50 < keys.len(), "{} segments", pgm.segments());
        let before = |a: &[u8], b: &[u8]| a < b;
        for probe in (0..20_010).map(object_id).chain([b"a".to_vec(), b"z".to_vec()]) {
            let want = keys.partition_point(|k| k.as_slice() < probe.as_slice());
            assert_eq!(find(Some(&pgm), &keys, (&probe, &probe), before), want);
            let (lo, hi) = pgm.window(&probe, keys.len());
            assert!(hi - lo <= 2 * EPSILON + 3);
        }
    }

    #[test]
    fn irregular_keys_keep_the_search() {
        let mut x = 1u64;
        let mut keys: Vec<Vec<u8>> = (0..2000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x.to_be_bytes().to_vec()
            })
            .collect();
        keys.sort();
        // Random keys are a straight line too, in expectation; keys whose gaps take any scale
        // from 1 to 2^47 are not.
        let mut at = 0u64;
        let clustered: Vec<Vec<u8>> = keys
            .iter()
            .map(|k| {
                at += 1 << (k[7] % 48);
                at.to_be_bytes().to_vec()
            })
            .collect();
        assert_eq!(Pgm::build(clustered.iter().map(|k| k.as_slice())), None);
        // Whatever the model, the answer is the search's.
        if let Some(p) = Pgm::build(keys.iter().map(|k| k.as_slice())) {
            for k in &keys {
                let want = keys.partition_point(|x| x < k);
                assert_eq!(find(Some(&p), &keys, (k, k), |a, b| a < b), want);
            }
        }
        assert_eq!(Pgm::build(std::iter::empty()), None);
        let one = [b"k".to_vec()];
        let p = Pgm::build(one.iter().map(|k| k.as_slice()));
        assert_eq!(find(p.as_ref(), &one, (b"a", b"a"), |a, b| a < b), 0);
        assert_eq!(find(p.as_ref(), &one, (b"z", b"z"), |a, b| a < b), 1);
    }
}
