//! Vector similarity of `$vectorSearch` (FR-005, FR-017): the three similarities of a
//! `vectorSearch` index, the scores `{$meta: "vectorSearchScore"}` gives for them, and the
//! quantizations of the index definition.
//!
//! The scores, measured on the reference search server 1.70.6 with and without `exact`, are `1 / (1 + d²)` for
//! `euclidean`, where `d` is the euclidean distance, `(1 + cos) / 2` for `cosine` (the cosine divided by the root of the norms' product in
//! double precision) and
//! `(1 + dot) / 2` for `dotProduct` (which may pass 1 for vectors longer than 1), as `f32`. A higher
//! score is a nearer vector.

/// The similarity of a `vector` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Similarity {
    /// `euclidean`.
    Euclidean,
    /// `cosine`.
    Cosine,
    /// `dotProduct`.
    DotProduct,
}

/// The names of the similarities in index definitions.
pub const SIMILARITIES: &[(Similarity, &str)] = &[
    (Similarity::Euclidean, "euclidean"),
    (Similarity::Cosine, "cosine"),
    (Similarity::DotProduct, "dotProduct"),
];

impl Similarity {
    /// The similarity called `name`.
    pub fn by_name(name: &str) -> Option<Similarity> {
        SIMILARITIES.iter().find(|(_, n)| *n == name).map(|(s, _)| *s)
    }

    /// The name in index definitions.
    pub fn name(self) -> &'static str {
        SIMILARITIES.iter().find(|(s, _)| *s == self).map_or("", |(_, n)| n)
    }

    /// The score of `a` against `b`.
    pub fn score(self, a: &[f32], b: &[f32]) -> f32 {
        match self {
            Similarity::Euclidean => 1.0 / (1.0 + squared_distance(a, b)),
            Similarity::Cosine => {
                // The norms multiply and take their root in double precision (measured: the
                // single-precision roots differ in the last bit).
                let (na, nb) = (f64::from(dot(a, a)), f64::from(dot(b, b)));
                let cos = match na == 0.0 || nb == 0.0 {
                    true => 0.0,
                    false => (f64::from(dot(a, b)) / (na * nb).sqrt()) as f32,
                };
                (1.0 + cos) / 2.0
            }
            Similarity::DotProduct => (1.0 + dot(a, b)) / 2.0,
        }
    }
}

/// The quantization of a `vector` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quantization {
    /// `none`: the graph compares full vectors.
    None,
    /// `scalar`: each component as a byte between the field's lowest and highest component.
    Scalar,
    /// `binary`: each component as its sign bit; candidates are scored again on full vectors.
    Binary,
}

/// The names of the quantizations in index definitions.
pub const QUANTIZATIONS: &[(Quantization, &str)] = &[
    (Quantization::None, "none"),
    (Quantization::Scalar, "scalar"),
    (Quantization::Binary, "binary"),
];

impl Quantization {
    /// The quantization called `name`.
    pub fn by_name(name: &str) -> Option<Quantization> {
        QUANTIZATIONS.iter().find(|(_, n)| *n == name).map(|(q, _)| *q)
    }

    /// The name in index definitions.
    pub fn name(self) -> &'static str {
        QUANTIZATIONS.iter().find(|(q, _)| *q == self).map_or("", |(_, n)| n)
    }
}

/// Above this length the reference search library's scalar code adds the terms in four accumulators.
const UNROLL_ABOVE: usize = 32;

/// `Σ term(i)` as the reference search library's scalar vector code adds it: fused multiply-adds, in four accumulators
/// for the first multiple of four terms of a long vector, then one by one (measured: the
/// euclidean score of an 8-dimension vector of `tests/compat/scenarios/vector-quantization.json`
/// matches the reference search server only with fused multiply-adds).
fn fused_sum(len: usize, term: impl Fn(usize) -> (f32, f32)) -> f32 {
    let mut res = 0f32;
    let mut i = 0;
    if len > UNROLL_ABOVE {
        let mut acc = [0f32; 4];
        while i + 4 <= len {
            for (k, a) in acc.iter_mut().enumerate() {
                let (x, y) = term(i + k);
                *a = x.mul_add(y, *a);
            }
            i += 4;
        }
        res += acc[0] + acc[1] + acc[2] + acc[3];
    }
    while i < len {
        let (x, y) = term(i);
        res = x.mul_add(y, res);
        i += 1;
    }
    res
}

/// The dot product of two vectors of the same length.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    fused_sum(a.len().min(b.len()), |i| (a[i], b[i]))
}

/// The squared euclidean distance of two vectors of the same length.
pub fn squared_distance(a: &[f32], b: &[f32]) -> f32 {
    fused_sum(a.len().min(b.len()), |i| {
        let d = a[i] - b[i];
        (d, d)
    })
}

/// A scored candidate: its score and the document number.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scored {
    /// The score (higher is nearer).
    pub score: f32,
    /// The document number in its segment.
    pub doc: u32,
}

/// Sorts candidates best first; equal scores keep the lower document number first.
pub fn sort_best_first(c: &mut [Scored]) {
    c.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc.cmp(&b.doc)));
}

/// `exact: true`: the `limit` best of `vectors` against `query`, among the documents `keep`
/// accepts (the prefilter).
pub fn exact(
    vectors: &[Option<Vec<f32>>],
    query: &[f32],
    similarity: Similarity,
    limit: usize,
    keep: impl Fn(u32) -> bool,
) -> Vec<Scored> {
    let mut all: Vec<Scored> = vectors
        .iter()
        .enumerate()
        .filter_map(|(doc, v)| {
            let doc = doc as u32;
            let v = v.as_ref().filter(|v| v.len() == query.len() && keep(doc))?;
            Some(Scored { score: similarity.score(v, query), doc })
        })
        .collect();
    sort_best_first(&mut all);
    all.truncate(limit);
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_like_the_reference() {
        let (a, b) = ([1.0, 0.0], [0.0, 1.0]);
        assert_eq!(Similarity::Euclidean.score(&a, &b), 1.0 / 3.0);
        assert_eq!(Similarity::Cosine.score(&a, &b), 0.5);
        assert_eq!(Similarity::Cosine.score(&a, &a), 1.0);
        assert_eq!(Similarity::Cosine.score(&a, &[0.0, 0.0]), 0.5);
        assert_eq!(Similarity::DotProduct.score(&a, &a), 1.0);
        for (s, n) in SIMILARITIES {
            assert_eq!(Similarity::by_name(n), Some(*s));
            assert_eq!(s.name(), *n);
        }
        for (q, n) in QUANTIZATIONS {
            assert_eq!(Quantization::by_name(n), Some(*q));
            assert_eq!(q.name(), *n);
        }
        assert_eq!(Similarity::by_name("manhattan"), None);
        assert_eq!(Quantization::by_name("pq"), None);
    }

    #[test]
    fn exact_search_with_prefilter() {
        let vectors = vec![Some(vec![0.0, 0.0]), None, Some(vec![1.0, 1.0]), Some(vec![3.0])];
        let top = exact(&vectors, &[1.0, 1.0], Similarity::Euclidean, 5, |_| true);
        assert_eq!(top.iter().map(|s| s.doc).collect::<Vec<_>>(), [2, 0]);
        let top = exact(&vectors, &[1.0, 1.0], Similarity::Euclidean, 5, |d| d != 2);
        assert_eq!(top.iter().map(|s| s.doc).collect::<Vec<_>>(), [0]);
        assert_eq!(exact(&vectors, &[1.0, 1.0], Similarity::Euclidean, 1, |_| true).len(), 1);
    }
}

/// A vector longer than 32 dimensions adds in four accumulators first (`m87.js` of lane 3: the
/// euclidean score of these 41 dimensions on the reference search server 1.70.6).
#[cfg(test)]
mod long_vectors {
    use super::Similarity;

    const A: [f32; 41] = [
        1.1528152,
        1.2457374,
        0.19333442,
        -1.0368195,
        -1.3137263,
        -0.38279924,
        0.9000716,
        1.3554208,
        0.5646023,
        -0.74530894,
        -1.3699865,
        -0.7351049,
        0.5756288,
        1.3571321,
        0.89089435,
        -0.39442754,
        -1.3171146,
        -1.0288526,
        0.20533177,
        1.250735,
        1.1462182,
        -0.012126294,
        -1.1593219,
        -1.2406423,
        -0.1813219,
        1.044705,
        1.310235,
        0.37114093,
        -0.90917844,
        -1.3536034,
        -0.5535316,
        0.75545454,
        1.3698792,
        0.72484326,
        -0.58661026,
        -1.358737,
        -0.8816472,
        0.40602496,
        1.3203996,
        1.020805,
        -0.21731305,
    ];
    const B: [f32; 41] = [
        0.91,
        0.6960064,
        0.1546701,
        -0.45940995,
        -0.85742235,
        -0.8521756,
        -0.44613734,
        0.16972625,
        0.70576495,
        0.90987134,
        0.6860511,
        0.13957022,
        -0.4725527,
        -0.86242664,
        -0.8466879,
        -0.4327386,
        0.18473442,
        0.715324,
        0.90948546,
        0.6759018,
        0.124430865,
        -0.4855618,
        -0.86718714,
        -0.84096086,
        -0.41921753,
        0.19969037,
        0.7246807,
        0.90884244,
        0.6655613,
        0.10925634,
        -0.49843362,
        -0.8717025,
        -0.83499604,
        -0.4055779,
        0.21458985,
        0.73383266,
        0.9079425,
        0.65503275,
        0.09405093,
        -0.51116455,
        -0.8759713,
    ];

    #[test]
    fn euclidean_score_of_41_dimensions() {
        assert_eq!(Similarity::Euclidean.score(&B, &A), 0.017_494_7_f32);
    }
}
