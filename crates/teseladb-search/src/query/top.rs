//! The `k` best documents of a disjunction of scored clauses with Block-Max WAND (011 FR-015,
//! FR-030, T058), in the order and with the scores of adding every clause of every document
//! first: a document's score is the sum of its clauses' 32-bit scores in doubles, in clause
//! order, rounded to 32 bits, and equal scores go by document key.
//!
//! Each clause is a list of documents in number order, cut into blocks of [`BLOCK`] with the
//! largest score of each. A document enters the result only when the bounds of the blocks that
//! hold it, rounded as its score would be, reach the `k`-th score so far; blocks whose bounds
//! cannot, and the documents outside `candidates` (the documents the other conditions of the
//! query keep, as a roaring bitmap of document numbers), are jumped over without adding them.

use std::cmp::Ordering;

use teseladb_index::roaring::Roaring;

/// Documents per block of a clause.
pub const BLOCK: usize = 128;

/// Relative slack on a sum of bounds, which may round otherwise than the score it bounds.
const BOUND_SLACK: f64 = 1e-9;

/// A clause: its documents in number order with their scores, and the largest score of each
/// block and of the whole list.
struct Clause<'a> {
    list: &'a [(u32, f32)],
    blocks: Vec<f64>,
    max: f64,
    at: usize,
}

impl<'a> Clause<'a> {
    fn new(list: &'a [(u32, f32)]) -> Clause<'a> {
        let blocks: Vec<f64> = list
            .chunks(BLOCK)
            .map(|b| b.iter().map(|&(_, s)| f64::from(s)).fold(f64::NEG_INFINITY, f64::max))
            .collect();
        let max = blocks.iter().copied().fold(0.0, f64::max);
        Clause { list, blocks, max, at: 0 }
    }

    /// The current document, `u32::MAX` past the end.
    fn doc(&self) -> u32 {
        self.list.get(self.at).map_or(u32::MAX, |&(d, _)| d)
    }

    /// Moves to the first document at or after `target`.
    fn advance(&mut self, target: u32) {
        if self.doc() >= target {
            return;
        }
        self.at += self.list[self.at..].partition_point(|&(d, _)| d < target);
    }

    /// The bound of the block of the first document at or after `target`, and the last
    /// document of that block, without moving.
    fn block_bound(&self, target: u32) -> Option<(f64, u32)> {
        let i = self.at + self.list[self.at..].partition_point(|&(d, _)| d < target);
        let block = i / BLOCK;
        let last = self.list.get(((block + 1) * BLOCK).min(self.list.len()).checked_sub(1)?)?;
        (i < self.list.len()).then(|| (self.blocks[block], last.0))
    }
}

/// A document of the result: its number, key and score.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Best<'k> {
    /// Document number.
    pub doc: u32,
    /// Its key.
    pub key: &'k [u8],
    /// Its score.
    pub score: f32,
}

/// Whether `a` goes before `b`: the higher score, then the lower key.
fn before(a: &Best<'_>, b: &Best<'_>) -> bool {
    match b.score.total_cmp(&a.score) {
        Ordering::Less => true,
        Ordering::Greater => false,
        Ordering::Equal => a.key < b.key,
    }
}

/// Whether a document whose clause scores add up to at most `bound` could take the place of
/// `worst`, the `k`-th so far: its score, rounded, could reach `worst`'s (a tie may go to it by
/// its key).
fn may_reach(bound: f64, worst: Option<&Best<'_>>) -> bool {
    let Some(w) = worst else { return true };
    ((bound + bound.abs() * BOUND_SLACK) as f32) >= w.score
}

/// The `k` best documents of the clauses `lists` (each in document order) among `candidates`
/// (every document when `None`), best first; `key_of` gives a document's key, and a document
/// without one is left out.
pub fn best<'k>(
    lists: &[Vec<(u32, f32)>],
    candidates: Option<&Roaring>,
    k: usize,
    key_of: impl Fn(u32) -> Option<&'k [u8]>,
) -> Vec<Best<'k>> {
    let mut found: Vec<Best<'k>> = Vec::with_capacity(k + 1);
    if k == 0 {
        return found;
    }
    let mut clauses: Vec<Clause<'_>> = lists.iter().map(|l| Clause::new(l)).collect();
    let mut order: Vec<usize> = (0..clauses.len()).collect();
    loop {
        let worst = (found.len() == k).then(|| found[k - 1]);
        order.sort_by_key(|&i| clauses[i].doc());
        // The pivot: the first clauses whose list bounds together could reach the threshold.
        let mut acc = 0.0;
        let mut pivot = None;
        for (p, &i) in order.iter().enumerate() {
            if clauses[i].doc() == u32::MAX {
                break;
            }
            acc += clauses[i].max;
            if may_reach(acc, worst.as_ref()) {
                pivot = Some(p);
                break;
            }
        }
        let Some(mut p) = pivot else { break };
        let d = clauses[order[p]].doc();
        // A document outside the candidates: every clause up to it jumps to the next one.
        if let Some(c) = candidates
            && !c.contains(d)
        {
            let next = c.next_from(d).unwrap_or(u32::MAX);
            for &i in &order[..=p] {
                clauses[i].advance(next);
            }
            continue;
        }
        while p + 1 < order.len() && clauses[order[p + 1]].doc() == d {
            p += 1;
        }
        let mut next = order.get(p + 1).map_or(u32::MAX, |&i| clauses[i].doc());
        let mut bound = 0.0;
        for &i in &order[..=p] {
            if let Some((b, last)) = clauses[i].block_bound(d) {
                bound += b;
                next = next.min(last.saturating_add(1));
            }
        }
        if !may_reach(bound, worst.as_ref()) {
            // No document in [d, next) holds another clause, and these blocks cannot reach
            // the threshold there.
            for &i in &order[..=p] {
                clauses[i].advance(next);
            }
            continue;
        }
        if clauses[order[0]].doc() != d {
            for &i in &order[..p] {
                clauses[i].advance(d);
            }
            continue;
        }
        // Every clause at `d` adds, in clause order.
        let mut terms: Vec<usize> = order[..=p].to_vec();
        terms.sort_unstable();
        let mut sum = 0.0f64;
        for &i in &terms {
            sum += f64::from(clauses[i].list[clauses[i].at].1);
            clauses[i].advance(d + 1);
        }
        if let Some(key) = key_of(d) {
            offer(&mut found, k, Best { doc: d, key, score: sum as f32 });
        }
    }
    found
}

/// Puts `hit` among the `k` best of `found`, kept in order.
fn offer<'k>(found: &mut Vec<Best<'k>>, k: usize, hit: Best<'k>) {
    let at = found.partition_point(|f| before(f, &hit));
    if at < k {
        found.insert(at, hit);
        found.truncate(k);
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// xorshift64*: deterministic data without external crates.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % n
        }
    }

    /// Every document of every clause added, sorted, cut at `k`: the oracle.
    fn all<'k>(
        lists: &[Vec<(u32, f32)>],
        candidates: Option<&Roaring>,
        k: usize,
        keys: &'k [Vec<u8>],
    ) -> Vec<Best<'k>> {
        let mut sums: std::collections::BTreeMap<u32, f64> = Default::default();
        for l in lists {
            for &(d, s) in l {
                *sums.entry(d).or_insert(0.0) += f64::from(s);
            }
        }
        let mut out: Vec<Best<'k>> = sums
            .into_iter()
            .filter(|(d, _)| candidates.is_none_or(|c| c.contains(*d)))
            .map(|(d, s)| Best { doc: d, key: &keys[d as usize], score: s as f32 })
            .collect();
        out.sort_by(|a, b| match before(a, b) {
            true => Ordering::Less,
            false if before(b, a) => Ordering::Greater,
            false => Ordering::Equal,
        });
        out.truncate(k);
        out
    }

    #[test]
    fn the_best_are_those_of_adding_everything() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for round in 0..400 {
            let docs = 1 + rng.below(3000) as u32;
            // Keys in another order than the numbers, so ties go by key.
            let keys: Vec<Vec<u8>> =
                (0..docs).map(|d| ((d * 7919) % docs).to_be_bytes().to_vec()).collect();
            let lists: Vec<Vec<(u32, f32)>> = (0..1 + rng.below(5))
                .map(|_| {
                    let density = 1 + rng.below(80);
                    let levels = 1 + rng.below(6);
                    let mut list = Vec::new();
                    for d in 0..docs {
                        if rng.below(100) < density {
                            list.push((d, (1 + rng.below(levels)) as f32 * 0.37));
                        }
                    }
                    list
                })
                .collect();
            let candidates = (round % 3 == 1).then(|| {
                let keep = 1 + rng.below(100);
                Roaring::from_ids((0..docs).filter(|_| rng.below(100) < keep))
            });
            let k = [1, 2, 10, 100, 5000][round % 5];
            let got =
                best(&lists, candidates.as_ref(), k, |d| keys.get(d as usize).map(|k| &k[..]));
            assert_eq!(got, all(&lists, candidates.as_ref(), k, &keys), "round {round}");
        }
    }

    #[test]
    fn empty_inputs_and_missing_keys() {
        let keys = [vec![1u8], vec![0u8]];
        let key_of = |d: u32| keys.get(d as usize).map(|k| &k[..]);
        assert!(best(&[], None, 3, key_of).is_empty());
        assert!(best(&[vec![(0, 1.0)]], None, 0, key_of).is_empty());
        let lists = [vec![(0, 1.0), (1, 1.0), (2, 5.0)]];
        // Document 2 has no key; the tie of 0 and 1 goes to the lower key, document 1's.
        let got: Vec<u32> = best(&lists, None, 2, key_of).iter().map(|b| b.doc).collect();
        assert_eq!(got, [1, 0]);
        let none = Roaring::new();
        assert!(best(&lists, Some(&none), 2, key_of).is_empty());
    }

    #[test]
    fn blocks_that_cannot_reach_the_threshold_are_jumped() {
        let common: Vec<(u32, f32)> = (0..10_000).map(|d| (d, 0.5)).collect();
        let rare: Vec<(u32, f32)> = [17, 5000, 9999].map(|d| (d, 9.0)).to_vec();
        let keys: Vec<Vec<u8>> = (0..10_000u32).map(|d| d.to_be_bytes().to_vec()).collect();
        let lists = [common, rare];
        let got = best(&lists, None, 3, |d| keys.get(d as usize).map(|k| &k[..]));
        assert_eq!(got.iter().map(|b| b.doc).collect::<Vec<_>>(), [17, 5000, 9999]);
        assert_eq!(got, all(&lists, None, 3, &keys));
    }
}
