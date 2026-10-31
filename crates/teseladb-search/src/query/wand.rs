//! Block-Max WAND top-k retrieval (research R3, FR-015).
//!
//! WAND (Broder and others, CIKM 2003) sorts the cursors of the query terms by document and picks
//! as pivot the first document whose cursors, by their best score over the whole list, could beat
//! the k-th score so far. Block-Max WAND (Ding and Suel, SIGIR 2011) then checks the bounds of the
//! blocks that hold the pivot: when they cannot beat it either, every cursor up to the pivot jumps
//! past the nearest block end, without decoding or scoring the documents in between. The result
//! is the same as scoring every document: the `k` best by score, ties to the lower document.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::query::postings::END;

/// Relative slack on the bounds: a sum of bounds may round differently from the score it bounds,
/// so a bound only discards a document when it is clearly below the threshold.
const BOUND_SLACK: f64 = 1e-9;

/// A cursor over the postings of one query term, as Block-Max WAND reads it.
pub trait Cursor {
    /// The current document, or [`END`] once exhausted.
    fn doc(&self) -> u32;
    /// Moves to the first document at or after `target`; never backwards.
    fn advance(&mut self, target: u32);
    /// The term's score in the current document (which is not [`END`]).
    fn score(&self) -> f64;
    /// A bound of [`Cursor::score`] over the whole list.
    fn max_score(&self) -> f64;
    /// The bound of the scores in the block that holds the first document at or after `target`,
    /// with the last document of that block, or `None` past the last block. It moves only the
    /// block, never [`Cursor::doc`].
    fn block_bound(&mut self, target: u32) -> Option<(f64, u32)>;
}

/// A document of the result and its score.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    /// Document number.
    pub doc: u32,
    /// Sum of the scores of its terms, in query order.
    pub score: f64,
}

/// Order of the heap: the worst hit (lowest score, then highest document) is the greatest.
#[derive(Debug)]
struct Worst(Hit);

impl PartialEq for Worst {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Worst {}

impl PartialOrd for Worst {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Worst {
    fn cmp(&self, other: &Self) -> Ordering {
        other.0.score.total_cmp(&self.0.score).then(self.0.doc.cmp(&other.0.doc))
    }
}

/// The `k` best documents of the disjunction of `cursors` (in query order), best first.
pub fn top_k<C: Cursor>(cursors: &mut [C], k: usize) -> Vec<Hit> {
    if k == 0 {
        return Vec::new();
    }
    let mut heap: BinaryHeap<Worst> = BinaryHeap::with_capacity(k + 1);
    let mut order: Vec<usize> = (0..cursors.len()).collect();
    loop {
        let theta = match heap.peek() {
            Some(w) if heap.len() == k => w.0.score,
            _ => f64::NEG_INFINITY,
        };
        order.sort_by_key(|&i| cursors[i].doc());
        let Some(mut p) = pivot(cursors, &order, theta) else { break };
        let d = cursors[order[p]].doc();
        while p + 1 < order.len() && cursors[order[p + 1]].doc() == d {
            p += 1;
        }
        let mut next = order.get(p + 1).map_or(END, |&i| cursors[i].doc());
        let mut bound = 0.0;
        for &i in &order[..=p] {
            if let Some((b, last)) = cursors[i].block_bound(d) {
                bound += b;
                next = next.min(last.saturating_add(1));
            }
        }
        if !passes(bound, theta) {
            // No document in [d, next) holds a term other than these, and their blocks cannot
            // reach the threshold there.
            for &i in &order[..=p] {
                cursors[i].advance(next);
            }
        } else if cursors[order[0]].doc() == d {
            let mut terms = order[..=p].to_vec();
            terms.sort_unstable();
            let score = terms.iter().map(|&i| cursors[i].score()).sum();
            offer(&mut heap, k, Hit { doc: d, score });
            for &i in &terms {
                cursors[i].advance(d + 1);
            }
        } else {
            for &i in &order[..p] {
                cursors[i].advance(d);
            }
        }
    }
    let mut hits: Vec<Hit> = heap.into_iter().map(|w| w.0).collect();
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc.cmp(&b.doc)));
    hits
}

/// The first position of `order` whose cursors up to it could beat `theta`, by their list bounds.
fn pivot<C: Cursor>(cursors: &[C], order: &[usize], theta: f64) -> Option<usize> {
    let mut acc = 0.0;
    for (p, &i) in order.iter().enumerate() {
        if cursors[i].doc() == END {
            return None;
        }
        acc += cursors[i].max_score();
        if passes(acc, theta) {
            return Some(p);
        }
    }
    None
}

fn passes(bound: f64, theta: f64) -> bool {
    bound + bound.abs() * BOUND_SLACK > theta
}

fn offer(heap: &mut BinaryHeap<Worst>, k: usize, hit: Hit) {
    if heap.len() < k {
        heap.push(Worst(hit));
    } else if heap.peek().is_some_and(|w| Worst(hit) < *w) {
        heap.pop();
        heap.push(Worst(hit));
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::query::bm25::{Bm25, FieldStats};
    use crate::query::postings::{Posting, PostingList, TermCursor};

    /// xorshift64*: deterministic data without external crates.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn random_list(rng: &mut Rng, docs: u32, density: u64, spread: u32) -> PostingList {
        let mut postings = Vec::new();
        for doc in 0..docs {
            if rng.below(100) < density {
                let freq = 1 + rng.below(u64::from(spread)) as u32;
                let len = 1 + rng.below(u64::from(spread) * 4) as u32;
                postings.push(Posting { doc, freq, len });
            }
        }
        PostingList::new(postings)
    }

    /// The oracle: every document of the union scored, sorted, cut at `k`.
    fn exhaustive(cursors: &mut [TermCursor<'_>], k: usize) -> Vec<Hit> {
        let mut hits = Vec::new();
        loop {
            let d = cursors.iter().map(|c| c.doc()).min().unwrap_or(END);
            if d == END {
                break;
            }
            let score = cursors.iter().filter(|c| c.doc() == d).map(|c| c.score()).sum();
            hits.push(Hit { doc: d, score });
            for c in cursors.iter_mut() {
                c.advance(d + 1);
            }
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc.cmp(&b.doc)));
        hits.truncate(k);
        hits
    }

    fn check(seed: u64, terms: usize, docs: u32, k: usize) {
        let mut rng = Rng(seed | 1);
        let lists: Vec<PostingList> = (0..terms)
            .map(|_| {
                let density = 1 + rng.below(60);
                let spread = 1 + rng.below(8) as u32;
                random_list(&mut rng, docs, density, spread)
            })
            .collect();
        let field = FieldStats { docs: u64::from(docs), total_len: u64::from(docs) * 12 };
        let scorers: Vec<_> = lists
            .iter()
            .map(|l| {
                let boost = 0.5 + rng.below(4) as f64;
                Bm25::DEFAULT.term(boost, field, l.len() as u64)
            })
            .collect();
        let cursors = || lists.iter().zip(&scorers).map(|(l, s)| l.cursor(*s)).collect::<Vec<_>>();
        let got = top_k(&mut cursors(), k);
        let want = exhaustive(&mut cursors(), k);
        assert_eq!(got, want, "seed {seed}, {terms} terms, {docs} docs, k {k}");
    }

    #[test]
    fn matches_exhaustive_scoring_on_random_queries() {
        for seed in 0..300_u64 {
            let terms = 1 + (seed % 6) as usize;
            let k = [1, 3, 10, 100, 5000][(seed % 5) as usize];
            check(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15), terms, 3000, k);
        }
    }

    #[test]
    fn ties_go_to_the_lower_document() {
        // One term, every posting identical: the k first documents win.
        let list = PostingList::new(
            (0..1000).map(|doc| Posting { doc: doc * 2, freq: 1, len: 4 }).collect(),
        );
        let s = Bm25::DEFAULT.term(1.0, FieldStats { docs: 2000, total_len: 8000 }, 1000);
        let hits = top_k(&mut [list.cursor(s)], 5);
        assert_eq!(hits.iter().map(|h| h.doc).collect::<Vec<_>>(), vec![0, 2, 4, 6, 8]);
    }

    #[test]
    fn skips_blocks_that_cannot_compete() {
        // A rare strong term and a common weak one: the answer is the strong term's documents.
        let common =
            PostingList::new((0..50_000).map(|doc| Posting { doc, freq: 1, len: 50 }).collect());
        let rare = PostingList::new(
            [700, 30_000, 49_999].map(|doc| Posting { doc, freq: 5, len: 5 }).to_vec(),
        );
        let field = FieldStats { docs: 50_000, total_len: 50_000 * 50 };
        let sc = Bm25::DEFAULT.term(1.0, field, 50_000);
        let sr = Bm25::DEFAULT.term(1.0, field, 3);
        let mut cursors = [common.cursor(sc), rare.cursor(sr)];
        let hits = top_k(&mut cursors, 3);
        assert_eq!(hits.iter().map(|h| h.doc).collect::<Vec<_>>(), vec![700, 30_000, 49_999]);
        let want = exhaustive(&mut [common.cursor(sc), rare.cursor(sr)], 3);
        assert_eq!(hits, want);
    }

    #[test]
    fn empty_inputs_give_no_hits() {
        let empty = PostingList::new(Vec::new());
        let s = Bm25::DEFAULT.term(1.0, FieldStats::default(), 0);
        assert!(top_k(&mut [empty.cursor(s)], 10).is_empty());
        assert!(top_k::<TermCursor<'_>>(&mut [], 10).is_empty());
        let one = PostingList::new(vec![Posting { doc: 3, freq: 1, len: 1 }]);
        assert!(top_k(&mut [one.cursor(s)], 0).is_empty());
    }

    #[test]
    fn heap_order_puts_the_worst_hit_on_top() {
        let a = Worst(Hit { doc: 1, score: 2.0 });
        let b = Worst(Hit { doc: 2, score: 2.0 });
        let c = Worst(Hit { doc: 0, score: 1.0 });
        assert!(b > a && c > b);
        assert_eq!(a.partial_cmp(&a), Some(Ordering::Equal));
        assert!(a == Worst(Hit { doc: 1, score: 2.0 }));
    }
}
