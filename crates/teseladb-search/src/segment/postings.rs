//! Postings of a term on disk (`segment-format.md`): the number of documents, then blocks of up
//! to [`BLOCK`] postings.
//!
//! A block starts with a header that a query reads without the rest: the number of postings, the
//! last document, the impacts (`(freq, norm)` pairs no other pair of the block dominates) and the
//! length of the body, which is the pointer to the next block. The body holds the document gaps
//! and the frequencies minus one, each packed at the smallest bit width, then per occurrence the
//! gaps of position and start offset and the token's length, as LEB128.

use crate::query::bm25::TermScorer;
use crate::query::postings::{BLOCK, END};
use crate::query::wand::Cursor;
use crate::segment::data::{Occurrence, PostingEntry};
use crate::segment::{Corrupt, bitpack, norms, varint};

/// Appends the postings of a term; `norm_of` holds the norm byte of each document.
pub fn write(out: &mut Vec<u8>, postings: &[PostingEntry], norm_of: &[u8]) {
    varint::put(out, postings.len() as u64);
    let mut base = 0_u32;
    for chunk in postings.chunks(BLOCK) {
        let last = chunk[chunk.len() - 1].doc;
        let mut body = Vec::new();
        let gaps: Vec<u32> = chunk
            .iter()
            .scan(base, |prev, p| {
                let gap = p.doc - *prev;
                *prev = p.doc;
                Some(gap)
            })
            .collect();
        bitpack::pack(&mut body, &gaps);
        let freqs: Vec<u32> = chunk.iter().map(|p| p.occurrences.len() as u32 - 1).collect();
        bitpack::pack(&mut body, &freqs);
        for p in chunk {
            let (mut pos, mut start) = (0, 0);
            for o in &p.occurrences {
                varint::put(&mut body, u64::from(o.position - pos));
                varint::put(&mut body, u64::from(o.start - start));
                varint::put(&mut body, u64::from(o.end - o.start));
                (pos, start) = (o.position, o.start);
            }
        }
        let pairs: Vec<(u32, u32)> = chunk
            .iter()
            .map(|p| (p.occurrences.len() as u32, u32::from(norm_of[p.doc as usize])))
            .collect();
        varint::put(out, chunk.len() as u64);
        varint::put(out, u64::from(last));
        let impacts = crate::query::postings::impacts_of(&pairs);
        varint::put(out, impacts.len() as u64);
        for (freq, norm) in impacts {
            varint::put(out, u64::from(freq));
            out.push(norm as u8);
        }
        varint::put(out, body.len() as u64);
        out.extend_from_slice(&body);
        base = last;
    }
}

/// A block header.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Header {
    count: usize,
    last: u32,
    impacts: Vec<(u32, u8)>,
    body: usize,
    body_len: usize,
}

/// Reads the document count and every block header of the postings at `bytes`.
fn headers(bytes: &[u8]) -> Option<(u64, Vec<Header>)> {
    let mut pos = 0;
    let df = varint::get(bytes, &mut pos)?;
    let mut out = Vec::new();
    let mut total = 0_u64;
    while total < df {
        let count = usize::try_from(varint::get(bytes, &mut pos)?).ok()?;
        let last = u32::try_from(varint::get(bytes, &mut pos)?).ok()?;
        if count == 0 || count > BLOCK || out.last().is_some_and(|h: &Header| h.last >= last) {
            return None;
        }
        let n = varint::get(bytes, &mut pos)?;
        if n == 0 || n > count as u64 {
            return None;
        }
        let mut impacts = Vec::new();
        for _ in 0..n {
            let freq = u32::try_from(varint::get(bytes, &mut pos)?).ok()?;
            impacts.push((freq, *bytes.get(pos)?));
            pos += 1;
        }
        let body_len = usize::try_from(varint::get(bytes, &mut pos)?).ok()?;
        let body = pos;
        pos = pos.checked_add(body_len).filter(|&p| p <= bytes.len())?;
        out.push(Header { count, last, impacts, body, body_len });
        total += count as u64;
    }
    (total == df).then_some((df, out))
}

/// The body of a block: documents and frequencies, and the occurrences if `full`.
fn body(
    bytes: &[u8],
    h: &Header,
    base: u32,
    full: bool,
) -> Option<(Vec<u32>, Vec<u32>, Vec<Vec<Occurrence>>)> {
    let b = &bytes[h.body..h.body + h.body_len];
    let mut pos = 0;
    let docs = bitpack::documents(base, bitpack::unpack(b, &mut pos, h.count)?);
    // A gap of 0 after the first document, or a sum past `u32::MAX` (which wraps below the
    // document before), leaves a document that does not grow.
    let first_ok = docs[0] > base || (docs[0] == 0 && base == 0);
    if !first_ok || docs.windows(2).any(|d| d[1] <= d[0]) || docs.last() != Some(&h.last) {
        return None;
    }
    let freqs: Vec<u32> = bitpack::unpack(b, &mut pos, h.count)?
        .into_iter()
        .map(|f| f.checked_add(1))
        .collect::<Option<_>>()?;
    let mut occ = Vec::new();
    if full {
        for &f in &freqs {
            let (mut position, mut start) = (0_u32, 0_u32);
            let mut list = Vec::with_capacity(f as usize);
            for _ in 0..f {
                let small = |pos: &mut usize| u32::try_from(varint::get(b, pos)?).ok();
                position = position.checked_add(small(&mut pos)?)?;
                start = start.checked_add(small(&mut pos)?)?;
                let end = start.checked_add(small(&mut pos)?)?;
                list.push(Occurrence { position, start, end });
            }
            occ.push(list);
        }
        if pos != b.len() {
            return None;
        }
    }
    Some((docs, freqs, occ))
}

/// Every posting of a term, with its occurrences (for merges and phrase queries).
pub fn read_all(bytes: &[u8]) -> Result<Vec<PostingEntry>, Corrupt> {
    let bad = Corrupt("postings");
    let (_, hs) = headers(bytes).ok_or(bad.clone())?;
    let mut out = Vec::new();
    let mut base = 0;
    for h in &hs {
        let (docs, _, occ) = body(bytes, h, base, true).ok_or(bad.clone())?;
        out.extend(
            docs.into_iter().zip(occ).map(|(doc, occurrences)| PostingEntry { doc, occurrences }),
        );
        base = h.last;
    }
    Ok(out)
}

/// A [`Cursor`] over postings on disk: it reads the block headers once and a block's body only
/// when a document of it is needed. A body that fails its checks ends the cursor and sets
/// [`DiskCursor::corrupt`].
#[derive(Clone, Debug)]
pub struct DiskCursor<'a> {
    bytes: &'a [u8],
    norms: &'a [u8],
    scorer: TermScorer,
    headers: Vec<Header>,
    block: usize,
    loaded: Option<usize>,
    docs: Vec<u32>,
    freqs: Vec<u32>,
    idx: usize,
    max_score: f64,
    corrupt: bool,
}

impl<'a> DiskCursor<'a> {
    /// A cursor over the postings at `bytes`, with the norm byte of each document in `norms`.
    pub fn new(bytes: &'a [u8], norms: &'a [u8], scorer: TermScorer) -> Result<Self, Corrupt> {
        let (_, headers) = headers(bytes).ok_or(Corrupt("postings"))?;
        let max_score = headers.iter().map(|h| bound(&scorer, h)).fold(0.0, f64::max);
        let mut c = DiskCursor {
            bytes,
            norms,
            scorer,
            headers,
            block: 0,
            loaded: None,
            docs: Vec::new(),
            freqs: Vec::new(),
            idx: 0,
            max_score,
            corrupt: false,
        };
        c.load();
        Ok(c)
    }

    /// Number of documents with the term.
    pub fn doc_freq(&self) -> u64 {
        self.headers.iter().map(|h| h.count as u64).sum()
    }

    /// Whether a block failed its checks.
    pub fn corrupt(&self) -> bool {
        self.corrupt
    }

    fn load(&mut self) {
        if self.loaded == Some(self.block) || self.block >= self.headers.len() {
            return;
        }
        let base = self.block.checked_sub(1).map_or(0, |b| self.headers[b].last);
        match body(self.bytes, &self.headers[self.block], base, false) {
            Some((docs, freqs, _)) if docs.iter().all(|&d| (d as usize) < self.norms.len()) => {
                self.docs = docs;
                self.freqs = freqs;
                self.idx = 0;
                self.loaded = Some(self.block);
            }
            _ => {
                self.corrupt = true;
                self.block = self.headers.len();
            }
        }
    }

    fn shallow(&mut self, target: u32) {
        let rest = &self.headers[self.block.min(self.headers.len())..];
        self.block += rest.partition_point(|h| h.last < target);
    }
}

fn bound(scorer: &TermScorer, h: &Header) -> f64 {
    h.impacts.iter().map(|&(f, n)| scorer.score(f, norms::decode(n))).fold(0.0, f64::max)
}

impl Cursor for DiskCursor<'_> {
    fn doc(&self) -> u32 {
        if self.loaded == Some(self.block) { self.docs[self.idx] } else { END }
    }

    fn advance(&mut self, target: u32) {
        if self.doc() >= target {
            return;
        }
        self.shallow(target);
        self.load();
        if self.loaded == Some(self.block) {
            self.idx = self.docs.partition_point(|&d| d < target);
        }
    }

    fn score(&self) -> f64 {
        let doc = self.docs[self.idx];
        self.scorer.score(self.freqs[self.idx], norms::decode(self.norms[doc as usize]))
    }

    fn max_score(&self) -> f64 {
        self.max_score
    }

    fn block_bound(&mut self, target: u32) -> Option<(f64, u32)> {
        let rest = &self.headers[self.block.min(self.headers.len())..];
        let h = rest.get(rest.partition_point(|h| h.last < target))?;
        Some((bound(&self.scorer, h), h.last))
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::query::bm25::{Bm25, FieldStats};
    use crate::query::postings::{Posting, PostingList};
    use crate::query::wand::top_k;

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    fn random_term(rng: &mut Rng, docs: u32, density: u64) -> Vec<PostingEntry> {
        let mut out = Vec::new();
        for doc in 0..docs {
            if rng.below(100) < density {
                let mut position = 0;
                let mut start = 0;
                let occurrences = (0..1 + rng.below(4))
                    .map(|_| {
                        position += rng.below(5) as u32;
                        start += rng.below(30) as u32;
                        Occurrence { position, start, end: start + 1 + rng.below(9) as u32 }
                    })
                    .collect();
                out.push(PostingEntry { doc, occurrences });
            }
        }
        out
    }

    fn norms_for(rng: &mut Rng, docs: u32) -> Vec<u8> {
        (0..docs).map(|_| norms::encode(1 + rng.below(3000) as u32)).collect()
    }

    #[test]
    fn round_trips_postings_with_occurrences() {
        let mut rng = Rng(7);
        for round in 0..40 {
            let docs = 1 + round * 37;
            let norm_of = norms_for(&mut rng, docs);
            let term = random_term(&mut rng, docs, 1 + u64::from(round) % 90);
            let mut bytes = Vec::new();
            write(&mut bytes, &term, &norm_of);
            assert_eq!(read_all(&bytes), Ok(term.clone()));
            let c = DiskCursor::new(
                &bytes,
                &norm_of,
                Bm25::DEFAULT.term(1.0, FieldStats::default(), 1),
            )
            .expect("valid");
            assert_eq!(c.doc_freq(), term.len() as u64);
        }
    }

    #[test]
    fn disk_cursor_scores_like_the_memory_list() {
        let mut rng = Rng(11);
        let docs = 20_000;
        let norm_of = norms_for(&mut rng, docs);
        let terms: Vec<_> =
            (0..4).map(|i| random_term(&mut rng, docs, [2, 10, 40, 80][i])).collect();
        let field = FieldStats { docs: u64::from(docs), total_len: u64::from(docs) * 1500 };
        let encoded: Vec<Vec<u8>> = terms
            .iter()
            .map(|t| {
                let mut b = Vec::new();
                write(&mut b, t, &norm_of);
                b
            })
            .collect();
        let memory: Vec<PostingList> = terms
            .iter()
            .map(|t| {
                PostingList::new(
                    t.iter()
                        .map(|p| Posting {
                            doc: p.doc,
                            freq: p.occurrences.len() as u32,
                            len: norms::decode(norm_of[p.doc as usize]),
                        })
                        .collect(),
                )
            })
            .collect();
        let scorers: Vec<_> =
            terms.iter().map(|t| Bm25::DEFAULT.term(1.0, field, t.len() as u64)).collect();
        for k in [1, 10, 1000] {
            let mut disk: Vec<_> = encoded
                .iter()
                .zip(&scorers)
                .map(|(b, s)| DiskCursor::new(b, &norm_of, *s).expect("valid"))
                .collect();
            let mut mem: Vec<_> = memory.iter().zip(&scorers).map(|(l, s)| l.cursor(*s)).collect();
            assert_eq!(top_k(&mut disk, k), top_k(&mut mem, k));
            assert!(disk.iter().all(|c| !c.corrupt()));
        }
        // Block bounds dominate the scores of their block, and advancing finds the right document.
        let mut c = DiskCursor::new(&encoded[2], &norm_of, scorers[2]).expect("valid");
        while c.doc() != END {
            let d = c.doc();
            let (b, last) = c.block_bound(d).expect("in a block");
            assert!(c.score() <= b && b <= c.max_score() && last >= d);
            c.advance(d + 1);
        }
        assert_eq!(c.block_bound(0).map(|x| x.1), None);
        let mut c = DiskCursor::new(&encoded[2], &norm_of, scorers[2]).expect("valid");
        let want = terms[2].iter().find(|p| p.doc >= 12_345).map_or(END, |p| p.doc);
        c.advance(12_345);
        assert_eq!(c.doc(), want);
        c.advance(0);
        assert_eq!(c.doc(), want);
    }

    #[test]
    fn empty_postings() {
        let mut bytes = Vec::new();
        write(&mut bytes, &[], &[]);
        assert_eq!(read_all(&bytes), Ok(Vec::new()));
        let c = DiskCursor::new(&bytes, &[], Bm25::DEFAULT.term(1.0, FieldStats::default(), 0))
            .expect("valid");
        assert_eq!(c.doc(), END);
        assert_eq!(c.max_score(), 0.0);
    }

    #[test]
    fn damaged_postings_never_panic() {
        let mut rng = Rng(5);
        let norm_of = norms_for(&mut rng, 600);
        let term = random_term(&mut rng, 600, 50);
        let mut bytes = Vec::new();
        write(&mut bytes, &term, &norm_of);
        let s = Bm25::DEFAULT.term(1.0, FieldStats { docs: 600, total_len: 600_000 }, 300);
        let mut rejected = 0;
        for _ in 0..4000 {
            let mut bad = bytes.clone();
            let at = rng.below(bad.len() as u64) as usize;
            bad[at] ^= 1 << rng.below(8);
            if read_all(&bad).is_err() {
                rejected += 1;
            }
            if let Ok(mut c) = DiskCursor::new(&bad, &norm_of, s) {
                while c.doc() != END {
                    let _ = c.score();
                    let d = c.doc();
                    c.advance(d + 1);
                }
            }
        }
        // Many flips only change a value (a position, an offset) and still decode: the CRC32C of
        // the section catches those; the checks here catch the structural ones.
        assert!(rejected > 0);
        // A document past the norms ends the cursor as corrupt.
        let mut c = DiskCursor::new(&bytes, &norm_of[..10], s).expect("headers are fine");
        while c.doc() != END {
            let d = c.doc();
            c.advance(d + 1);
        }
        assert!(c.corrupt());
        assert!(read_all(&bytes[..bytes.len() - 1]).is_err());
    }
}
