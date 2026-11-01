//! The HNSW graph against exact search (FR-005, FR-017): recall@10, the prefilter, the
//! quantizations, the encoding and the checks of a malformed section.

use teseladb_search::segment::hnsw::{DEFAULT_M, Hnsw};
use teseladb_search::vector::{Quantization, Similarity, exact};

/// Deterministic pseudo-random vectors (SplitMix64) with components in [-1, 1).
fn vectors(n: usize, dims: usize, seed: u64) -> Vec<Option<Vec<f32>>> {
    let mut s = seed;
    let mut next = move || {
        s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 23) as f32 * 2.0 - 1.0
    };
    (0..n).map(|_| Some((0..dims).map(|_| next()).collect())).collect()
}

/// The mean recall@10 of the graph over `queries`, with `num_candidates` = 10 × limit.
fn recall(g: &Hnsw, queries: &[Option<Vec<f32>>], keep: impl Fn(u32) -> bool + Copy) -> f64 {
    let mut hits = 0;
    let mut total = 0;
    for q in queries.iter().flatten() {
        let want: Vec<u32> =
            exact(g.vectors(), q, g.similarity(), 10, keep).iter().map(|s| s.doc).collect();
        let got: Vec<u32> = g.search(q, 100, 10, keep).iter().map(|s| s.doc).collect();
        hits += want.iter().filter(|d| got.contains(d)).count();
        total += want.len();
    }
    hits as f64 / total as f64
}

#[test]
fn recall_at_10_reaches_95_percent() {
    let data = vectors(5_000, 32, 1);
    let queries = vectors(100, 32, 2);
    for similarity in [Similarity::Euclidean, Similarity::Cosine, Similarity::DotProduct] {
        let g = Hnsw::build(32, similarity, Quantization::None, DEFAULT_M, data.clone());
        let r = recall(&g, &queries, |_| true);
        assert!(r >= 0.95, "{similarity:?}: recall {r}");
    }
}

#[test]
fn prefilter_and_quantizations() {
    let data = vectors(3_000, 16, 3);
    let queries = vectors(50, 16, 4);
    for q in [Quantization::None, Quantization::Scalar, Quantization::Binary] {
        let g = Hnsw::build(16, Similarity::Cosine, q, DEFAULT_M, data.clone());
        let r = recall(&g, &queries, |d| d % 3 == 0);
        assert!(r >= 0.9, "{q:?}: recall {r}");
        for s in g.search(queries[0].as_deref().unwrap_or(&[]), 50, 10, |d| d % 3 == 0) {
            assert_eq!(s.doc % 3, 0);
        }
    }
}

#[test]
fn scores_are_the_exact_ones_and_best_first() {
    let data = vectors(500, 8, 5);
    let g = Hnsw::build(8, Similarity::Euclidean, Quantization::None, DEFAULT_M, data.clone());
    let q = vectors(1, 8, 6).remove(0).unwrap_or_default();
    let got = g.search(&q, 500, 20, |_| true);
    let want = exact(&data, &q, Similarity::Euclidean, 20, |_| true);
    assert_eq!(got, want);
}

#[test]
fn documents_without_vectors_and_wrong_dimensions() {
    let mut data = vectors(50, 4, 7);
    data[3] = None;
    data[4] = Some(vec![1.0, 2.0]);
    data[5] = Some(vec![f32::NAN, 0.0, 0.0, 0.0]);
    let g = Hnsw::build(4, Similarity::Euclidean, Quantization::None, DEFAULT_M, data);
    let all = g.search(&[0.0; 4], 100, 100, |_| true);
    assert_eq!(all.len(), 47);
    assert!(all.iter().all(|s| ![3, 4, 5].contains(&s.doc)));
    // A query of other dimensions finds nothing.
    assert!(g.search(&[0.0; 3], 10, 10, |_| true).is_empty());
    // An empty graph.
    let empty = Hnsw::build(4, Similarity::Cosine, Quantization::None, DEFAULT_M, vec![None]);
    assert!(empty.search(&[0.0; 4], 10, 10, |_| true).is_empty());
}

#[test]
fn encoding_round_trips_and_rejects_damage() {
    let mut data = vectors(300, 6, 8);
    data[7] = None;
    let g = Hnsw::build(6, Similarity::DotProduct, Quantization::Scalar, 8, data);
    let bytes = g.encode();
    let back = Hnsw::decode(&bytes).expect("decode");
    assert_eq!(back.encode(), bytes);
    assert_eq!(
        (back.dims(), back.similarity(), back.quantization()),
        (6, Similarity::DotProduct, Quantization::Scalar)
    );
    let q = [0.5; 6];
    assert_eq!(back.search(&q, 50, 5, |_| true), g.search(&q, 50, 5, |_| true));
    for cut in [0, 5, 20, bytes.len() - 1] {
        assert!(Hnsw::decode(&bytes[..cut]).is_err(), "cut {cut}");
    }
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(Hnsw::decode(&longer).is_err());
    let mut bad = bytes.clone();
    bad[4] = 9;
    assert!(Hnsw::decode(&bad).is_err());
}
