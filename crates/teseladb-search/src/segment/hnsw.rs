//! The HNSW graph of a vector field of a segment (FR-005, FR-017, research R4): the hierarchical
//! navigable small world graph of Malkov and Yashunin (arXiv:1603.09320, algorithms 1 to 5).
//!
//! Each document with a vector gets a level, drawn as `⌊−ln(u) · mL⌋` with `mL = 1 / ln(M)` and
//! `u` derived from the document number, so the same vectors always build the same graph (D-019:
//! every replica builds its own segments). Level 0 keeps up to `2M` neighbours per node and the
//! upper levels `M`; a node's neighbours are chosen with the heuristic of algorithm 4, and a
//! search walks greedily down the upper levels and then keeps `ef` candidates on level 0. The
//! prefilter is applied on level 0: rejected nodes are still walked but never returned.
//!
//! The graph compares vectors through a [`Space`]: the full vectors, or their scalar or binary
//! quantization; with a quantization, the candidates are scored again on the full vectors.
//!
//! Layout of the section payload (little-endian): `u32` dimensions, `u8` similarity, `u8`
//! quantization, `u16` M, `u32` documents, then per document a `u8` flag (1 when it has a vector)
//! and its components as `f32`; then the entry point (`u32`, `u32::MAX` for none), the top level,
//! and per document with a vector its level and per level the `u32` count and numbers of its
//! neighbours.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};

use crate::segment::Corrupt;
use crate::vector::{
    QUANTIZATIONS, Quantization, SIMILARITIES, Scored, Similarity, sort_best_first,
    squared_distance,
};

/// The default number of neighbours per node on the upper levels.
pub const DEFAULT_M: usize = 16;
/// How many more candidates the binary quantization scores again on the full vectors.
const BINARY_OVERSAMPLING: usize = 8;
/// The candidates kept while building.
const EF_CONSTRUCTION: usize = 100;

/// How the graph compares vectors.
#[derive(Clone, Debug)]
pub struct Space {
    similarity: Similarity,
    quantization: Quantization,
    /// Lowest and highest component, for the scalar quantization.
    range: (f32, f32),
}

impl Space {
    fn new(
        similarity: Similarity,
        quantization: Quantization,
        vectors: &[Option<Vec<f32>>],
    ) -> Space {
        let mut range = (f32::INFINITY, f32::NEG_INFINITY);
        for v in vectors.iter().flatten() {
            for &x in v {
                range = (range.0.min(x), range.1.max(x));
            }
        }
        if range.0 > range.1 {
            range = (0.0, 0.0);
        }
        Space { similarity, quantization, range }
    }

    /// `v` as the graph sees it.
    fn view(&self, v: &[f32]) -> Vec<f32> {
        match self.quantization {
            Quantization::None => v.to_vec(),
            Quantization::Scalar => {
                let (lo, hi) = self.range;
                let step = if hi > lo { (hi - lo) / 255.0 } else { 1.0 };
                v.iter().map(|&x| lo + ((x.clamp(lo, hi) - lo) / step).round() * step).collect()
            }
            Quantization::Binary => v.iter().map(|&x| if x > 0.0 { 1.0 } else { -1.0 }).collect(),
        }
    }

    /// How near two viewed vectors are (higher is nearer).
    fn nearness(&self, a: &[f32], b: &[f32]) -> f32 {
        match self.quantization {
            Quantization::Binary => -squared_distance(a, b),
            _ => self.similarity.score(a, b),
        }
    }
}

/// A candidate ordered by nearness, then by lower document number.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Near(f32, u32);

impl Eq for Near {}

impl Ord for Near {
    fn cmp(&self, other: &Near) -> Ordering {
        self.0.total_cmp(&other.0).then(other.1.cmp(&self.1))
    }
}

impl PartialOrd for Near {
    fn partial_cmp(&self, other: &Near) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The HNSW graph of one vector field.
#[derive(Clone, Debug)]
pub struct Hnsw {
    dims: usize,
    m: usize,
    space: Space,
    vectors: Vec<Option<Vec<f32>>>,
    views: Vec<Option<Vec<f32>>>,
    /// Per document its neighbours on each of its levels (empty without a vector).
    links: Vec<Vec<Vec<u32>>>,
    entry: Option<u32>,
}

/// A number in `(0, 1]` derived from a document number (SplitMix64).
fn unit(doc: u32) -> f64 {
    let mut z = u64::from(doc).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    ((z >> 11) as f64 + 1.0) / (1u64 << 53) as f64
}

impl Hnsw {
    /// Builds the graph of `vectors` (by document number), with `m` neighbours per node.
    pub fn build(
        dims: usize,
        similarity: Similarity,
        quantization: Quantization,
        m: usize,
        vectors: Vec<Option<Vec<f32>>>,
    ) -> Hnsw {
        let space = Space::new(similarity, quantization, &vectors);
        let views = vectors.iter().map(|v| v.as_ref().map(|v| space.view(v))).collect();
        let mut g = Hnsw {
            dims,
            m: m.max(2),
            space,
            links: vec![Vec::new(); vectors.len()],
            vectors,
            views,
            entry: None,
        };
        for doc in 0..g.vectors.len() as u32 {
            let usable = |v: &Vec<f32>| v.len() == dims && v.iter().all(|x| x.is_finite());
            if g.vectors[doc as usize].as_ref().is_some_and(usable) {
                g.insert(doc);
            }
        }
        g
    }

    /// The dimensions.
    pub fn dims(&self) -> usize {
        self.dims
    }

    /// The similarity.
    pub fn similarity(&self) -> Similarity {
        self.space.similarity
    }

    /// The quantization.
    pub fn quantization(&self) -> Quantization {
        self.space.quantization
    }

    /// The vectors, by document number.
    pub fn vectors(&self) -> &[Option<Vec<f32>>] {
        &self.vectors
    }

    fn level_of(&self, doc: u32) -> usize {
        let ml = 1.0 / (self.m as f64).ln();
        ((-unit(doc).ln() * ml).floor() as usize).min(32)
    }

    fn view(&self, doc: u32) -> &[f32] {
        self.views[doc as usize].as_deref().unwrap_or(&[])
    }

    fn top(&self) -> usize {
        self.entry.map_or(0, |e| self.links[e as usize].len() - 1)
    }

    /// Algorithm 1: inserts `doc`.
    fn insert(&mut self, doc: u32) {
        let level = self.level_of(doc);
        self.links[doc as usize] = vec![Vec::new(); level + 1];
        let Some(entry) = self.entry else {
            self.entry = Some(doc);
            return;
        };
        let q = self.view(doc).to_vec();
        let mut ep = vec![entry];
        for l in (level + 1..=self.top()).rev() {
            ep = vec![self.search_layer(&q, &ep, 1, l, &|_| true)[0].1];
        }
        for l in (0..=level.min(self.top())).rev() {
            let found = self.search_layer(&q, &ep, EF_CONSTRUCTION, l, &|_| true);
            let max = if l == 0 { 2 * self.m } else { self.m };
            let chosen = self.select(&found, self.m);
            for &n in &chosen {
                self.links[n as usize][l].push(doc);
                if self.links[n as usize][l].len() > max {
                    self.shrink(n, l, max);
                }
            }
            self.links[doc as usize][l] = chosen;
            ep = found.iter().map(|n| n.1).collect();
        }
        if level > self.top() {
            self.entry = Some(doc);
        }
    }

    /// Keeps the `max` neighbours of `n` on level `l` the heuristic chooses.
    fn shrink(&mut self, n: u32, l: usize, max: usize) {
        let q = self.view(n).to_vec();
        let cands: Vec<Near> = self.links[n as usize][l]
            .iter()
            .map(|&c| Near(self.space.nearness(&q, self.view(c)), c))
            .collect();
        self.links[n as usize][l] = self.select(&cands, max);
    }

    /// Algorithm 4: the up to `m` candidates, nearest first, each nearer to the node than to any
    /// one already chosen, then the skipped ones nearest first until there are `m`.
    fn select(&self, cands: &[Near], m: usize) -> Vec<u32> {
        let mut sorted = cands.to_vec();
        sorted.sort_by(|a, b| b.cmp(a));
        let mut chosen: Vec<u32> = Vec::with_capacity(m);
        let mut skipped = Vec::new();
        for c in sorted {
            if chosen.len() == m {
                break;
            }
            let v = self.view(c.1);
            if chosen.iter().all(|&s| self.space.nearness(v, self.view(s)) < c.0) {
                chosen.push(c.1);
            } else {
                skipped.push(c.1);
            }
        }
        chosen.extend(skipped.into_iter().take(m - chosen.len()));
        chosen
    }

    /// Algorithm 2: the `ef` nodes nearest to `q` on level `l` from the entry points `ep`, best
    /// first, among those `keep` accepts (the others are walked but not returned).
    fn search_layer(
        &self,
        q: &[f32],
        ep: &[u32],
        ef: usize,
        l: usize,
        keep: &dyn Fn(u32) -> bool,
    ) -> Vec<Near> {
        let mut visited: HashSet<u32> = ep.iter().copied().collect();
        let mut frontier: BinaryHeap<Near> = BinaryHeap::new();
        let mut found: BinaryHeap<std::cmp::Reverse<Near>> = BinaryHeap::new();
        for &e in ep {
            let near = Near(self.space.nearness(q, self.view(e)), e);
            frontier.push(near);
            if keep(e) {
                found.push(std::cmp::Reverse(near));
            }
        }
        while let Some(c) = frontier.pop() {
            let worst = found.peek().map(|r| r.0);
            if found.len() >= ef && worst.is_some_and(|w| c < w) {
                break;
            }
            for &n in self.links[c.1 as usize].get(l).map_or(&[][..], |v| &v[..]) {
                if !visited.insert(n) {
                    continue;
                }
                let near = Near(self.space.nearness(q, self.view(n)), n);
                let worst = found.peek().map(|r| r.0);
                if found.len() < ef || worst.is_some_and(|w| near > w) {
                    frontier.push(near);
                    if keep(n) {
                        found.push(std::cmp::Reverse(near));
                        if found.len() > ef {
                            found.pop();
                        }
                    }
                }
            }
        }
        let mut out: Vec<Near> = found.into_iter().map(|r| r.0).collect();
        out.sort_by(|a, b| b.cmp(a));
        out
    }

    /// Algorithm 5: the `limit` documents nearest to `query` among `num_candidates` candidates,
    /// best first, scored on the full vectors, among those `keep` accepts.
    pub fn search(
        &self,
        query: &[f32],
        num_candidates: usize,
        limit: usize,
        keep: impl Fn(u32) -> bool,
    ) -> Vec<Scored> {
        let Some(entry) = self.entry.filter(|_| query.len() == self.dims) else {
            return Vec::new();
        };
        let q = self.space.view(query);
        let mut ep = vec![entry];
        for l in (1..=self.top()).rev() {
            ep = vec![self.search_layer(&q, &ep, 1, l, &|_| true)[0].1];
        }
        // With sign bits the graph only ranks coarsely: more candidates are scored again.
        let factor =
            if self.space.quantization == Quantization::Binary { BINARY_OVERSAMPLING } else { 1 };
        let ef = num_candidates.max(limit) * factor;
        let found = self.search_layer(&q, &ep, ef, 0, &keep);
        let mut scored: Vec<Scored> = found
            .iter()
            .filter_map(|n| {
                let v = self.vectors[n.1 as usize].as_deref()?;
                Some(Scored { score: self.space.similarity.score(v, query), doc: n.1 })
            })
            .collect();
        sort_best_first(&mut scored);
        scored.truncate(limit);
        scored
    }

    /// The section payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.dims as u32).to_le_bytes());
        out.push(
            SIMILARITIES.iter().position(|(s, _)| *s == self.space.similarity).unwrap_or(0) as u8
        );
        out.push(
            QUANTIZATIONS.iter().position(|(q, _)| *q == self.space.quantization).unwrap_or(0)
                as u8,
        );
        out.extend_from_slice(&(self.m as u16).to_le_bytes());
        out.extend_from_slice(&(self.vectors.len() as u32).to_le_bytes());
        for v in &self.vectors {
            match v {
                Some(v) => {
                    out.push(1);
                    out.extend_from_slice(&(v.len() as u32).to_le_bytes());
                    v.iter().for_each(|x| out.extend_from_slice(&x.to_le_bytes()));
                }
                None => out.push(0),
            }
        }
        out.extend_from_slice(&self.entry.unwrap_or(u32::MAX).to_le_bytes());
        for levels in &self.links {
            out.extend_from_slice(&(levels.len() as u32).to_le_bytes());
            for n in levels {
                out.extend_from_slice(&(n.len() as u32).to_le_bytes());
                n.iter().for_each(|x| out.extend_from_slice(&x.to_le_bytes()));
            }
        }
        out
    }

    /// Reads a section payload written by [`Hnsw::encode`].
    pub fn decode(bytes: &[u8]) -> Result<Hnsw, Corrupt> {
        let mut r = Reader { bytes, at: 0 };
        let dims = r.u32()? as usize;
        let similarity = SIMILARITIES.get(usize::from(r.u8()?)).ok_or(BAD)?.0;
        let quantization = QUANTIZATIONS.get(usize::from(r.u8()?)).ok_or(BAD)?.0;
        let m = usize::from(r.u16()?);
        let docs = r.u32()? as usize;
        let mut vectors = Vec::with_capacity(docs.min(bytes.len()));
        for _ in 0..docs {
            vectors.push(match r.u8()? {
                0 => None,
                _ => {
                    let n = r.u32()? as usize;
                    Some((0..n).map(|_| r.u32().map(f32::from_bits)).collect::<Result<_, _>>()?)
                }
            });
        }
        let entry = Some(r.u32()?).filter(|&e| e != u32::MAX && (e as usize) < docs);
        let mut links = Vec::with_capacity(docs);
        for _ in 0..docs {
            let levels = r.u32()? as usize;
            let mut doc = Vec::with_capacity(levels.min(64));
            for _ in 0..levels {
                let n = r.u32()? as usize;
                let ns: Vec<u32> = (0..n).map(|_| r.u32()).collect::<Result<_, _>>()?;
                if ns.iter().any(|&x| x as usize >= docs) {
                    return Err(BAD);
                }
                doc.push(ns);
            }
            links.push(doc);
        }
        if r.at != bytes.len() || entry.is_some_and(|e| links[e as usize].is_empty()) {
            return Err(BAD);
        }
        let space = Space::new(similarity, quantization, &vectors);
        let views = vectors.iter().map(|v| v.as_ref().map(|v| space.view(v))).collect();
        Ok(Hnsw { dims, m, space, vectors, views, links, entry })
    }
}

/// The error of a malformed HNSW section.
const BAD: Corrupt = Corrupt("hnsw section");

/// A little-endian reader.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], Corrupt> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.bytes.len()).ok_or(BAD)?;
        let s = &self.bytes[self.at..end];
        self.at = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, Corrupt> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, Corrupt> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, Corrupt> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}
