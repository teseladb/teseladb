//! The documents of a search index in memory (011 FR-009, FR-016): each string field analyzed
//! by its mapping, its postings and its lengths, and the `text` operator scored as `refsearch`
//! 1.70.6 scores it. Measured on a 9.0.4 replica set (01-11-2026): a term scores
//! `w − w / (1 + freq · n)` in 32-bit floats, with `w = boost · idf` and
//! `n = 1 / (k1 · (1 − b + b · dl / avgdl))`, `dl` the field's length through its norm byte and
//! `avgdl` the exact mean length as a 32-bit float; the terms and paths of a query add up in their
//! order.

use std::collections::BTreeMap;

use teseladb_bson::{Document, Value};

use crate::analysis::analyzer::Analyzer;
use crate::definition::{DEFAULT_ANALYZER, Definition, Dynamic, FieldMapping, Mapping};
use crate::query::bm25::{Bm25, idf};
use crate::segment::norms;

/// A string field of the index: the postings of each term (document number to positions) and the
/// length of the field in each document.
#[derive(Clone, Debug, Default)]
struct TextField {
    postings: BTreeMap<String, BTreeMap<u32, Vec<u32>>>,
    lengths: BTreeMap<u32, u32>,
    /// Query analyzer.
    search_analyzer: String,
}

/// Why a document or a query cannot be analyzed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexError {
    /// An analyzer the definition names and this engine cannot build.
    Analyzer(String),
    /// A regular expression of an analyzer ran out of steps.
    Analysis,
}

/// The documents of one search index.
#[derive(Clone, Debug)]
pub struct MemIndex {
    definition: Definition,
    keys: Vec<Option<Vec<u8>>>,
    docs: Vec<Option<Document>>,
    by_key: BTreeMap<Vec<u8>, u32>,
    fields: BTreeMap<String, TextField>,
    /// The statistics of the whole index when this one is a segment of it: its scores then use
    /// them instead of its own (FR-015, FR-016).
    global: Option<Stats>,
}

/// What the scores of a string field depend on: its number of documents, the sum of their
/// lengths and the number of documents with each term.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FieldStats {
    /// Documents with the field.
    pub docs: u64,
    /// Sum of their lengths.
    pub length: u64,
    /// Documents with each term.
    pub df: BTreeMap<String, u64>,
}

/// The statistics of the string fields of an index, by path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// By path.
    pub fields: BTreeMap<String, FieldStats>,
}

impl Stats {
    /// The statistics of `self` and `other` together (segments with no document in common).
    pub fn plus(&self, other: &Stats) -> Stats {
        let mut out = self.clone();
        for (path, f) in &other.fields {
            let into = out.fields.entry(path.clone()).or_default();
            into.docs += f.docs;
            into.length += f.length;
            for (term, n) in &f.df {
                *into.df.entry(term.clone()).or_insert(0) += n;
            }
        }
        out
    }
}

/// A document the index found, with its score.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    /// Key of the document.
    pub key: Vec<u8>,
    /// Score, a 32-bit float as `refsearch` gives it.
    pub score: f32,
}

/// The statistics of a field no segment has.
static EMPTY_FIELD: FieldStats = FieldStats { docs: 0, length: 0, df: BTreeMap::new() };

/// The built-in analyzer called `name`.
fn analyzer(name: &str) -> Result<Analyzer, IndexError> {
    Analyzer::builtin(name).ok_or_else(|| IndexError::Analyzer(name.to_owned()))
}

impl MemIndex {
    /// An empty index of `definition`.
    pub fn new(definition: Definition) -> MemIndex {
        MemIndex {
            definition,
            keys: Vec::new(),
            docs: Vec::new(),
            by_key: BTreeMap::new(),
            fields: BTreeMap::new(),
            global: None,
        }
    }

    /// The statistics of this index's own documents.
    pub fn stats(&self) -> Stats {
        let fields = self
            .fields
            .iter()
            .map(|(path, f)| {
                let df = f.postings.iter().map(|(t, d)| (t.clone(), d.len() as u64)).collect();
                let length = f.lengths.values().map(|&l| u64::from(l)).sum();
                (path.clone(), FieldStats { docs: f.lengths.len() as u64, length, df })
            })
            .collect();
        Stats { fields }
    }

    /// Scores with `stats`, those of the whole index this one is a segment of, or with its own
    /// statistics when `None`.
    pub fn set_global(&mut self, stats: Option<Stats>) {
        self.global = stats;
    }

    /// The statistics a field of path `path` scores with, when they are not its own.
    fn global_of(&self, path: &str) -> Option<&FieldStats> {
        self.global.as_ref().map(|g| g.fields.get(path).unwrap_or(&EMPTY_FIELD))
    }

    /// The index analyzer of the fields without their own.
    fn index_analyzer(&self) -> &str {
        self.definition.analyzer.as_deref().unwrap_or(DEFAULT_ANALYZER)
    }

    /// Adds or replaces the document of `key`.
    pub fn put(&mut self, key: &[u8], doc: &Document) -> Result<(), IndexError> {
        self.delete(key);
        let id = u32::try_from(self.keys.len()).unwrap_or(u32::MAX);
        self.keys.push(Some(key.to_vec()));
        self.docs.push(Some(doc.clone()));
        self.by_key.insert(key.to_vec(), id);
        let mut texts: Vec<(String, String, String, String)> = Vec::new();
        let mapping = self.definition.mappings.clone();
        self.collect(&mapping, "", doc, &mut texts);
        for (path, analyzer_name, search_name, text) in texts {
            let tokens =
                analyzer(&analyzer_name)?.analyze(&text).map_err(|_| IndexError::Analysis)?;
            let field = self.fields.entry(path).or_insert_with(|| TextField {
                search_analyzer: search_name.clone(),
                ..TextField::default()
            });
            let base = field.lengths.get(&id).copied().unwrap_or(0);
            for t in &tokens {
                field
                    .postings
                    .entry(t.text.clone())
                    .or_default()
                    .entry(id)
                    .or_default()
                    .push(base + t.position);
            }
            let len = tokens.iter().map(|t| t.position + 1).max().unwrap_or(0);
            *field.lengths.entry(id).or_insert(0) += len;
        }
        Ok(())
    }

    /// The string values of `doc` that `mapping` indexes, as (path, index analyzer, query
    /// analyzer, text).
    fn collect(
        &self,
        mapping: &Mapping,
        prefix: &str,
        doc: &Document,
        out: &mut Vec<(String, String, String, String)>,
    ) {
        for (name, value) in doc.iter() {
            let path = if prefix.is_empty() { name.to_owned() } else { format!("{prefix}.{name}") };
            match mapping.fields.iter().find(|(n, _)| n == name) {
                Some((_, types)) => {
                    for t in types {
                        self.collect_typed(t, &path, value, out);
                    }
                }
                None if mapping.dynamic == Dynamic::On => self.collect_dynamic(&path, value, out),
                None => {}
            }
        }
    }

    fn collect_dynamic(
        &self,
        path: &str,
        value: &Value,
        out: &mut Vec<(String, String, String, String)>,
    ) {
        let a = self.index_analyzer().to_owned();
        let s = self.definition.search_analyzer.clone().unwrap_or_else(|| a.clone());
        match value {
            Value::String(text) => out.push((path.to_owned(), a, s, text.clone())),
            Value::Array(items) => items.iter().for_each(|v| self.collect_dynamic(path, v, out)),
            Value::Document(d) => {
                let dynamic = Mapping {
                    dynamic: Dynamic::On,
                    fields: Vec::new(),
                    stored_source: crate::definition::StoredSource::None,
                };
                self.collect(&dynamic, path, d, out);
            }
            _ => {}
        }
    }

    fn collect_typed(
        &self,
        t: &FieldMapping,
        path: &str,
        value: &Value,
        out: &mut Vec<(String, String, String, String)>,
    ) {
        match (t, value) {
            (FieldMapping::String(f), Value::String(text)) => {
                let a = f.analyzer.clone().unwrap_or_else(|| self.index_analyzer().to_owned());
                let s = f
                    .search_analyzer
                    .clone()
                    .or_else(|| f.analyzer.clone())
                    .or_else(|| self.definition.search_analyzer.clone())
                    .unwrap_or_else(|| a.clone());
                out.push((path.to_owned(), a, s, text.clone()));
            }
            (FieldMapping::String(_) | FieldMapping::Document(_), Value::Array(items)) => {
                items.iter().for_each(|v| self.collect_typed(t, path, v, out));
            }
            (FieldMapping::Document(m), Value::Document(d)) => self.collect(m, path, d, out),
            _ => {}
        }
    }

    /// Removes the document of `key`, if the index has it.
    pub fn delete(&mut self, key: &[u8]) {
        let Some(id) = self.by_key.remove(key) else { return };
        if let Some(k) = self.keys.get_mut(id as usize) {
            *k = None;
        }
        if let Some(d) = self.docs.get_mut(id as usize) {
            *d = None;
        }
        for field in self.fields.values_mut() {
            field.lengths.remove(&id);
            field.postings.retain(|_, docs| {
                docs.remove(&id);
                !docs.is_empty()
            });
        }
    }

    /// Number of documents indexed.
    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    /// Whether the index has no documents.
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    /// The indexed string paths that `pattern` names: a path, or `*` for all of them.
    pub fn paths(&self, pattern: &str) -> Vec<String> {
        if pattern == "*" {
            return self.fields.keys().cloned().collect();
        }
        self.fields.keys().filter(|p| *p == pattern).cloned().collect()
    }

    /// The documents with a term of `query` in one of `paths`, each with the sum of its term
    /// scores (`boost` multiplies each term's weight).
    pub fn text(
        &self,
        query: &[String],
        paths: &[String],
        boost: f32,
    ) -> Result<Vec<Hit>, IndexError> {
        Ok(self.hits(self.text_scores(query, paths, boost)?))
    }

    /// [`MemIndex::text`] by document number.
    pub(crate) fn text_scores(
        &self,
        query: &[String],
        paths: &[String],
        boost: f32,
    ) -> Result<BTreeMap<u32, f32>, IndexError> {
        let mut scores: BTreeMap<u32, f32> = BTreeMap::new();
        for path in paths {
            let Some(field) = self.fields.get(path) else { continue };
            let a = analyzer(&field.search_analyzer)?;
            for q in query {
                for t in a.analyze(q).map_err(|_| IndexError::Analysis)? {
                    for (doc, score) in field.term_scores(&t.text, boost, self.global_of(path)) {
                        let s = scores.entry(doc).or_insert(0.0);
                        *s += score;
                    }
                }
            }
        }
        Ok(scores)
    }

    /// The documents where a phrase of `queries` appears in one of `paths` with at most `slop`
    /// positions between its terms, scored as `refsearch` scores a phrase (measured): the weight
    /// is `boost` times the sum of the terms' idf and the frequency counts each match as
    /// `1 / (1 + length)`, its length being how far the terms are from their places.
    pub(crate) fn phrase_scores(
        &self,
        queries: &[String],
        paths: &[String],
        slop: u32,
        boost: f32,
    ) -> Result<BTreeMap<u32, f32>, IndexError> {
        let mut scores: BTreeMap<u32, f32> = BTreeMap::new();
        for path in paths {
            let Some(field) = self.fields.get(path) else { continue };
            let a = analyzer(&field.search_analyzer)?;
            for q in queries {
                let terms = a.analyze(q).map_err(|_| IndexError::Analysis)?;
                let g = self.global_of(path);
                for (doc, score) in field.phrase_scores(&terms, slop, boost, g) {
                    *scores.entry(doc).or_insert(0.0) += score;
                }
            }
        }
        Ok(scores)
    }

    /// The numbers of the documents indexed.
    pub(crate) fn live(&self) -> impl Iterator<Item = u32> + '_ {
        self.by_key.values().copied()
    }

    /// The document of number `id`.
    pub fn document(&self, id: u32) -> Option<&Document> {
        self.docs.get(id as usize)?.as_ref()
    }

    /// The keys and documents indexed, in key order.
    pub fn entries(&self) -> impl Iterator<Item = (&[u8], &Document)> + '_ {
        self.by_key.iter().filter_map(|(k, &id)| Some((k.as_slice(), self.document(id)?)))
    }

    /// The document of `key`.
    pub fn document_of(&self, key: &[u8]) -> Option<&Document> {
        self.document(*self.by_key.get(key)?)
    }

    /// The scored documents, best first (equal scores by key).
    pub(crate) fn hits(&self, scores: BTreeMap<u32, f32>) -> Vec<Hit> {
        let mut hits: Vec<Hit> = scores
            .into_iter()
            .filter_map(|(doc, score)| {
                let key = self.keys.get(doc as usize)?.clone()?;
                Some(Hit { key, score })
            })
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.key.cmp(&b.key)));
        hits
    }
}

impl TextField {
    /// The number of documents with `term`: `local` in this segment, or the whole index's.
    fn df(&self, term: &str, local: usize, g: Option<&FieldStats>) -> u64 {
        g.map_or(local as u64, |g| g.df.get(term).copied().unwrap_or(local as u64))
    }

    /// The mean length as a 32-bit float and the score of a match frequency for `doc`.
    fn scorer(&self, weight: f32, g: Option<&FieldStats>) -> impl Fn(u32, f32) -> f32 + '_ {
        let (n, total) = match g {
            Some(g) => (g.docs, g.length),
            None => (self.lengths.len() as u64, self.lengths.values().map(|&l| u64::from(l)).sum()),
        };
        let avg = if n == 0 { 1.0f32 } else { (total as f64 / n as f64) as f32 };
        let Bm25 { k1, b } = Bm25::DEFAULT;
        move |doc, freq| {
            let len = self.lengths.get(&doc).copied().unwrap_or(0);
            let dl = norms::decode(norms::encode(len)) as f32;
            let (k1, b) = (k1 as f32, b as f32);
            let inverse = 1.0 / (k1 * ((1.0 - b) + b * dl / avg));
            weight - weight / (1.0 + freq * inverse)
        }
    }

    /// The phrase frequency of each document with all of `terms` and its score.
    fn phrase_scores(
        &self,
        terms: &[crate::analysis::tokenizers::Token],
        slop: u32,
        boost: f32,
        g: Option<&FieldStats>,
    ) -> Vec<(u32, f32)> {
        let Some(first) = terms.first() else { return Vec::new() };
        let n = g.map_or(self.lengths.len() as u64, |g| g.docs);
        let mut lists = Vec::with_capacity(terms.len());
        let mut idf_sum = 0.0f32;
        for t in terms {
            let Some(docs) = self.postings.get(&t.text) else { return Vec::new() };
            idf_sum += idf(n, self.df(&t.text, docs.len(), g)) as f32;
            lists.push((docs, i64::from(t.position) - i64::from(first.position)));
        }
        let score = self.scorer(boost * idf_sum, g);
        let (shortest, _) =
            lists.iter().min_by_key(|(d, _)| d.len()).copied().unwrap_or((lists[0].0, 0));
        shortest
            .keys()
            .filter_map(|doc| {
                let places: Option<Vec<Vec<i64>>> = lists
                    .iter()
                    .map(|(d, offset)| {
                        d.get(doc).map(|ps| ps.iter().map(|&p| i64::from(p) - offset).collect())
                    })
                    .collect();
                let freq = phrase_freq(&places?, i64::from(slop));
                (freq > 0.0).then(|| (*doc, score(*doc, freq)))
            })
            .collect()
    }

    /// The score of each document with `term`.
    fn term_scores(&self, term: &str, boost: f32, g: Option<&FieldStats>) -> Vec<(u32, f32)> {
        let Some(docs) = self.postings.get(term) else { return Vec::new() };
        let n = g.map_or(self.lengths.len() as u64, |g| g.docs);
        let weight = boost * idf(n, self.df(term, docs.len(), g)) as f32;
        let score = self.scorer(weight, g);
        docs.iter().map(|(&doc, positions)| (doc, score(doc, positions.len() as f32))).collect()
    }
}

/// The matches of a phrase whose terms sit at `places` (each term's positions minus its offset in
/// the phrase), each worth `1 / (1 + span)`: the term at the lowest place moves on while it stays
/// at or below the next lowest, which keeps the shortest span of the group, and the group counts
/// once if that span is at most `slop`.
fn phrase_freq(places: &[Vec<i64>], slop: i64) -> f32 {
    let mut at = vec![0usize; places.len()];
    let mut freq = 0.0f32;
    loop {
        let current: Option<Vec<i64>> =
            places.iter().zip(&at).map(|(p, &i)| p.get(i).copied()).collect();
        let Some(current) = current else { return freq };
        let Some((low, &lo)) = current.iter().enumerate().min_by_key(|&(_, v)| *v) else {
            return freq;
        };
        let hi = current.iter().copied().max().unwrap_or(lo);
        let second = current
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != low)
            .map(|(_, &v)| v)
            .min()
            .unwrap_or(lo);
        let mut span = hi - lo;
        while let Some(&next) = places[low].get(at[low] + 1).filter(|&&v| v <= second) {
            at[low] += 1;
            span = span.min(hi - next);
        }
        if span <= slop {
            freq += 1.0 / (1.0 + span as f32);
        }
        at[low] += 1;
    }
}

#[cfg(test)]
mod tests;
