//! The `autocomplete` operator (011 FR-002, FR-003) as `refsearch` 1.70.6 runs it (measured
//! 02-11-2026 with `scoreDetails`, `scratchpad/lane3j/m6.js`): the grams of an `autocomplete`
//! field are those of its whole value and of each of its words, and a query asks for the first
//! `maxGrams` characters of each of its words and of its whole text (`tokenOrder: "any"`), or
//! of its whole text alone (`"sequential"`), each a term query added to the others; with
//! `"any"` the query also runs as `text` on the field's `string` mapping when it has one. With `fuzzy` a
//! document holding a gram within the edits scores 1.

use std::collections::BTreeMap;

use teseladb_bson::{Document, Value};

use super::{Operator, Score, SearchError, parse_fuzzy, parse_score, shape};
use crate::definition::FieldMapping;
use crate::index::{Fuzzy, Grams, MemIndex, autocomplete_key};
use crate::query::fuzzy::Levenshtein;

/// An `autocomplete` operator.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Autocomplete {
    query: Vec<String>,
    path: String,
    sequential: bool,
    fuzzy: Option<Fuzzy>,
    pub(super) score: Score,
}

/// `autocomplete` with its options `d` at `path` of the query.
pub(super) fn parse(d: &Document, path: &str) -> Result<Operator, SearchError> {
    const OPTIONS: [&str; 5] = ["query", "path", "tokenOrder", "fuzzy", "score"];
    if let Some((k, _)) = d.iter().find(|(k, _)| !OPTIONS.contains(k)) {
        return Err(SearchError(format!("\"{path}\" unrecognized field \"{k}\"")));
    }
    let at = |f: &str| format!("{path}.{f}");
    let query = super::queries(d, &at("query"))?;
    let field = match d.get("path") {
        None => return Err(shape(&at("path"), "is required")),
        Some(Value::String(p)) => p.clone(),
        Some(_) => return Err(shape(&at("path"), "must be a string")),
    };
    let sequential = match d.get("tokenOrder") {
        None => false,
        Some(Value::String(o)) if o == "any" => false,
        Some(Value::String(o)) if o == "sequential" => true,
        Some(_) => return Err(shape(&at("tokenOrder"), "must be one of [any, sequential]")),
    };
    let fuzzy = parse_fuzzy(d.get("fuzzy"), &at("fuzzy"))?;
    let score = parse_score(d.get("score"), &at("score"))?;
    Ok(Operator::Autocomplete(Autocomplete { query, path: field, sequential, fuzzy, score }))
}

impl Autocomplete {
    /// The grams of the field and its analyzer.
    fn field(&self, index: &MemIndex) -> Result<(Grams, String), SearchError> {
        let types = super::values::types_at(index.definition(), &self.path);
        types
            .into_iter()
            .find_map(|t| match t {
                FieldMapping::Autocomplete {
                    min_grams,
                    max_grams,
                    fold_diacritics,
                    tokenization,
                    analyzer,
                } => Some((
                    Grams {
                        min: usize::try_from(min_grams).unwrap_or(0),
                        max: usize::try_from(max_grams).unwrap_or(0),
                        fold: fold_diacritics,
                        edge: tokenization == "edgeGram",
                    },
                    analyzer.unwrap_or_else(|| {
                        index
                            .definition()
                            .analyzer
                            .clone()
                            .unwrap_or_else(|| crate::definition::DEFAULT_ANALYZER.to_owned())
                    }),
                )),
                _ => None,
            })
            .ok_or_else(|| {
                SearchError(format!(
                    "autocomplete index field definition not present at path {}",
                    self.path
                ))
            })
    }

    /// The scores of the documents the query matches, with weight `boost`.
    pub(super) fn scores(
        &self,
        index: &MemIndex,
        boost: f32,
    ) -> Result<BTreeMap<u32, f32>, SearchError> {
        let (g, analyzer) = self.field(index)?;
        let key = autocomplete_key(&self.path);
        let mut out: BTreeMap<u32, f32> = BTreeMap::new();
        for q in &self.query {
            let whole = g.normalized(q);
            if let Some(f) = &self.fuzzy {
                let Some(term) = g.term(&whole) else { continue };
                let lev = Levenshtein::new(&term, f.max_edits, f.prefix_length);
                for doc in index.matching_terms(&key, |t| lev.distance(t).is_some()) {
                    *out.entry(doc).or_insert(0.0) += boost;
                }
                continue;
            }
            let words: Vec<String> =
                index.analyze(&analyzer, q)?.into_iter().map(|w| g.normalized(&w)).collect();
            let mut terms: Vec<String> = Vec::new();
            if !self.sequential {
                for t in words.iter().filter_map(|w| g.term(w)) {
                    if !terms.contains(&t) {
                        terms.push(t);
                    }
                }
            }
            if let Some(t) = g.term(&whole).filter(|t| !terms.contains(t)) {
                terms.push(t);
            }
            for (doc, s) in index.terms_scores(&key, &terms, boost) {
                *out.entry(doc).or_insert(0.0) += s;
            }
            if !self.sequential && index.has_text(&self.path) {
                let paths = [self.path.clone()];
                for (doc, s) in index.text_scores(std::slice::from_ref(q), &paths, boost)? {
                    *out.entry(doc).or_insert(0.0) += s;
                }
            }
        }
        Ok(out)
    }
}
