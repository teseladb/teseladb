//! `text` and `phrase` with `synonyms` (011 FR-003, T036), as `refsearch` 1.70.6 builds them
//! (measured 02-11-2026, `scratchpad/lane3l/m21.js`, `m22.js`). For `text`, a query of one token
//! with synonyms asks for any of them, each term with its own weight; a query of several tokens
//! is a phrase whose positions hold each token's synonyms. For `phrase`, the phrase with the
//! synonyms (each position listing the token again after its group) is added to the plain
//! phrase; a phrase of one token becomes a `Synonym` query added to the token's term query.
//! With `matchCriteria` (measured `scratchpad/lane3m/m27.js`, `synonyms_all.js`), `text` of
//! several tokens adds two queries: one with a clause per position (the token, its `Synonym`
//! query, or the `DisjunctionMax` of the entries of a rule of several words) and one with the
//! token terms at twice the weight; `all` asks for every clause of each.

use std::collections::BTreeMap;

use super::{Score, SearchError};
use crate::index::{Explanation, MemIndex, MultiPhrase, SynonymSegment as Segment, java_float};

/// The factor of the plain query beside the one with synonyms in `phrase` (measured: the exact
/// words weigh twice).
const EXACT_BOOST: f32 = 2.0;
/// The factor of the scores other than the best in a `DisjunctionMax` (measured).
const TIE_BREAKER: f32 = 0.1;

/// `text` or `phrase` with a synonym mapping.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Synonyms {
    pub(super) query: Vec<String>,
    pub(super) paths: Vec<String>,
    pub(super) mapping: String,
    /// The slop of a `phrase`; `None` for `text`.
    pub(super) phrase: Option<u32>,
    /// `matchCriteria` of a `text` when given: whether it is `all`.
    pub(super) criteria: Option<bool>,
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
    /// `text` of several tokens with `matchCriteria`: the clause of each position, then the
    /// tokens at [`EXACT_BOOST`]; with `all`, every clause of each is required.
    Criteria { parts: Vec<Part>, exact: Vec<String>, all: bool },
}

/// The clause of one position of [`Clause::Criteria`].
#[derive(Clone, Debug, PartialEq)]
enum Part {
    Term(String),
    Synonym(Vec<String>),
    /// The `DisjunctionMax` of word sequences.
    Max(Vec<Vec<String>>),
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
                if let (Some(all), Some(m)) = (self.criteria, map)
                    && !tokens.is_empty()
                {
                    out.push((p.clone(), criteria_clause(m.segments(&tokens), tokens, all)));
                    continue;
                }
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
                Clause::Criteria { parts, exact, all } => {
                    criteria_description(&n, parts, exact, *all)
                }
            }
        };
        let all: Vec<String> = clauses.iter().map(|(p, c)| one(p, c)).collect();
        match all.as_slice() {
            [single] => single.clone(),
            _ => all.iter().map(|s| format!("({s})")).collect::<Vec<_>>().join(" "),
        }
    }
}

/// How RefLib writes [`Clause::Criteria`] on the field `name` under `ConstantScore` (measured,
/// `scratchpad/lane3m/m31.js`): with `all`, both Boolean queries with their clauses as filters
/// (`#`); with `any`, one disjunction of the distinct clauses.
fn criteria_description(name: &str, parts: &[Part], exact: &[String], all: bool) -> String {
    let sequence = |w: &[String]| match w {
        [one] => format!("{name}:{one}"),
        _ => phrase_of(w).description(name),
    };
    let part = |p: &Part| match p {
        Part::Term(t) => format!("{name}:{t}"),
        Part::Synonym(ts) => synonym_name(name, ts),
        Part::Max(alternatives) => {
            let each: Vec<String> = alternatives.iter().map(|w| sequence(w)).collect();
            format!("({})~{}", each.join(" | "), java_float(TIE_BREAKER))
        }
    };
    let parts: Vec<String> = parts.iter().map(part).collect();
    let exact: Vec<String> = exact.iter().map(|t| format!("{name}:{t}")).collect();
    if all {
        let filters = |v: &[String]| -> String {
            let each: Vec<String> = v.iter().map(|x| format!("#{x}")).collect();
            format!("({})", each.join(" "))
        };
        return format!("{} {}", filters(&parts), filters(&exact));
    }
    let mut distinct: Vec<String> = Vec::new();
    for x in parts.into_iter().chain(exact) {
        if !distinct.contains(&x) {
            distinct.push(x);
        }
    }
    distinct.join(" ")
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
        Clause::Criteria { parts, exact, all } => {
            let parts: Vec<BTreeMap<u32, f32>> =
                parts.iter().map(|p| part_scores(index, path, p, boost)).collect();
            let exact: Vec<BTreeMap<u32, f32>> = exact
                .iter()
                .map(|t| part_scores(index, path, &Part::Term(t.clone()), boost * EXACT_BOOST))
                .collect();
            add(vec![bool_scores(parts, *all, &add), bool_scores(exact, *all, &add)])
        }
    }
}

/// The scores of a Boolean query of `clauses`: their sum, only in the documents every clause
/// matches when `all`.
fn bool_scores(
    clauses: Vec<BTreeMap<u32, f32>>,
    all: bool,
    add: &dyn Fn(Vec<BTreeMap<u32, f32>>) -> BTreeMap<u32, f32>,
) -> BTreeMap<u32, f32> {
    let keep: Option<Vec<u32>> = all.then(|| {
        let first = clauses.first().map(|c| c.keys().copied().collect()).unwrap_or_default();
        clauses
            .iter()
            .skip(1)
            .fold(first, |acc: Vec<u32>, c| acc.into_iter().filter(|d| c.contains_key(d)).collect())
    });
    let mut out = add(clauses);
    if let Some(keep) = keep {
        out.retain(|d, _| keep.contains(d));
    }
    out
}

/// The scores of one position of [`Clause::Criteria`].
fn part_scores(index: &MemIndex, path: &str, part: &Part, boost: f32) -> BTreeMap<u32, f32> {
    match part {
        Part::Term(t) => index
            .terms_scores(path, std::slice::from_ref(t), boost)
            .into_iter()
            .map(|(d, s)| (d, s as f32))
            .collect(),
        Part::Synonym(ts) => index.synonym_query_scores(path, ts, boost),
        Part::Max(alternatives) => {
            let mut each: BTreeMap<u32, Vec<f32>> = BTreeMap::new();
            for words in alternatives {
                for (d, s) in sequence_scores(index, path, words, boost) {
                    each.entry(d).or_default().push(s);
                }
            }
            each.into_iter().map(|(d, v)| (d, max_plus(&v))).collect()
        }
    }
}

/// The scores of one word (a term query) or of several (a phrase).
fn sequence_scores(
    index: &MemIndex,
    path: &str,
    words: &[String],
    boost: f32,
) -> BTreeMap<u32, f32> {
    match words {
        [one] => index
            .terms_scores(path, std::slice::from_ref(one), boost)
            .into_iter()
            .map(|(d, s)| (d, s as f32))
            .collect(),
        _ => index.multi_phrase_scores(path, &phrase_of(words), boost),
    }
}

fn phrase_of(words: &[String]) -> MultiPhrase {
    MultiPhrase { positions: words.iter().map(|w| vec![w.clone()]).collect(), slop: 0 }
}

/// RefLib's `DisjunctionMax` score of `scores`: the best plus [`TIE_BREAKER`] times the others,
/// summed in doubles.
fn max_plus(scores: &[f32]) -> f32 {
    let mut max = 0f32;
    let mut others = 0f64;
    for &s in scores {
        if s > max {
            others += f64::from(max);
            max = s;
        } else {
            others += f64::from(s);
        }
    }
    (f64::from(max) + others * f64::from(TIE_BREAKER)) as f32
}

/// The clause of `tokens` (one query string on one path) with `matchCriteria`: a token alone
/// asks as without it; several ask for [`Clause::Criteria`].
fn criteria_clause(segments: Vec<Segment>, tokens: Vec<String>, all: bool) -> Clause {
    if let [Segment::Word(t, expanded)] = segments.as_slice() {
        return match expanded {
            Some(ts) => Clause::SynonymAndTerm(ts.clone(), t.clone()),
            None => Clause::Term(t.clone()),
        };
    }
    let parts = segments
        .into_iter()
        .map(|s| match s {
            Segment::Word(t, None) => Part::Term(t),
            Segment::Word(_, Some(ts)) => Part::Synonym(ts),
            Segment::Group(alternatives) => Part::Max(alternatives),
        })
        .collect();
    Clause::Criteria { parts, exact: tokens, all }
}

/// The explanation of the term query of `t`: `weight(… in doc)` when `weighted`, the bare query
/// otherwise (measured: the bare form unless a weight other than 1 reaches a Boolean query).
fn explain_leaf(
    index: &MemIndex,
    path: &str,
    name: &str,
    t: &str,
    boost: f32,
    weighted: bool,
    doc: u32,
) -> Option<Explanation> {
    if weighted {
        index.explain_term_as(path, format!("weight({name}:{t} in {doc})"), t, boost, doc)
    } else {
        index.explain_term(path, name, t, boost, doc)
    }
}

/// The explanation of one word or of a phrase of several, as [`explain_leaf`].
fn explain_sequence(
    index: &MemIndex,
    path: &str,
    name: &str,
    words: &[String],
    boost: f32,
    weighted: bool,
    doc: u32,
) -> Option<Explanation> {
    match words {
        [one] => explain_leaf(index, path, name, one, boost, weighted, doc),
        _ => {
            let phrase = phrase_of(words);
            let text = phrase.description(name);
            let text = if weighted { format!("weight({text} in {doc})") } else { text };
            index.explain_multi_phrase_as(path, text, &phrase, boost, doc)
        }
    }
}

/// The explanation of a Boolean query of `clauses` (`None` for a clause that does not match):
/// the clause itself when it is the only one.
fn explain_bool(mut clauses: Vec<Option<Explanation>>, all: bool) -> Option<Explanation> {
    if clauses.len() == 1 {
        return clauses.pop().flatten();
    }
    if all && clauses.iter().any(Option::is_none) {
        return None;
    }
    let parts: Vec<Explanation> = clauses.into_iter().flatten().collect();
    (!parts.is_empty()).then(|| sum(parts))
}

/// The explanation of [`Clause::Criteria`] for `doc`.
fn explain_criteria(
    index: &MemIndex,
    path: &str,
    name: &str,
    (parts, exact, all): (&[Part], &[String], bool),
    boost: f32,
    doc: u32,
) -> Option<Explanation> {
    let weighted = boost != 1.0;
    let part = |p: &Part| -> Option<Explanation> {
        match p {
            Part::Term(t) => explain_leaf(index, path, name, t, boost, weighted, doc),
            Part::Synonym(ts) => {
                let description = format!("weight({} in {doc})", synonym_name(name, ts));
                index.explain_synonym_query(path, description, ts, boost, doc)
            }
            Part::Max(alternatives) => {
                let each: Vec<Explanation> = alternatives
                    .iter()
                    .filter_map(|w| explain_sequence(index, path, name, w, boost, weighted, doc))
                    .collect();
                let values: Vec<f32> = each.iter().map(|e| e.value).collect();
                (!each.is_empty()).then(|| {
                    let text = format!("max plus {} times others of:", java_float(TIE_BREAKER));
                    Explanation::node(max_plus(&values), text, each)
                })
            }
        }
    };
    let syn = explain_bool(parts.iter().map(part).collect(), all);
    let exact = match exact {
        [one] => explain_leaf(index, path, name, one, boost * EXACT_BOOST, weighted, doc),
        many => explain_bool(
            many.iter()
                .map(|t| explain_leaf(index, path, name, t, boost * EXACT_BOOST, true, doc))
                .collect(),
            all,
        ),
    };
    let found: Vec<Explanation> = [syn, exact].into_iter().flatten().collect();
    (!found.is_empty()).then(|| sum(found))
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
                explain_leaf(index, path, name, t, boost * EXACT_BOOST, boost != 1.0, doc),
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
        Clause::Criteria { parts, exact, all } => {
            explain_criteria(index, path, name, (parts, exact, *all), boost, doc)
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
