//! The explanation of a term's BM25 score, as `refsearch` 1.70.6 gives it with `scoreDetails`
//! (RefLib's `Explanation` of `BM25Similarity`, measured 02-11-2026,
//! `scratchpad/lane3k/m15.js`).

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

impl TextField {
    /// The explanation of `term`'s score for `doc`, with weight `boost`, named `query`.
    fn explain_term(
        &self,
        query: &str,
        term: &str,
        boost: f32,
        doc: u32,
        g: Option<&FieldStats>,
    ) -> Option<Explanation> {
        let docs = self.postings.get(term)?;
        let freq = docs.get(&doc)?.len() as f32;
        let n_docs = g.map_or(self.lengths.len() as u64, |g| g.docs);
        let df = self.df(term, docs.len(), g);
        let idf_value = idf(n_docs, df) as f32;
        let total =
            g.map_or_else(|| self.tokens.values().map(|&l| u64::from(l)).sum(), |g| g.length);
        let avg = if n_docs == 0 { 1.0f32 } else { (total as f64 / n_docs as f64) as f32 };
        let len = self.lengths.get(&doc).copied().unwrap_or(0);
        let dl = norms::decode(norms::encode(len)) as f32;
        let Bm25 { k1, b } = Bm25::DEFAULT;
        let (k1, b) = (k1 as f32, b as f32);
        let score = self.scorer(boost * idf_value, g)(doc, freq);
        let tf = freq / (freq + k1 * (1.0 - b + b * dl / avg));
        let idf_part = Explanation::node(
            idf_value,
            "idf, computed as log(1 + (N - n + 0.5) / (n + 0.5)) from:",
            vec![
                Explanation::leaf(df as f32, "n, number of documents containing term"),
                Explanation::leaf(n_docs as f32, "N, total number of documents with field"),
            ],
        );
        let tf_part = Explanation::node(
            tf,
            "tf, computed as freq / (freq + k1 * (1 - b + b * dl / avgdl)) from:",
            vec![
                Explanation::leaf(freq, "freq, occurrences of term within document"),
                Explanation::leaf(k1, "k1, term saturation parameter"),
                Explanation::leaf(b, "b, length normalization parameter"),
                Explanation::leaf(dl, "dl, length of field"),
                Explanation::leaf(avg, "avgdl, average length of field"),
            ],
        );
        let mut parts = Vec::new();
        if boost != 1.0 {
            parts.push(Explanation::leaf(boost, "boost"));
        }
        parts.push(idf_part);
        parts.push(tf_part);
        let computed = Explanation::node(
            score,
            format!("score(freq={freq:?}), computed as boost * idf * tf from:"),
            parts,
        );
        Some(Explanation::node(
            score,
            format!("{query} [BM25Similarity], result of:"),
            vec![computed],
        ))
    }
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
        let field = self.fields.get(key)?;
        field.explain_term(&format!("{name}:{term}"), term, boost, doc, self.global_of(key))
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
