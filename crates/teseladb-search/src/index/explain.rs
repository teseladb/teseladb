//! The explanation of a term's BM25 score, as `refsearch` 1.70.6 gives it with `scoreDetails`
//! (RefLib's `Explanation` of `BM25Similarity`, measured 02-11-2026,
//! `scratchpad/lane3k/m15.js`).

use std::collections::BTreeMap;

use super::{FieldStats, MemIndex, TextField};
use crate::query::bm25::{Bm25, idf};
use crate::segment::norms;

/// A score and how it was computed.
#[derive(Clone, Debug, PartialEq)]
pub struct Explanation {
    /// The value, a 32-bit float.
    pub value: f32,
    /// What the value is.
    pub description: String,
    /// The values it comes from.
    pub details: Vec<Explanation>,
}

impl Explanation {
    /// A value without parts.
    pub fn leaf(value: f32, description: impl Into<String>) -> Explanation {
        Explanation { value, description: description.into(), details: Vec::new() }
    }

    /// A value made of `details`.
    pub fn node(
        value: f32,
        description: impl Into<String>,
        details: Vec<Explanation>,
    ) -> Explanation {
        Explanation { value, description: description.into(), details }
    }

    /// The document `$meta: "searchScoreDetails"` gives.
    pub fn to_document(&self) -> teseladb_bson::Document {
        let details: Vec<teseladb_bson::Value> =
            self.details.iter().map(|d| d.to_document().into()).collect();
        teseladb_bson::Document::new()
            .with("value", f64::from(self.value))
            .with("description", self.description.as_str())
            .with("details", teseladb_bson::Value::Array(details))
    }
}

/// How a weight's frequency is named in its explanation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FreqKind {
    /// A term's occurrences.
    Term,
    /// A phrase's or span's matches.
    Phrase,
    /// The occurrences of the terms of a `Synonym` query.
    Synonym,
}

/// A BM25 weight to explain: its description, the documents with each of its terms (one idf
/// each, added), its boost, and how its frequency is named.
pub(crate) struct Weight<'a> {
    /// What the query is called.
    pub description: String,
    /// Documents with each term.
    pub dfs: &'a [u64],
    /// Factor of the weight (shown when it is not 1).
    pub boost: f32,
    /// How the frequency is named.
    pub kind: FreqKind,
}

impl TextField {
    /// The explanation of `weight` for `doc` matched `freq` times.
    pub(super) fn explain_weight(
        &self,
        weight: &Weight<'_>,
        doc: u32,
        freq: f32,
        g: Option<&FieldStats>,
    ) -> Explanation {
        let n_docs = g.map_or(self.lengths.len() as u64, |g| g.docs);
        let idf_of = |df: u64| {
            Explanation::node(
                idf(n_docs, df) as f32,
                "idf, computed as log(1 + (N - n + 0.5) / (n + 0.5)) from:",
                vec![
                    Explanation::leaf(df as f32, "n, number of documents containing term"),
                    Explanation::leaf(n_docs as f32, "N, total number of documents with field"),
                ],
            )
        };
        let idf_part = match weight.dfs {
            [one] => idf_of(*one),
            many => {
                let parts: Vec<Explanation> = many.iter().map(|&df| idf_of(df)).collect();
                let sum = parts.iter().map(|p| f64::from(p.value)).sum::<f64>() as f32;
                Explanation::node(sum, "idf, sum of:", parts)
            }
        };
        let total =
            g.map_or_else(|| self.tokens.values().map(|&l| u64::from(l)).sum(), |g| g.length);
        let avg = if n_docs == 0 { 1.0f32 } else { (total as f64 / n_docs as f64) as f32 };
        let len = self.lengths.get(&doc).copied().unwrap_or(0);
        let dl = norms::decode(norms::encode(len)) as f32;
        let Bm25 { k1, b } = Bm25::DEFAULT;
        let (k1, b) = (k1 as f32, b as f32);
        let score = self.scorer(weight.boost * idf_part.value, g)(doc, freq);
        // As RefLib's scorer: one minus one over one plus the frequency times the inverse norm.
        let inverse = 1.0 / (k1 * ((1.0 - b) + b * dl / avg));
        let tf = 1.0 - 1.0 / (1.0 + freq * inverse);
        let freq_text = java_float(freq);
        let freq_leaf = match weight.kind {
            FreqKind::Term => "freq, occurrences of term within document".to_owned(),
            FreqKind::Phrase => format!("phraseFreq={freq_text}"),
            FreqKind::Synonym => format!("termFreq={freq_text}"),
        };
        let tf_part = Explanation::node(
            tf,
            "tf, computed as freq / (freq + k1 * (1 - b + b * dl / avgdl)) from:",
            vec![
                Explanation::leaf(freq, freq_leaf),
                Explanation::leaf(k1, "k1, term saturation parameter"),
                Explanation::leaf(b, "b, length normalization parameter"),
                Explanation::leaf(dl, "dl, length of field"),
                Explanation::leaf(avg, "avgdl, average length of field"),
            ],
        );
        let mut parts = Vec::new();
        if weight.boost != 1.0 {
            parts.push(Explanation::leaf(weight.boost, "boost"));
        }
        parts.push(idf_part);
        parts.push(tf_part);
        let computed = Explanation::node(
            score,
            format!("score(freq={freq_text}), computed as boost * idf * tf from:"),
            parts,
        );
        Explanation::node(
            score,
            format!("{} [BM25Similarity], result of:", weight.description),
            vec![computed],
        )
    }
}

/// `x` as Java's `Float.toString` writes it.
pub(crate) fn java_float(x: f32) -> String {
    java_number(f64::from(x), format!("{x:?}"), format!("{x:e}"))
}

/// `x` as Java's `Double.toString` writes it.
pub(crate) fn java_double(x: f64) -> String {
    java_number(x, format!("{x:?}"), format!("{x:e}"))
}

/// Java's form of a number whose shortest digits are `plain` and `sci`: plain between 10^-3
/// and 10^7, scientific with `E` outside, always with a fraction.
fn java_number(x: f64, plain: String, sci: String) -> String {
    if !x.is_finite() {
        return if x.is_nan() {
            "NaN".to_owned()
        } else if x > 0.0 {
            "Infinity".to_owned()
        } else {
            "-Infinity".to_owned()
        };
    }
    let a = x.abs();
    if a == 0.0 || (1e-3..1e7).contains(&a) {
        return if plain.contains('.') { plain } else { format!("{plain}.0") };
    }
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let mantissa =
        if mantissa.contains('.') { mantissa.to_owned() } else { format!("{mantissa}.0") };
    format!("{mantissa}E{exp}")
}

impl MemIndex {
    /// The explanation of the score of `term` of the field `key` (named `name` in the query's
    /// description, as `$type:string/t`) for document `doc`.
    pub(crate) fn explain_term(
        &self,
        key: &str,
        name: &str,
        term: &str,
        boost: f32,
        doc: u32,
    ) -> Option<Explanation> {
        self.explain_term_as(key, format!("{name}:{term}"), term, boost, doc)
    }

    /// [`MemIndex::explain_term`] with the query's whole `description`.
    pub(crate) fn explain_term_as(
        &self,
        key: &str,
        description: String,
        term: &str,
        boost: f32,
        doc: u32,
    ) -> Option<Explanation> {
        let field = self.fields.get(key)?;
        let g = self.global_of(key);
        let docs = field.postings.get(term)?;
        let freq = docs.get(&doc)?.len() as f32;
        let df = field.df(term, docs.len(), g);
        let weight = Weight { description, dfs: &[df], boost, kind: FreqKind::Term };
        Some(field.explain_weight(&weight, doc, freq, g))
    }

    /// The explanation of `weight` on the field `key` for `doc` matched `freq` times.
    pub(crate) fn explain_weight(
        &self,
        key: &str,
        weight: &Weight<'_>,
        doc: u32,
        freq: f32,
    ) -> Option<Explanation> {
        let field = self.fields.get(key)?;
        Some(field.explain_weight(weight, doc, freq, self.global_of(key)))
    }

    /// The number of documents with `term` in the field `key` (of the whole index when this
    /// one is a segment of it).
    pub(crate) fn doc_freq(&self, key: &str, term: &str) -> u64 {
        let Some(field) = self.fields.get(key) else { return 0 };
        let local = field.postings.get(term).map_or(0, BTreeMap::len);
        field.df(term, local, self.global_of(key))
    }

    /// The query terms of `text` for the field at `path`, one per position (the terms of a
    /// position that holds several are left out).
    pub(crate) fn single_terms(&self, path: &str, text: &str) -> Vec<String> {
        let Some(field) = self.fields.get(path) else { return Vec::new() };
        let Ok(a) = self.analyzer(&field.search_analyzer) else { return Vec::new() };
        let Ok(tokens) = a.analyze(text) else { return Vec::new() };
        let mut at: std::collections::BTreeMap<u32, Vec<String>> =
            std::collections::BTreeMap::new();
        for t in tokens {
            at.entry(t.position).or_default().push(t.text);
        }
        at.into_values().filter(|v| v.len() == 1).flatten().collect()
    }
}
