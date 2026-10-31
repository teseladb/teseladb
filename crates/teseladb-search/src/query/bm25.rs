//! BM25 and BM25F (research R2, FR-016).
//!
//! A term scores `boost · idf · tf` with `idf = ln(1 + (N − n + 0.5) / (n + 0.5))` and
//! `tf = freq / (freq + k1 · (1 − b + b · dl / avgdl))`, the formula RefDb Search documents. The
//! statistics `N`, `n` and `avgdl` are the global ones at the read timestamp (FR-015), so every
//! replica gives the same score. BM25F (Robertson, Zaragoza and Taylor, CIKM 2004) weights the
//! frequency of each field before the saturation, which is how `$text` applies its index weights.

/// The two BM25 parameters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bm25 {
    /// Term-frequency saturation.
    pub k1: f64,
    /// Length normalisation, from 0 (none) to 1 (full).
    pub b: f64,
}

impl Bm25 {
    /// `k1 = 1.2` and `b = 0.75`. FR-016 asks for the parameters of RefDb Search; T048 checks
    /// these against its `scoreDetails` on controlled corpora.
    pub const DEFAULT: Bm25 = Bm25 { k1: 1.2, b: 0.75 };

    /// The scorer of one query term over one field.
    pub fn term(&self, boost: f64, field: FieldStats, docs_with_term: u64) -> TermScorer {
        TermScorer {
            weight: boost * idf(field.docs, docs_with_term),
            k1: self.k1,
            b: self.b,
            avg_len: field.avg_len(),
        }
    }
}

/// Statistics of a field at a read timestamp, summed over every range (FR-015).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FieldStats {
    /// Documents with the field.
    pub docs: u64,
    /// Sum of the lengths of the field, in tokens.
    pub total_len: u64,
}

impl FieldStats {
    /// Mean length of the field; 1 for an empty field, so a lone document is not penalised.
    pub fn avg_len(self) -> f64 {
        if self.docs == 0 { 1.0 } else { self.total_len as f64 / self.docs as f64 }
    }
}

/// `ln(1 + (N − n + 0.5) / (n + 0.5))`; `n` above `N` (statistics from different moments) counts
/// as `N`.
pub fn idf(docs: u64, docs_with_term: u64) -> f64 {
    let n = docs_with_term.min(docs) as f64;
    (1.0 + (docs as f64 - n + 0.5) / (n + 0.5)).ln()
}

/// The score of one term in one field: `weight · tf`, with `weight = boost · idf`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TermScorer {
    weight: f64,
    k1: f64,
    b: f64,
    avg_len: f64,
}

impl TermScorer {
    /// `boost · idf`, the score a term tends to as its frequency grows.
    pub fn weight(&self) -> f64 {
        self.weight
    }

    /// The score of a document where the term appears `freq` times in a field of `len` tokens.
    /// It grows with `freq` and falls with `len`, which is what makes block bounds valid.
    pub fn score(&self, freq: u32, len: u32) -> f64 {
        let freq = f64::from(freq);
        let norm = 1.0 - self.b + self.b * f64::from(len) / self.avg_len;
        self.weight * (freq / (freq + self.k1 * norm))
    }
}

/// One field of a BM25F document: its index weight, its statistics and the term's frequency and
/// the field's length in the document.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WeightedField {
    /// Index weight of the field (`weights` of a text index).
    pub weight: f64,
    /// Statistics of the field.
    pub stats: FieldStats,
    /// Occurrences of the term in the field of the document.
    pub freq: u32,
    /// Length of the field in the document, in tokens.
    pub len: u32,
}

/// BM25F: `boost · idf · tf / (tf + k1)` over the pseudo-frequency
/// `tf = Σ weight · freq / (1 − b + b · len / avglen)` of every field, with `idf` from `docs` and
/// `docs_with_term` counted over the whole document.
pub fn bm25f(
    params: Bm25,
    boost: f64,
    docs: u64,
    docs_with_term: u64,
    fields: &[WeightedField],
) -> f64 {
    let tf: f64 = fields
        .iter()
        .map(|f| {
            let norm = 1.0 - params.b + params.b * f64::from(f.len) / f.stats.avg_len();
            f.weight * f64::from(f.freq) / norm
        })
        .sum();
    boost * idf(docs, docs_with_term) * (tf / (tf + params.k1))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    #[test]
    fn idf_matches_the_formula_and_saturates_n() {
        assert!(close(idf(10, 1), (1.0 + 9.5 / 1.5_f64).ln()));
        assert!(close(idf(10, 10), (1.0 + 0.5 / 10.5_f64).ln()));
        assert_eq!(idf(10, 20), idf(10, 10));
        assert!(idf(1000, 1) > idf(1000, 100));
    }

    #[test]
    fn term_score_is_boost_idf_tf() {
        let field = FieldStats { docs: 4, total_len: 40 };
        let s = Bm25::DEFAULT.term(2.0, field, 1);
        let tf = 3.0 / (3.0 + 1.2 * (1.0 - 0.75 + 0.75 * 5.0 / 10.0));
        assert!(close(s.score(3, 5), 2.0 * idf(4, 1) * tf));
        assert!(close(s.weight(), 2.0 * idf(4, 1)));
    }

    #[test]
    fn term_score_grows_with_freq_and_falls_with_length() {
        let s = Bm25::DEFAULT.term(1.0, FieldStats { docs: 100, total_len: 1000 }, 7);
        for len in 1..200 {
            for freq in 1..50 {
                assert!(s.score(freq + 1, len) >= s.score(freq, len));
                assert!(s.score(freq, len + 1) <= s.score(freq, len));
            }
        }
        assert!(s.score(1_000_000, 1) < s.weight());
    }

    #[test]
    fn empty_field_has_unit_average_length() {
        assert_eq!(FieldStats::default().avg_len(), 1.0);
        assert_eq!(FieldStats { docs: 4, total_len: 10 }.avg_len(), 2.5);
    }

    #[test]
    fn bm25f_with_one_unit_field_equals_the_saturated_bm25_form() {
        let stats = FieldStats { docs: 50, total_len: 500 };
        let f = WeightedField { weight: 1.0, stats, freq: 2, len: 8 };
        let p = Bm25::DEFAULT;
        let norm = 1.0 - p.b + p.b * 8.0 / 10.0;
        let tf = 2.0 / norm;
        assert!(close(bm25f(p, 1.0, 50, 3, &[f]), idf(50, 3) * tf / (tf + p.k1)));
        // The same as the BM25 term score: freq / (freq + k1 · norm) = tf / (tf + k1).
        assert!(close(bm25f(p, 1.0, 50, 3, &[f]), p.term(1.0, stats, 3).score(2, 8)));
    }

    #[test]
    fn bm25f_weights_fields_before_saturation() {
        let stats = FieldStats { docs: 10, total_len: 100 };
        let title = WeightedField { weight: 10.0, stats, freq: 1, len: 10 };
        let body = WeightedField { weight: 1.0, stats, freq: 1, len: 10 };
        let p = Bm25::DEFAULT;
        let both = bm25f(p, 1.0, 10, 2, &[title, body]);
        assert!(both > bm25f(p, 1.0, 10, 2, &[title]));
        // Saturation: the combination is below the sum of the separate scores.
        assert!(both < bm25f(p, 1.0, 10, 2, &[title]) + bm25f(p, 1.0, 10, 2, &[body]));
        assert_eq!(bm25f(p, 1.0, 10, 2, &[]), 0.0);
    }
}
