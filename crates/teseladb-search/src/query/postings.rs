//! Posting lists cut into blocks of [`BLOCK`] postings, each with its impacts (FR-009).
//!
//! The impacts of a block are its `(freq, len)` pairs that no other pair of the block dominates
//! (a pair dominates another with at least its frequency and at most its length). Since a term's
//! score grows with the frequency and falls with the length, the best score of a block is the best
//! score of its impacts, computed at query time with the global statistics of the read timestamp
//! (`segment-format.md`). The segment reader of T011 decodes its compressed blocks into this
//! shape; [`PostingList`] keeps them in memory.

use crate::query::bm25::TermScorer;
use crate::query::wand::Cursor;

/// Postings per block.
pub const BLOCK: usize = 128;

/// Document number past the last posting of a cursor.
pub const END: u32 = u32::MAX;

/// One posting: a document, the term's frequency in it and the length of the field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Posting {
    /// Document number within the segment.
    pub doc: u32,
    /// Occurrences of the term.
    pub freq: u32,
    /// Length of the field, in tokens.
    pub len: u32,
}

/// A block: its postings, the last document and the impacts.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Block {
    last: u32,
    impacts: Vec<(u32, u32)>,
}

/// The postings of one term in one field, in document order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostingList {
    postings: Vec<Posting>,
    blocks: Vec<Block>,
}

impl PostingList {
    /// Builds the list from postings in strictly increasing document order, below [`END`].
    pub fn new(postings: Vec<Posting>) -> PostingList {
        assert!(
            postings.windows(2).all(|w| w[0].doc < w[1].doc),
            "postings go in strictly increasing document order"
        );
        assert!(postings.last().is_none_or(|p| p.doc < END), "document numbers stay below END");
        let blocks = postings
            .chunks(BLOCK)
            .map(|chunk| {
                let pairs: Vec<(u32, u32)> = chunk.iter().map(|p| (p.freq, p.len)).collect();
                Block { last: chunk[chunk.len() - 1].doc, impacts: impacts_of(&pairs) }
            })
            .collect();
        PostingList { postings, blocks }
    }

    /// Number of postings (the documents with the term).
    pub fn len(&self) -> usize {
        self.postings.len()
    }

    /// Whether the list has no posting.
    pub fn is_empty(&self) -> bool {
        self.postings.is_empty()
    }

    /// The impacts of every block, for tests and the segment writer.
    pub fn block_impacts(&self) -> impl Iterator<Item = &[(u32, u32)]> {
        self.blocks.iter().map(|b| b.impacts.as_slice())
    }

    /// A scoring cursor over the list.
    pub fn cursor(&self, scorer: TermScorer) -> TermCursor<'_> {
        let max_score = self.blocks.iter().map(|b| best(&scorer, &b.impacts)).fold(0.0, f64::max);
        TermCursor { list: self, scorer, pos: 0, block: 0, max_score }
    }
}

/// The `(freq, len)` pairs that no other pair dominates, by increasing frequency. A norm byte
/// can stand for the length: it grows with it.
pub(crate) fn impacts_of(pairs: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let mut pairs = pairs.to_vec();
    // By frequency descending, then length ascending: a pair survives when its length is below
    // that of every pair with a higher frequency.
    pairs.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut kept: Vec<(u32, u32)> = Vec::new();
    for (freq, len) in pairs {
        if kept.last().is_none_or(|&(_, l)| len < l) {
            kept.push((freq, len));
        }
    }
    kept.reverse();
    kept
}

fn best(scorer: &TermScorer, impacts: &[(u32, u32)]) -> f64 {
    impacts.iter().map(|&(f, l)| scorer.score(f, l)).fold(0.0, f64::max)
}

/// A cursor over a [`PostingList`] that scores with one [`TermScorer`].
#[derive(Clone, Debug)]
pub struct TermCursor<'a> {
    list: &'a PostingList,
    scorer: TermScorer,
    pos: usize,
    block: usize,
    max_score: f64,
}

impl Cursor for TermCursor<'_> {
    fn doc(&self) -> u32 {
        self.list.postings.get(self.pos).map_or(END, |p| p.doc)
    }

    fn advance(&mut self, target: u32) {
        if self.doc() >= target {
            return;
        }
        self.shallow(target);
        let start = self.block * BLOCK;
        let block = &self.list.postings[start.min(self.list.postings.len())..];
        let block = &block[..block.len().min(BLOCK)];
        self.pos = start + block.partition_point(|p| p.doc < target);
    }

    fn score(&self) -> f64 {
        let p = self.list.postings[self.pos];
        self.scorer.score(p.freq, p.len)
    }

    fn max_score(&self) -> f64 {
        self.max_score
    }

    fn block_bound(&mut self, target: u32) -> Option<(f64, u32)> {
        self.shallow(target);
        let b = self.list.blocks.get(self.block)?;
        Some((best(&self.scorer, &b.impacts), b.last))
    }
}

impl TermCursor<'_> {
    /// Moves the block, not the posting, to the first block whose last document is `target` or
    /// later; never backwards.
    fn shallow(&mut self, target: u32) {
        let rest = &self.list.blocks[self.block..];
        self.block += rest.partition_point(|b| b.last < target);
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::query::bm25::{Bm25, FieldStats};

    fn scorer() -> TermScorer {
        Bm25::DEFAULT.term(1.0, FieldStats { docs: 1000, total_len: 10_000 }, 10)
    }

    fn list(n: u32) -> PostingList {
        PostingList::new(
            (0..n).map(|i| Posting { doc: i * 3, freq: 1 + i % 5, len: 5 + i % 17 }).collect(),
        )
    }

    #[test]
    fn impacts_keep_only_undominated_pairs() {
        let pairs = [(1, 10), (2, 10), (2, 3), (5, 20), (1, 1), (5, 25), (3, 3)];
        assert_eq!(impacts_of(&pairs), vec![(1, 1), (3, 3), (5, 20)]);
    }

    #[test]
    fn block_bounds_dominate_every_score_of_their_block() {
        let l = list(1000);
        assert_eq!(l.len(), 1000);
        assert!(!l.is_empty());
        assert_eq!(l.block_impacts().count(), 1000_usize.div_ceil(BLOCK));
        let mut c = l.cursor(scorer());
        let mut global = 0.0_f64;
        while c.doc() != END {
            let d = c.doc();
            let (bound, last) = c.block_bound(d).expect("in a block");
            assert!(last >= d);
            assert!(c.score() <= bound);
            assert!(bound <= c.max_score());
            global = global.max(c.score());
            c.advance(d + 1);
        }
        assert_eq!(global, c.max_score());
        assert_eq!(c.block_bound(END), None);
    }

    #[test]
    fn advance_finds_the_first_posting_at_or_after_the_target() {
        let l = list(500);
        let mut c = l.cursor(scorer());
        c.advance(0);
        assert_eq!(c.doc(), 0);
        c.advance(301);
        assert_eq!(c.doc(), 303);
        c.advance(100); // never backwards
        assert_eq!(c.doc(), 303);
        c.advance(3 * 499);
        assert_eq!(c.doc(), 3 * 499);
        c.advance(3 * 499 + 1);
        assert_eq!(c.doc(), END);
        c.advance(END);
        assert_eq!(c.doc(), END);
    }

    #[test]
    fn empty_list_is_exhausted() {
        let l = PostingList::new(Vec::new());
        assert!(l.is_empty());
        let mut c = l.cursor(scorer());
        assert_eq!(c.doc(), END);
        assert_eq!(c.max_score(), 0.0);
        c.advance(5);
        assert_eq!(c.doc(), END);
        assert_eq!(c.block_bound(0), None);
    }

    #[test]
    #[should_panic(expected = "strictly increasing")]
    fn unsorted_postings_are_rejected() {
        let p = Posting { doc: 1, freq: 1, len: 1 };
        PostingList::new(vec![p, p]);
    }

    #[test]
    #[should_panic(expected = "below END")]
    fn end_is_not_a_document() {
        PostingList::new(vec![Posting { doc: END, freq: 1, len: 1 }]);
    }
}
