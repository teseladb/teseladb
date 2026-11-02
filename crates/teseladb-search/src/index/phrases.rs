//! The queries synonyms make (011 T036), scored as RefLib scores them in `refsearch` 1.70.6
//! (measured 02-11-2026, `scratchpad/lane3l/m21.js`, `m22.js`): a phrase whose positions each
//! hold one or more terms (`MultiPhraseQuery`, its weight the sum of the idf of every term listed,
//! repeats included) and a `Synonym` query (the frequencies of its terms added, repeats counted
//! again, with the largest document count among them).

use std::collections::BTreeMap;

use super::explain::{FreqKind, Weight};
use super::{Explanation, FieldStats, MemIndex, TextField, phrase_freq};
use crate::query::bm25::idf;

impl TextField {
    /// The frequency of the phrase of `positions` in each document that has it. Without slop
    /// each occurrence of a term of the first position (repeats counted again) that the other
    /// positions follow is one match, as RefLib's exact phrase matcher counts them.
    fn multi_phrase_freqs(&self, positions: &[Vec<String>], slop: u32) -> Vec<(u32, f32)> {
        let unions: Option<Vec<BTreeMap<u32, Vec<u32>>>> = positions
            .iter()
            .map(|terms| {
                let mut by_doc: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
                for t in terms {
                    for (&doc, ps) in self.postings.get(t)? {
                        by_doc.entry(doc).or_default().extend(ps);
                    }
                }
                by_doc.values_mut().for_each(|ps| ps.sort_unstable());
                Some(by_doc)
            })
            .collect();
        let Some(unions) = unions.filter(|u| !u.is_empty()) else { return Vec::new() };
        let mut out = Vec::new();
        for (doc, lead) in &unions[0] {
            let lists: Option<Vec<&Vec<u32>>> = unions.iter().map(|u| u.get(doc)).collect();
            let Some(lists) = lists else { continue };
            let freq = if slop == 0 {
                lead.iter()
                    .filter(|&&p| {
                        lists
                            .iter()
                            .enumerate()
                            .skip(1)
                            .all(|(i, l)| l.binary_search(&(p + i as u32)).is_ok())
                    })
                    .count() as f32
            } else {
                let places: Vec<Vec<i64>> = lists
                    .iter()
                    .enumerate()
                    .map(|(i, l)| {
                        let mut v: Vec<i64> = l.iter().map(|&p| i64::from(p) - i as i64).collect();
                        v.dedup();
                        v
                    })
                    .collect();
                phrase_freq(&places, i64::from(slop))
            };
            if freq > 0.0 {
                out.push((*doc, freq));
            }
        }
        out
    }

    /// The number of places where one of `paths` (term sequences) starts, in each document with
    /// one.
    fn automaton_freqs(&self, paths: &[Vec<String>]) -> BTreeMap<u32, f32> {
        let mut found: BTreeMap<u32, f32> = BTreeMap::new();
        for path in paths {
            let Some(first) = path.first().and_then(|t| self.postings.get(t)) else { continue };
            for (&doc, starts) in first {
                let follows = |p: u32| {
                    path.iter().enumerate().skip(1).all(|(i, t)| {
                        self.postings
                            .get(t)
                            .and_then(|d| d.get(&doc))
                            .is_some_and(|ps| ps.contains(&(p + i as u32)))
                    })
                };
                let n = starts.iter().filter(|&&p| follows(p)).count();
                if n > 0 {
                    *found.entry(doc).or_insert(0.0) += n as f32;
                }
            }
        }
        found
    }

    /// The summed frequencies of `terms` (repeats counted again) in each document with one.
    fn synonym_freqs(&self, terms: &[String]) -> BTreeMap<u32, f32> {
        let mut freqs: BTreeMap<u32, f32> = BTreeMap::new();
        for t in terms {
            for (&doc, ps) in self.postings.get(t).into_iter().flatten() {
                *freqs.entry(doc).or_insert(0.0) += ps.len() as f32;
            }
        }
        freqs
    }

    /// The documents with each term of `terms`.
    fn dfs(&self, terms: &[String], g: Option<&FieldStats>) -> Vec<u64> {
        terms.iter().map(|t| self.df(t, self.postings.get(t).map_or(0, BTreeMap::len), g)).collect()
    }
}

/// A phrase of a query on the field `key`, named `name` in descriptions, with its slop and
/// whether it shows as a weight (`weight(… in n)`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PhraseAt<'a> {
    pub key: &'a str,
    pub name: &'a str,
    pub text: &'a str,
    pub slop: u32,
    pub weighted: bool,
}

/// A phrase query: each position's terms and the positions it may move.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MultiPhrase {
    /// Terms of each position, repeats kept.
    pub positions: Vec<Vec<String>>,
    /// Positions of slop.
    pub slop: u32,
}

impl MultiPhrase {
    /// Every term listed, in order.
    fn terms(&self) -> Vec<String> {
        self.positions.iter().flatten().cloned().collect()
    }

    /// How RefLib names it on the field `name`: `name:"(a b) c"`, `~slop` after.
    pub(crate) fn description(&self, name: &str) -> String {
        let words: Vec<String> = self
            .positions
            .iter()
            .map(|p| if p.len() == 1 { p[0].clone() } else { format!("({})", p.join(" ")) })
            .collect();
        let slop = if self.slop > 0 { format!("~{}", self.slop) } else { String::new() };
        format!("{name}:\"{}\"{slop}", words.join(" "))
    }
}

impl MemIndex {
    /// The terms of the field `key` within `fuzzy` of `term`, each with its weight.
    pub(crate) fn fuzzy_terms(
        &self,
        key: &str,
        term: &str,
        fuzzy: &super::Fuzzy,
    ) -> Vec<(String, f32)> {
        self.fields.get(key).map_or_else(Vec::new, |f| f.fuzzy_terms(term, fuzzy))
    }

    /// The explanation of fuzzy term `term`, one of `kept`, with weight `boost`, for `doc`.
    pub(crate) fn explain_fuzzy(
        &self,
        key: &str,
        description: String,
        term: &str,
        kept: &[(String, f32)],
        boost: f32,
        doc: u32,
    ) -> Option<Explanation> {
        let field = self.fields.get(key)?;
        let g = self.global_of(key);
        let freq = field.postings.get(term)?.get(&doc)?.len() as f32;
        let df = field.blended_df(kept, g);
        let weight = Weight { description, dfs: &[df], boost, kind: FreqKind::Term };
        Some(field.explain_weight(&weight, doc, freq, g))
    }

    /// The explanation of `phrase` with weight `boost` for `doc`.
    pub(crate) fn explain_phrase(
        &self,
        phrase: &PhraseAt<'_>,
        boost: f32,
        doc: u32,
    ) -> Result<Option<Explanation>, super::IndexError> {
        let PhraseAt { key, name, text, slop, weighted } = *phrase;
        let Some(field) = self.fields.get(key) else { return Ok(None) };
        let g = self.global_of(key);
        let a = self.analyzer(&field.search_analyzer)?;
        let tokens = a.analyze(text).map_err(super::analysis)?;
        // A phrase of one term is that term's query (RefLib's rewrite, measured).
        if let [one] = tokens.as_slice() {
            let t = &one.text;
            let description = if weighted {
                format!("weight({name}:{t} in {doc})")
            } else {
                format!("{name}:{t}")
            };
            return Ok(self.explain_term_as(key, description, t, boost, doc));
        }
        let Some((dfs, freqs)) = field.phrase_freqs(&tokens, slop, g) else { return Ok(None) };
        let Some(&(_, freq)) = freqs.iter().find(|f| f.0 == doc) else { return Ok(None) };
        let words: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
        let slop_text = if slop > 0 { format!("~{slop}") } else { String::new() };
        let query = format!("{name}:\"{}\"{slop_text}", words.join(" "));
        let description = if weighted { format!("weight({query} in {doc})") } else { query };
        let weight = Weight { description, dfs: &dfs, boost, kind: FreqKind::Phrase };
        Ok(Some(field.explain_weight(&weight, doc, freq, g)))
    }

    /// The scores of `phrase` on the field `key`, with weight `boost`.
    pub(crate) fn multi_phrase_scores(
        &self,
        key: &str,
        phrase: &MultiPhrase,
        boost: f32,
    ) -> BTreeMap<u32, f32> {
        let Some(field) = self.fields.get(key) else { return BTreeMap::new() };
        let g = self.global_of(key);
        let n = g.map_or(field.lengths.len() as u64, |g| g.docs);
        let idf_sum: f32 =
            field.dfs(&phrase.terms(), g).into_iter().map(|df| idf(n, df) as f32).sum();
        let score = field.scorer(boost * idf_sum, g);
        field
            .multi_phrase_freqs(&phrase.positions, phrase.slop)
            .into_iter()
            .map(|(doc, f)| (doc, score(doc, f)))
            .collect()
    }

    /// The explanation of `phrase` on the field `key` for `doc`, named `name`.
    pub(crate) fn explain_multi_phrase(
        &self,
        key: &str,
        name: &str,
        phrase: &MultiPhrase,
        boost: f32,
        doc: u32,
    ) -> Option<Explanation> {
        let field = self.fields.get(key)?;
        let g = self.global_of(key);
        let (_, freq) = field
            .multi_phrase_freqs(&phrase.positions, phrase.slop)
            .into_iter()
            .find(|f| f.0 == doc)?;
        let dfs = field.dfs(&phrase.terms(), g);
        let weight = Weight {
            description: phrase.description(name),
            dfs: &dfs,
            boost,
            kind: FreqKind::Phrase,
        };
        Some(field.explain_weight(&weight, doc, freq, g))
    }

    /// The scores of RefLib's `TermAutomatonQuery` of `paths` on the field `key` (measured: the
    /// sum of the idf of its distinct terms, its frequency the number of matches).
    pub(crate) fn automaton_scores(
        &self,
        key: &str,
        paths: &[Vec<String>],
        boost: f32,
    ) -> BTreeMap<u32, f32> {
        let Some(field) = self.fields.get(key) else { return BTreeMap::new() };
        let g = self.global_of(key);
        let n = g.map_or(field.lengths.len() as u64, |g| g.docs);
        let mut terms: Vec<String> = Vec::new();
        for t in paths.iter().flatten() {
            if !terms.contains(t) {
                terms.push(t.clone());
            }
        }
        let idf_sum: f32 = field.dfs(&terms, g).into_iter().map(|df| idf(n, df) as f32).sum();
        let score = field.scorer(boost * idf_sum, g);
        field.automaton_freqs(paths).into_iter().map(|(d, f)| (d, score(d, f))).collect()
    }

    /// The scores of RefLib's `Synonym` query of `terms` on the field `key`, with weight
    /// `boost`.
    pub(crate) fn synonym_query_scores(
        &self,
        key: &str,
        terms: &[String],
        boost: f32,
    ) -> BTreeMap<u32, f32> {
        let Some(field) = self.fields.get(key) else { return BTreeMap::new() };
        let g = self.global_of(key);
        let n = g.map_or(field.lengths.len() as u64, |g| g.docs);
        let df = field.dfs(terms, g).into_iter().max().unwrap_or(0);
        let score = field.scorer(boost * idf(n, df) as f32, g);
        field.synonym_freqs(terms).into_iter().map(|(doc, f)| (doc, score(doc, f))).collect()
    }

    /// The explanation of the `Synonym` query of `terms` for `doc`, named `name`.
    pub(crate) fn explain_synonym_query(
        &self,
        key: &str,
        description: String,
        terms: &[String],
        boost: f32,
        doc: u32,
    ) -> Option<Explanation> {
        let field = self.fields.get(key)?;
        let g = self.global_of(key);
        let freq = *field.synonym_freqs(terms).get(&doc)?;
        let df = field.dfs(terms, g).into_iter().max().unwrap_or(0);
        let weight = Weight { description, dfs: &[df], boost, kind: FreqKind::Synonym };
        Some(field.explain_weight(&weight, doc, freq, g))
    }
}
