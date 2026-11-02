//! The `autocomplete` operator (011 FR-002, FR-003) as `refsearch` 1.70.6 runs it (measured
//! 02-11-2026 with `scoreDetails`, `scratchpad/lane3j/m6.js`, `scratchpad/lane3k/m10.js` to
//! `m13.js`): an `autocomplete` field holds the grams of each word of its value and of each run
//! of two or three words (`edgeGram` from the start, `rightEdgeGram` from the end, kept
//! reversed, `nGram` everywhere), and a query asks for the first `maxGrams` characters (the last
//! ones, reversed, for `rightEdgeGram`) of each of its words and of its whole text
//! (`tokenOrder: "any"`), or of its whole text alone (`"sequential"`), each a term query of one
//! disjunction; with `"any"` the query also runs as `text` on the field's `string` mapping when
//! it has one. With `fuzzy` a document holding a gram within the edits scores 1.

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
                    Grams::new(min_grams, max_grams, fold_diacritics, &tokenization),
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

    /// The grams query `q` asks for: those of each of its words (unless `sequential`) and of
    /// its whole text, without repeats.
    fn gram_terms(
        &self,
        index: &MemIndex,
        g: &Grams,
        analyzer: &str,
        q: &str,
    ) -> Result<Vec<String>, SearchError> {
        let whole = g.normalized(q);
        let words: Vec<String> =
            index.analyze(analyzer, q)?.into_iter().map(|w| g.normalized(&w)).collect();
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
        Ok(terms)
    }

    /// The explanation of the score of `doc`, with weight `boost` (measured: a sum of the grams'
    /// disjunction and of the `text` query, nested once more for several queries; with `fuzzy`
    /// RefLib's automaton query, named by its identity, here a hash of its term, DV-083).
    pub(super) fn explain(
        &self,
        ctx: &super::explain::Ctx<'_>,
        boost: f32,
        doc: u32,
    ) -> Result<Option<crate::index::Explanation>, SearchError> {
        use crate::index::Explanation;
        let (g, analyzer) = self.field(ctx.index)?;
        let key = autocomplete_key(&self.path);
        let name = format!("{}{key}", ctx.prefix);
        let mut units = Vec::new();
        for q in &self.query {
            let mut clauses = Vec::new();
            if let Some(f) = &self.fuzzy {
                let Some(term) = g.term(&g.normalized(q)) else { continue };
                let lev = Levenshtein::new(&term, f.max_edits, f.prefix_length);
                if ctx.index.matching_terms(&key, |t| lev.distance(t).is_some()).contains(&doc) {
                    let hash = term
                        .bytes()
                        .fold(0x811c_9dc5u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193));
                    clauses.push(Explanation::leaf(
                        boost,
                        format!(
                            "{name}:AutomatonQuery {{\norg.apache.reflib.util.automaton.Automaton@{hash:x}}}"
                        ),
                    ));
                }
                let unit = if self.sequential {
                    clauses.pop()
                } else {
                    (!clauses.is_empty()).then(|| super::explain::sum(clauses))
                };
                units.extend(unit);
                continue;
            }
            let terms = self.gram_terms(ctx.index, &g, &analyzer, q)?;
            let found: Vec<Explanation> = terms
                .iter()
                .filter_map(|t| {
                    let description = ctx.term_description(&name, t, doc);
                    ctx.index.explain_term_as(&key, description, t, boost, doc)
                })
                .collect();
            let grams = match (terms.len(), found.len()) {
                (_, 0) => None,
                (1, _) => found.into_iter().next(),
                _ => Some(super::explain::sum(found)),
            };
            let with_text = !self.sequential && ctx.index.has_text(&self.path);
            if !with_text {
                units.extend(grams);
                continue;
            }
            clauses.extend(grams);
            let text = Operator::Text {
                query: vec![q.clone()],
                paths: vec![self.path.clone()],
                fuzzy: None,
                score: if boost == 1.0 { Score::Plain } else { Score::Boost(boost) },
            };
            clauses.extend(super::explain::explain(ctx, &text, doc, 0.0)?);
            if !clauses.is_empty() {
                units.push(super::explain::sum(clauses));
            }
        }
        Ok(match units.len() {
            0 => None,
            1 if self.query.len() == 1 => units.pop(),
            _ => Some(super::explain::sum(units)),
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
        let mut out: BTreeMap<u32, f64> = BTreeMap::new();
        for q in &self.query {
            let whole = g.normalized(q);
            if let Some(f) = &self.fuzzy {
                let Some(term) = g.term(&whole) else { continue };
                let lev = Levenshtein::new(&term, f.max_edits, f.prefix_length);
                for doc in index.matching_terms(&key, |t| lev.distance(t).is_some()) {
                    *out.entry(doc).or_insert(0.0) += f64::from(boost);
                }
                continue;
            }
            let terms = self.gram_terms(index, &g, &analyzer, q)?;
            // The grams are one disjunction, a clause of the query with `text` (measured with
            // `scoreDetails`: two nested sums, each in doubles and then rounded to a float).
            for (doc, s) in index.terms_scores(&key, &terms, boost) {
                *out.entry(doc).or_insert(0.0) += f64::from(s as f32);
            }
            if !self.sequential && index.has_text(&self.path) {
                let paths = [self.path.clone()];
                for (doc, s) in index.text_scores(std::slice::from_ref(q), &paths, boost)? {
                    *out.entry(doc).or_insert(0.0) += f64::from(s);
                }
            }
        }
        Ok(out.into_iter().map(|(doc, s)| (doc, s as f32)).collect())
    }
}
