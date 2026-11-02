//! `text` and `phrase` with `synonyms` (011 FR-003, T036), as `refsearch` 1.70.6 builds them
//! (measured 02-11-2026, `scratchpad/lane3l/m21.js`, `m22.js`). For `text`, a query of one token
//! with synonyms asks for any of them, each term with its own weight; a query of several tokens
//! is a phrase whose positions hold each token's synonyms. For `phrase`, the phrase with the
//! synonyms (each position listing the token again after its group) is added to the plain
//! phrase; a phrase of one token becomes a `Synonym` query added to the token's term query.

use std::collections::BTreeMap;

use super::{Score, SearchError};
use crate::index::{Explanation, MemIndex, MultiPhrase};

/// The factor of the plain query beside the one with synonyms in `phrase` (measured: the exact
/// words weigh twice).
const EXACT_BOOST: f32 = 2.0;

/// `text` or `phrase` with a synonym mapping.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Synonyms {
    pub(super) query: Vec<String>,
    pub(super) paths: Vec<String>,
    pub(super) mapping: String,
    /// The slop of a `phrase`; `None` for `text`.
    pub(super) phrase: Option<u32>,
    pub(super) score: Score,
}

/// A query clause of one path and one query string.
#[derive(Clone, Debug, PartialEq)]
enum Clause {
    /// One term.
    Term(String),
    /// Any of the terms, each scored alone.
    AnyTerm(Vec<String>),
    /// RefLib's `Synonym` query of the terms, then the token's term query.
    SynonymAndTerm(Vec<String>, String),
    /// A phrase.
    Phrase(MultiPhrase),
    /// The phrase with synonyms, then the plain phrase.
    PhraseAndPlain(MultiPhrase, MultiPhrase),
    /// Any of the term sequences (synonyms of several words).
    Automaton(Vec<Vec<String>>),
}

impl Synonyms {
    /// The clauses of the query, by path.
    fn clauses(&self, index: &MemIndex) -> Result<Vec<(String, Clause)>, SearchError> {
        let map = index.synonym_map(&self.mapping);
        if map.is_some_and(|m| m.is_invalid()) {
            return Err(SearchError(format!(
                "synonym mapping \"{}\" is in state INVALID and is not ready to service queries",
                self.mapping
            )));
        }
        let expand = |t: &str, keep: bool| map.and_then(|m| m.expand(t, keep));
        let mut out = Vec::new();
        for p in &self.paths {
            for q in &self.query {
                let tokens = index.query_terms(p, q)?;
                if let Some(paths) = map.and_then(|m| m.multi_word(&tokens)) {
                    out.push((p.clone(), Clause::Automaton(paths)));
                    continue;
                }
                let clause = match (self.phrase, tokens.as_slice()) {
                    (_, []) => continue,
                    (None, [t]) => match expand(t, false) {
                        Some(all) if all.len() > 1 => Clause::AnyTerm(all),
                        _ => Clause::Term(t.clone()),
                    },
                    (None, many) => Clause::Phrase(MultiPhrase {
                        positions: many
                            .iter()
                            .map(|t| expand(t, false).unwrap_or_else(|| vec![t.clone()]))
                            .collect(),
                        slop: 0,
                    }),
                    (Some(_), [t]) => match expand(t, true) {
                        Some(all) => Clause::SynonymAndTerm(all, t.clone()),
                        None => Clause::Term(t.clone()),
                    },
                    (Some(slop), many) => {
                        let plain = MultiPhrase {
                            positions: many.iter().map(|t| vec![t.clone()]).collect(),
                            slop,
                        };
                        if many.iter().any(|t| expand(t, true).is_some()) {
                            let with = MultiPhrase {
                                positions: many
                                    .iter()
                                    .map(|t| expand(t, true).unwrap_or_else(|| vec![t.clone()]))
                                    .collect(),
                                slop,
                            };
                            Clause::PhraseAndPlain(with, plain)
                        } else {
                            Clause::Phrase(plain)
                        }
                    }
                };
                out.push((p.clone(), clause));
            }
        }
        Ok(out)
    }

    /// The scores of the query, with weight `boost`.
    pub(super) fn scores(
        &self,
        index: &MemIndex,
        boost: f32,
    ) -> Result<BTreeMap<u32, f32>, SearchError> {
        let mut out: BTreeMap<u32, f64> = BTreeMap::new();
        for (path, clause) in self.clauses(index)? {
            for (doc, s) in clause_scores(index, &path, &clause, boost) {
                *out.entry(doc).or_insert(0.0) += f64::from(s);
            }
        }
        Ok(out.into_iter().map(|(d, s)| (d, s as f32)).collect())
    }

    /// The explanation of the score of `doc`, with weight `boost`; `name` gives the field name
    /// of a path.
    pub(super) fn explain(
        &self,
        index: &MemIndex,
        boost: f32,
        doc: u32,
        name: &dyn Fn(&str) -> String,
    ) -> Result<Option<Explanation>, SearchError> {
        let clauses = self.clauses(index)?;
        let mut parts = Vec::new();
        for (path, clause) in &clauses {
            if let Some(e) = explain_clause(index, path, &name(path), clause, boost, doc) {
                parts.push(e);
            }
        }
        Ok(match (clauses.len(), parts.len()) {
            (_, 0) => None,
            (1, _) => parts.pop(),
            _ => Some(sum(parts)),
        })
    }

    /// How RefLib writes the query.
    pub(super) fn description(&self, index: &MemIndex, name: &dyn Fn(&str) -> String) -> String {
        let Ok(clauses) = self.clauses(index) else { return String::new() };
        let one = |path: &str, c: &Clause| -> String {
            let n = name(path);
            match c {
                Clause::Term(t) => format!("{n}:{t}"),
                Clause::AnyTerm(ts) => {
                    ts.iter().map(|t| format!("{n}:{t}")).collect::<Vec<_>>().join(" ")
                }
                Clause::SynonymAndTerm(ts, t) => format!("{} {n}:{t}", synonym_name(&n, ts)),
                Clause::Phrase(p) => p.description(&n),
                Clause::PhraseAndPlain(w, p) => {
                    format!("{} {}", w.description(&n), p.description(&n))
                }
                Clause::Automaton(_) => "TermAutomatonQuery".to_owned(),
            }
        };
        let all: Vec<String> = clauses.iter().map(|(p, c)| one(p, c)).collect();
        match all.as_slice() {
            [single] => single.clone(),
            _ => all.iter().map(|s| format!("({s})")).collect::<Vec<_>>().join(" "),
        }
    }
}

/// A node adding `parts`.
fn sum(parts: Vec<Explanation>) -> Explanation {
    let total: f64 = parts.iter().map(|p| f64::from(p.value)).sum();
    Explanation::node(total as f32, "sum of:", parts)
}

/// How RefLib writes a `Synonym` query of `terms` on the field `name`.
fn synonym_name(name: &str, terms: &[String]) -> String {
    // RefLib's `SynonymQuery` keeps its terms sorted (measured).
    let mut sorted = terms.to_vec();
    sorted.sort();
    let all: Vec<String> = sorted.iter().map(|t| format!("{name}:{t}")).collect();
    format!("Synonym({})", all.join(" "))
}

fn clause_scores(index: &MemIndex, path: &str, c: &Clause, boost: f32) -> BTreeMap<u32, f32> {
    let add = |parts: Vec<BTreeMap<u32, f32>>| -> BTreeMap<u32, f32> {
        let mut out: BTreeMap<u32, f64> = BTreeMap::new();
        for part in parts {
            for (d, s) in part {
                *out.entry(d).or_insert(0.0) += f64::from(s);
            }
        }
        out.into_iter().map(|(d, s)| (d, s as f32)).collect()
    };
    let term = |t: &str| index.terms_scores(path, &[t.to_owned()], boost);
    let term = |t: &str| term(t).into_iter().map(|(d, s)| (d, s as f32)).collect();
    match c {
        Clause::Term(t) => term(t),
        Clause::AnyTerm(ts) => add(ts.iter().map(|t| term(t)).collect()),
        Clause::SynonymAndTerm(ts, t) => add(vec![
            index.synonym_query_scores(path, ts, boost),
            index
                .terms_scores(path, std::slice::from_ref(t), boost * EXACT_BOOST)
                .into_iter()
                .map(|(d, s)| (d, s as f32))
                .collect(),
        ]),
        Clause::Phrase(p) => index.multi_phrase_scores(path, p, boost),
        Clause::PhraseAndPlain(w, p) => add(vec![
            index.multi_phrase_scores(path, w, boost),
            index.multi_phrase_scores(path, p, boost * EXACT_BOOST),
        ]),
        Clause::Automaton(paths) => index.automaton_scores(path, paths, boost),
    }
}

fn explain_clause(
    index: &MemIndex,
    path: &str,
    name: &str,
    c: &Clause,
    boost: f32,
    doc: u32,
) -> Option<Explanation> {
    let weighted = |t: &str| {
        index.explain_term_as(path, format!("weight({name}:{t} in {doc})"), t, boost, doc)
    };
    match c {
        Clause::Term(t) => index.explain_term(path, name, t, boost, doc),
        Clause::AnyTerm(ts) => {
            let parts: Vec<Explanation> = ts.iter().filter_map(|t| weighted(t)).collect();
            (!parts.is_empty()).then(|| sum(parts))
        }
        Clause::SynonymAndTerm(ts, t) => {
            let description = format!("weight({} in {doc})", synonym_name(name, ts));
            let parts: Vec<Explanation> = [
                index.explain_synonym_query(path, description, ts, boost, doc),
                index.explain_term(path, name, t, boost * EXACT_BOOST, doc),
            ]
            .into_iter()
            .flatten()
            .collect();
            (!parts.is_empty()).then(|| sum(parts))
        }
        Clause::Phrase(p) => index.explain_multi_phrase(path, name, p, boost, doc),
        Clause::Automaton(paths) => {
            let s = *index.automaton_scores(path, paths, boost).get(&doc)?;
            Some(Explanation::leaf(s, "TermAutomatonQuery"))
        }
        Clause::PhraseAndPlain(w, p) => {
            let parts: Vec<Explanation> = [
                index.explain_multi_phrase(path, name, w, boost, doc),
                index.explain_multi_phrase(path, name, p, boost * EXACT_BOOST, doc),
            ]
            .into_iter()
            .flatten()
            .collect();
            (!parts.is_empty()).then(|| sum(parts))
        }
    }
}
