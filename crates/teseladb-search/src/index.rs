//! The documents of a search index in memory (011 FR-009, FR-016): each string field analyzed
//! by its mapping, its postings and its lengths, and the `text` operator scored as `refsearch`
//! 1.70.6 scores it. Measured on a 9.0.4 replica set (01-11-2026): a term scores
//! `w − w / (1 + freq · n)` in 32-bit floats, with `w = boost · idf` and
//! `n = 1 / (k1 · (1 − b + b · dl / avgdl))`, `dl` the field's length through its norm byte and
//! `avgdl` the exact mean length as a 32-bit float; the terms and paths of a query add up in their
//! order.

use std::borrow::Cow;
use std::collections::BTreeMap;

use teseladb_bson::{Document, Value};

use crate::analysis::analyzer::Analyzer;
use crate::analysis::jregex::RegexError;
use crate::definition::{DEFAULT_ANALYZER, Definition, Dynamic, FieldMapping, Mapping};
use crate::query::bm25::{Bm25, idf};
use crate::query::fuzzy::Levenshtein;
use crate::segment::norms;

/// A string field of the index: the postings of each term (document number to positions), the
/// length of the field in each document and its number of tokens. As RefLib's BM25 does with
/// overlaps discounted (measured on `refsearch` with stacked edge grams), the length that scores a
/// document counts the tokens that start a position, while the mean length comes from every
/// token.
#[derive(Clone, Debug, Default)]
struct TextField {
    postings: BTreeMap<String, BTreeMap<u32, Vec<u32>>>,
    lengths: BTreeMap<u32, u32>,
    /// Tokens of each document, overlaps included.
    tokens: BTreeMap<u32, u32>,
    /// Position after the last token of each document, where its next value starts.
    ends: BTreeMap<u32, u32>,
    /// Query analyzer.
    search_analyzer: String,
}

/// Why a document or a query cannot be analyzed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexError {
    /// An analyzer the definition names and this engine cannot build.
    Analyzer(String),
    /// An analyzer failed on a text, with the message `refsearch` gives: a component that cannot be
    /// built, a replacement that names a group the pattern lacks, a match out of steps.
    Analysis(String),
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
    /// The synonym mappings loaded from their collections, by name.
    synonyms: BTreeMap<String, synonyms::SynonymMap>,
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

/// One string field of [`MemIndex::contents`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldContents {
    /// Each term's positions by document key.
    pub terms: BTreeMap<String, BTreeMap<Vec<u8>, Vec<u32>>>,
    /// Each document's scoring length and number of tokens, by key.
    pub lengths: BTreeMap<Vec<u8>, (u32, u32)>,
}

/// [`MemIndex::contents`]: the fields by path.
pub type Contents = BTreeMap<String, FieldContents>;

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

/// The error of an analyzer that fails on a text.
fn analysis(e: RegexError) -> IndexError {
    IndexError::Analysis(match e {
        RegexError::Replacement(m) | RegexError::Engine(m) => m,
        RegexError::Syntax(_) | RegexError::Limit => "analysis ran out of steps".to_owned(),
    })
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
            synonyms: BTreeMap::new(),
        }
    }

    /// The statistics of this index's own documents.
    pub fn stats(&self) -> Stats {
        let fields = self
            .fields
            .iter()
            .map(|(path, f)| {
                let df = f.postings.iter().map(|(t, d)| (t.clone(), d.len() as u64)).collect();
                let length = f.tokens.values().map(|&l| u64::from(l)).sum();
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

    /// The analyzer called `name`: one the definition defines, or a built-in one.
    fn analyzer(&self, name: &str) -> Result<Cow<'_, Analyzer>, IndexError> {
        if let Some(custom) = self.definition.custom_analyzer(name) {
            return match &custom.built {
                Ok(a) => Ok(Cow::Borrowed(a)),
                Err(message) => Err(IndexError::Analysis(message.clone())),
            };
        }
        Analyzer::builtin(name).map(Cow::Owned).ok_or_else(|| IndexError::Analyzer(name.to_owned()))
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
        let mut texts: Vec<Collected> = Vec::new();
        let mapping = self.definition.mappings.clone();
        self.collect(&mapping, "", doc, &mut texts);
        for Collected { path, analyzer: analyzer_name, search: search_name, text, grams } in texts {
            if let Some(g) = grams {
                self.put_grams(id, &path, &analyzer_name, &g, &text)?;
                continue;
            }
            let tokens = self.analyzer(&analyzer_name)?.analyze(&text).map_err(analysis)?;
            // A value without tokens (an empty string) leaves no length: the document does not
            // count among those of the field (measured: `t: ""` changes no score).
            if tokens.is_empty() {
                continue;
            }
            let field = self.fields.entry(path).or_insert_with(|| TextField {
                search_analyzer: search_name.clone(),
                ..TextField::default()
            });
            let base = field.ends.get(&id).copied().unwrap_or(0);
            for t in &tokens {
                field
                    .postings
                    .entry(t.text.clone())
                    .or_default()
                    .entry(id)
                    .or_default()
                    .push(base + t.position);
            }
            let end = tokens.iter().map(|t| t.position + 1).max().unwrap_or(0);
            let mut starts: Vec<u32> = tokens.iter().map(|t| t.position).collect();
            starts.sort_unstable();
            starts.dedup();
            *field.ends.entry(id).or_insert(0) += end;
            *field.lengths.entry(id).or_insert(0) += starts.len() as u32;
            *field.tokens.entry(id).or_insert(0) += tokens.len() as u32;
        }
        Ok(())
    }

    /// The string values of `doc` that `mapping` indexes, as (path, index analyzer, query
    /// analyzer, text).
    fn collect(&self, mapping: &Mapping, prefix: &str, doc: &Document, out: &mut Vec<Collected>) {
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

    fn collect_dynamic(&self, path: &str, value: &Value, out: &mut Vec<Collected>) {
        let a = self.index_analyzer().to_owned();
        let s = self.definition.search_analyzer.clone().unwrap_or_else(|| a.clone());
        match value {
            Value::String(text) => out.push(Collected::text(path, a, s, text)),
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

    fn collect_typed(&self, t: &FieldMapping, path: &str, value: &Value, out: &mut Vec<Collected>) {
        match (t, value) {
            (FieldMapping::String(f), Value::String(text)) => {
                let a = f.analyzer.clone().unwrap_or_else(|| self.index_analyzer().to_owned());
                let s = f
                    .search_analyzer
                    .clone()
                    .or_else(|| f.analyzer.clone())
                    .or_else(|| self.definition.search_analyzer.clone())
                    .unwrap_or_else(|| a.clone());
                out.push(Collected::text(path, a, s, text));
            }
            (
                FieldMapping::Autocomplete {
                    min_grams,
                    max_grams,
                    fold_diacritics,
                    tokenization,
                    analyzer,
                },
                Value::String(text),
            ) => {
                let a = analyzer.clone().unwrap_or_else(|| self.index_analyzer().to_owned());
                let grams = Grams::new(*min_grams, *max_grams, *fold_diacritics, tokenization);
                out.push(Collected {
                    path: autocomplete_key(path),
                    analyzer: a.clone(),
                    search: a,
                    text: text.clone(),
                    grams: Some(grams),
                });
            }
            (FieldMapping::Autocomplete { .. }, Value::Array(items)) => {
                items.iter().for_each(|v| self.collect_typed(t, path, v, out));
            }
            (FieldMapping::String(_) | FieldMapping::Document(_), Value::Array(items)) => {
                items.iter().for_each(|v| self.collect_typed(t, path, v, out));
            }
            (FieldMapping::Document(m), Value::Document(d)) => self.collect(m, path, d, out),
            _ => {}
        }
    }

    /// Adds the grams of `text` to the `autocomplete` field `key` for document `id`: those of
    /// each word and of each run of two or three words (measured on `refsearch`: `gato montes`
    /// holds `gat` twice, `el gatopardo salvaje` holds `el` three times); its length is its
    /// number of words.
    fn put_grams(
        &mut self,
        id: u32,
        key: &str,
        analyzer: &str,
        g: &Grams,
        text: &str,
    ) -> Result<(), IndexError> {
        let words: Vec<String> = self
            .analyzer(analyzer)?
            .analyze(text)
            .map_err(analysis)?
            .into_iter()
            .map(|t| g.normalized(&t.text))
            .collect();
        if words.is_empty() {
            return Ok(());
        }
        let terms: Vec<String> = Grams::shingles(&words).iter().flat_map(|w| g.of(w)).collect();
        let field = self.fields.entry(key.to_owned()).or_insert_with(|| TextField {
            search_analyzer: analyzer.to_owned(),
            ..TextField::default()
        });
        let base = field.ends.get(&id).copied().unwrap_or(0);
        for (i, t) in terms.iter().enumerate() {
            let at = base + i as u32;
            field.postings.entry(t.clone()).or_default().entry(id).or_default().push(at);
        }
        *field.ends.entry(id).or_insert(0) += terms.len() as u32;
        *field.lengths.entry(id).or_insert(0) += words.len() as u32;
        *field.tokens.entry(id).or_insert(0) += terms.len() as u32;
        Ok(())
    }

    /// The scores of `terms` on field `key` (a sum in doubles, as RefLib adds the clauses of a
    /// disjunction, each a term query), with weight `boost`.
    pub(crate) fn terms_scores(
        &self,
        key: &str,
        terms: &[String],
        boost: f32,
    ) -> BTreeMap<u32, f64> {
        let mut out: BTreeMap<u32, f64> = BTreeMap::new();
        let Some(field) = self.fields.get(key) else { return out };
        let g = self.global_of(key);
        for t in terms {
            for (doc, s) in field.term_scores(t, boost, g) {
                *out.entry(doc).or_insert(0.0) += f64::from(s);
            }
        }
        out
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
            field.tokens.remove(&id);
            field.ends.remove(&id);
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
            // The grams of `autocomplete` fields are not a string path.
            return self.fields.keys().filter(|k| !k.starts_with("$type:")).cloned().collect();
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
            let a = self.analyzer(&field.search_analyzer)?;
            for q in query {
                let tokens = a.analyze(q).map_err(analysis)?;
                for segment in segments(&tokens)? {
                    let g = self.global_of(path);
                    for (doc, score) in field.segment_scores(&segment, boost, g) {
                        let s = scores.entry(doc).or_insert(0.0);
                        *s += score;
                    }
                }
            }
        }
        Ok(scores)
    }

    /// The query analyzer of the field at `path`: its own, or the definition's when no document
    /// has it.
    pub(crate) fn query_analyzer(&self, path: &str) -> &str {
        match self.fields.get(path) {
            Some(f) => &f.search_analyzer,
            None => {
                self.definition.search_analyzer.as_deref().unwrap_or_else(|| self.index_analyzer())
            }
        }
    }

    /// The documents with a term of the field at `path` that `matches`.
    pub(crate) fn matching_terms(&self, path: &str, matches: impl Fn(&str) -> bool) -> Vec<u32> {
        let Some(field) = self.fields.get(path) else { return Vec::new() };
        let mut docs: Vec<u32> = field
            .postings
            .iter()
            .filter(|(term, _)| matches(term))
            .flat_map(|(_, d)| d.keys().copied())
            .collect();
        docs.sort_unstable();
        docs.dedup();
        docs
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
            let a = self.analyzer(&field.search_analyzer)?;
            for q in queries {
                let terms = a.analyze(q).map_err(analysis)?;
                let g = self.global_of(path);
                for (doc, score) in field.phrase_scores(&terms, slop, boost, g) {
                    *scores.entry(doc).or_insert(0.0) += score;
                }
            }
        }
        Ok(scores)
    }

    /// The documents with a term of one of `paths` within `fuzzy.max_edits` edits of a token of
    /// `queries`, scored as RefLib's `FuzzyQuery` with its top-terms blended rewrite (measured
    /// on `refsearch`): the `max_expansions` closest terms, each weighted `1 - edits / shortest
    /// length` with the largest document count among them, their scores added.
    pub(crate) fn fuzzy_scores(
        &self,
        queries: &[String],
        paths: &[String],
        fuzzy: &Fuzzy,
        boost: f32,
    ) -> Result<BTreeMap<u32, f32>, IndexError> {
        let mut scores: BTreeMap<u32, f32> = BTreeMap::new();
        for path in paths {
            let Some(field) = self.fields.get(path) else { continue };
            let a = self.analyzer(&field.search_analyzer)?;
            for q in queries {
                for t in a.analyze(q).map_err(analysis)? {
                    let g = self.global_of(path);
                    for (doc, score) in field.fuzzy_scores(&t.text, fuzzy, boost, g) {
                        *scores.entry(doc).or_insert(0.0) += score;
                    }
                }
            }
        }
        Ok(scores)
    }

    /// The tokens, with their offsets, analyzer `name` makes of `text`.
    pub(crate) fn analyze_tokens(
        &self,
        name: &str,
        text: &str,
    ) -> Result<Vec<crate::analysis::tokenizers::Token>, IndexError> {
        self.analyzer(name)?.analyze(text).map_err(analysis)
    }

    /// The tokens analyzer `name` makes of `text`.
    pub(crate) fn analyze(&self, name: &str, text: &str) -> Result<Vec<String>, IndexError> {
        let tokens = self.analyzer(name)?.analyze(text).map_err(analysis)?;
        Ok(tokens.into_iter().map(|t| t.text).collect())
    }

    /// An estimate of the bytes the index takes: its documents (as BSON), and per field each
    /// term and its positions.
    pub(crate) fn memory_bytes(&self) -> u64 {
        let docs: usize =
            self.docs.iter().flatten().map(|d| d.to_bytes().map_or(0, |b| b.len())).sum();
        let postings: usize = self
            .fields
            .iter()
            .map(|(name, f)| {
                name.len()
                    + f.postings
                        .iter()
                        .map(|(t, by_doc)| {
                            t.len() + by_doc.values().map(|p| 8 + 4 * p.len()).sum::<usize>()
                        })
                        .sum::<usize>()
                    + 12 * f.lengths.len()
            })
            .sum();
        (docs + postings) as u64
    }

    /// The number of the document of `key`.
    pub(crate) fn id_of(&self, key: &[u8]) -> Option<u32> {
        self.by_key.get(key).copied()
    }

    /// The key of document number `id`.
    pub(crate) fn key_of(&self, id: u32) -> Option<&[u8]> {
        self.keys.get(id as usize)?.as_deref()
    }

    /// Whether some document has analyzed text at `path`.
    pub(crate) fn has_text(&self, path: &str) -> bool {
        self.fields.contains_key(path)
    }

    /// The positions of `term` in the field at `path`, by document.
    pub(crate) fn positions(&self, path: &str, term: &str) -> Option<&BTreeMap<u32, Vec<u32>>> {
        self.fields.get(path)?.postings.get(term)
    }

    /// The query analyzer's tokens of `text` for the field at `path`.
    pub(crate) fn query_terms(&self, path: &str, text: &str) -> Result<Vec<String>, IndexError> {
        let a = self.analyzer(self.query_analyzer(path))?;
        Ok(a.analyze(text).map_err(analysis)?.into_iter().map(|t| t.text).collect())
    }

    /// Scores of a span query of the field at `path` (RefLib's `SpanWeight`): the weight is
    /// `boost` times the sum of the idf of `terms`, and `freqs` the documents' span frequencies.
    pub(crate) fn span_scores(
        &self,
        path: &str,
        terms: &[String],
        freqs: BTreeMap<u32, f32>,
        boost: f32,
    ) -> BTreeMap<u32, f32> {
        let Some(field) = self.fields.get(path) else { return BTreeMap::new() };
        let g = self.global_of(path);
        let n = g.map_or(field.lengths.len() as u64, |g| g.docs);
        let idf_sum = idf_sum(
            n,
            terms.iter().filter_map(|t| field.postings.get(t).map(|d| field.df(t, d.len(), g))),
        );
        let score = field.scorer(boost * idf_sum, g);
        freqs.into_iter().map(|(doc, f)| (doc, score(doc, f))).collect()
    }

    /// The numbers of the documents indexed.
    pub(crate) fn live(&self) -> impl Iterator<Item = u32> + '_ {
        self.by_key.values().copied()
    }

    /// The definition the index follows.
    pub(crate) fn definition(&self) -> &Definition {
        &self.definition
    }

    /// The document of number `id`.
    pub fn document(&self, id: u32) -> Option<&Document> {
        self.docs.get(id as usize)?.as_ref()
    }

    /// The keys and documents indexed, in key order.
    /// What the index holds with its document numbers replaced by keys, so two indexes built
    /// from the same documents in another order compare equal (FR-011): for each path, each
    /// term's positions by key, and each key's length and number of tokens.
    pub fn contents(&self) -> Contents {
        let key = |id: &u32| self.keys.get(*id as usize).cloned().flatten().unwrap_or_default();
        self.fields
            .iter()
            .map(|(path, f)| {
                let terms = f
                    .postings
                    .iter()
                    .map(|(t, docs)| {
                        (t.clone(), docs.iter().map(|(d, ps)| (key(d), ps.clone())).collect())
                    })
                    .collect();
                let lengths = f
                    .lengths
                    .iter()
                    .map(|(d, &l)| (key(d), (l, f.tokens.get(d).copied().unwrap_or(0))))
                    .collect();
                (path.clone(), FieldContents { terms, lengths })
            })
            .collect()
    }

    /// The documents and their keys.
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
            None => (self.lengths.len() as u64, self.tokens.values().map(|&l| u64::from(l)).sum()),
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
        let Some((dfs, freqs)) = self.phrase_freqs(terms, slop, g) else { return Vec::new() };
        let n = g.map_or(self.lengths.len() as u64, |g| g.docs);
        let idf_sum = idf_sum(n, dfs.iter().copied());
        let score = self.scorer(boost * idf_sum, g);
        freqs.into_iter().map(|(doc, f)| (doc, score(doc, f))).collect()
    }

    /// The document counts of `terms` and the phrase frequency of each document with all of
    /// them, `None` when a term is missing.
    fn phrase_freqs(
        &self,
        terms: &[crate::analysis::tokenizers::Token],
        slop: u32,
        g: Option<&FieldStats>,
    ) -> Option<(Vec<u64>, Vec<(u32, f32)>)> {
        let first = terms.first()?;
        let mut lists = Vec::with_capacity(terms.len());
        let mut dfs = Vec::with_capacity(terms.len());
        for t in terms {
            let docs = self.postings.get(&t.text)?;
            dfs.push(self.df(&t.text, docs.len(), g));
            lists.push((docs, i64::from(t.position) - i64::from(first.position)));
        }
        let (shortest, _) =
            lists.iter().min_by_key(|(d, _)| d.len()).copied().unwrap_or((lists[0].0, 0));
        let freqs = shortest
            .keys()
            .filter_map(|doc| {
                let places: Option<Vec<Vec<i64>>> = lists
                    .iter()
                    .map(|(d, offset)| {
                        d.get(doc).map(|ps| ps.iter().map(|&p| i64::from(p) - offset).collect())
                    })
                    .collect();
                let freq = phrase_freq(&places?, i64::from(slop));
                (freq > 0.0).then_some((*doc, freq))
            })
            .collect();
        Some((dfs, freqs))
    }

    /// The score of each document that `segment` matches.
    fn segment_scores(
        &self,
        segment: &Segment<'_>,
        boost: f32,
        g: Option<&FieldStats>,
    ) -> Vec<(u32, f32)> {
        match segment {
            Segment::Position(terms) => self.synonym_scores(terms, boost, g),
            Segment::Paths(paths) => {
                let mut sums: BTreeMap<u32, f64> = BTreeMap::new();
                for path in paths {
                    for (doc, score) in self.path_scores(path, boost, g) {
                        *sums.entry(doc).or_insert(0.0) += f64::from(score);
                    }
                }
                sums.into_iter().map(|(doc, s)| (doc, s as f32)).collect()
            }
        }
    }

    /// The documents with every term of `path`, each with the sum of the terms' scores (a
    /// RefLib conjunction, summed in double precision).
    fn path_scores(&self, path: &[&str], boost: f32, g: Option<&FieldStats>) -> Vec<(u32, f32)> {
        let mut sums: Option<BTreeMap<u32, f64>> = None;
        for term in path {
            let scores: BTreeMap<u32, f32> = self.term_scores(term, boost, g).into_iter().collect();
            sums = Some(match sums {
                None => scores.into_iter().map(|(d, s)| (d, f64::from(s))).collect(),
                Some(before) => before
                    .into_iter()
                    .filter_map(|(d, s)| scores.get(&d).map(|&t| (d, s + f64::from(t))))
                    .collect(),
            });
        }
        sums.unwrap_or_default().into_iter().map(|(d, s)| (d, s as f32)).collect()
    }

    /// The score of each document with one of `terms`, which sit at one position: one term
    /// scores alone; several score as RefLib's `SynonymQuery`, with the sum of their
    /// frequencies and the largest of their document counts.
    fn synonym_scores(
        &self,
        terms: &[&str],
        boost: f32,
        g: Option<&FieldStats>,
    ) -> Vec<(u32, f32)> {
        if let [term] = terms {
            return self.term_scores(term, boost, g);
        }
        let mut freqs: BTreeMap<u32, f32> = BTreeMap::new();
        let mut df = 0;
        for term in terms {
            let Some(docs) = self.postings.get(*term) else { continue };
            df = df.max(self.df(term, docs.len(), g));
            for (&doc, positions) in docs {
                *freqs.entry(doc).or_insert(0.0) += positions.len() as f32;
            }
        }
        if freqs.is_empty() {
            return Vec::new();
        }
        let n = g.map_or(self.lengths.len() as u64, |g| g.docs);
        let score = self.scorer(boost * idf(n, df) as f32, g);
        freqs.into_iter().map(|(doc, freq)| (doc, score(doc, freq))).collect()
    }

    /// The terms within `fuzzy` of `term`, closest first, each with its weight
    /// `1 - edits / shortest length` (RefLib's top-terms blended rewrite).
    fn fuzzy_terms(&self, term: &str, fuzzy: &Fuzzy) -> Vec<(String, f32)> {
        let lev = Levenshtein::new(term, fuzzy.max_edits, fuzzy.prefix_length);
        let len = term.chars().count();
        let mut found: Vec<(&String, f32)> = self
            .postings
            .keys()
            .filter_map(|t| {
                let edits = lev.distance(t)?;
                let shortest = len.min(t.chars().count()).max(1);
                Some((t, 1.0 - f32::from(edits) / shortest as f32))
            })
            .collect();
        found.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        let mut kept: Vec<(String, f32)> =
            found.into_iter().take(fuzzy.max_expansions).map(|(t, w)| (t.clone(), w)).collect();
        // The rewritten query lists its terms in term order (measured with `scoreDetails`).
        kept.sort_by(|a, b| a.0.cmp(&b.0));
        kept
    }

    /// The largest document count among `kept`, which every kept term's idf uses.
    fn blended_df(&self, kept: &[(String, f32)], g: Option<&FieldStats>) -> u64 {
        kept.iter()
            .map(|(t, _)| self.df(t, self.postings.get(t).map_or(0, BTreeMap::len), g))
            .max()
            .unwrap_or(0)
    }

    /// [`MemIndex::fuzzy_scores`] for one query term.
    fn fuzzy_scores(
        &self,
        term: &str,
        fuzzy: &Fuzzy,
        boost: f32,
        g: Option<&FieldStats>,
    ) -> Vec<(u32, f32)> {
        let kept = self.fuzzy_terms(term, fuzzy);
        let n = g.map_or(self.lengths.len() as u64, |g| g.docs);
        let idf = idf(n, self.blended_df(&kept, g)) as f32;
        let mut sums: BTreeMap<u32, f32> = BTreeMap::new();
        for (t, weight) in &kept {
            let score = self.scorer(boost * weight * idf, g);
            for (&doc, positions) in &self.postings[t] {
                *sums.entry(doc).or_insert(0.0) += score(doc, positions.len() as f32);
            }
        }
        sums.into_iter().collect()
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

/// A string value of a document the index analyzes: its path (the key of its field), index and
/// query analyzers, text, and how an `autocomplete` field makes grams of it.
struct Collected {
    path: String,
    analyzer: String,
    search: String,
    text: String,
    grams: Option<Grams>,
}

impl Collected {
    fn text(path: &str, analyzer: String, search: String, text: &str) -> Collected {
        Collected { path: path.to_owned(), analyzer, search, text: text.to_owned(), grams: None }
    }
}

/// How an `autocomplete` field makes grams.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Grams {
    /// Shortest gram.
    pub min: usize,
    /// Longest gram.
    pub max: usize,
    /// Whether diacritics are folded.
    pub fold: bool,
    /// `edgeGram` or `rightEdgeGram` (grams from one end); otherwise every gram.
    pub edge: bool,
    /// `rightEdgeGram`: the grams are taken from the reversed text.
    pub reverse: bool,
}

/// The most words `refsearch` joins into one string of an `autocomplete` field (measured
/// 02-11-2026, `scratchpad/lane3k/m13.js`: `a1 b2 c3 d4 …` holds `a1 b2 c3` but no longer run).
const SHINGLE_WORDS: usize = 3;

impl Grams {
    /// The grams of an `autocomplete` field of `tokenization` (`edgeGram`, `rightEdgeGram` or
    /// `nGram`).
    pub(crate) fn new(min: i64, max: i64, fold: bool, tokenization: &str) -> Grams {
        Grams {
            min: usize::try_from(min).unwrap_or(0),
            max: usize::try_from(max).unwrap_or(0),
            fold,
            edge: tokenization != "nGram",
            reverse: tokenization == "rightEdgeGram",
        }
    }

    /// `text` lower-cased, and folded when the field folds.
    pub(crate) fn normalized(&self, text: &str) -> String {
        let lower: String = text.chars().flat_map(char::to_lowercase).collect();
        if self.fold { crate::analysis::tokenfilters::fold_ascii(&lower) } else { lower }
    }

    /// The characters grams are taken from: `word`'s, reversed for `rightEdgeGram`.
    fn chars(&self, word: &str) -> Vec<char> {
        if self.reverse { word.chars().rev().collect() } else { word.chars().collect() }
    }

    /// The grams of `word`: its prefixes (or every slice) of `min` to `max` characters, and its
    /// first `max` characters once more when it is longer (measured on `refsearch`, with
    /// `rightEdgeGram` on the reversed word).
    pub(crate) fn of(&self, word: &str) -> Vec<String> {
        let chars = self.chars(word);
        let mut out = Vec::new();
        let starts = if self.edge { 0..1 } else { 0..chars.len() };
        for start in starts {
            for n in self.min..=self.max {
                if start + n <= chars.len() {
                    out.push(chars[start..start + n].iter().collect());
                }
            }
        }
        if chars.len() > self.max {
            out.push(chars[..self.max].iter().collect());
        }
        out
    }

    /// The strings of a field value whose grams are indexed: its words and the runs of up to
    /// three of them, joined by a space, in the order of their first word.
    pub(crate) fn shingles(words: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        for start in 0..words.len() {
            for end in start + 1..=words.len().min(start + SHINGLE_WORDS) {
                out.push(words[start..end].join(" "));
            }
        }
        out
    }

    /// The query term of `word`: its first `max` characters (its last ones, reversed, for
    /// `rightEdgeGram`), `None` under `min`.
    pub(crate) fn term(&self, word: &str) -> Option<String> {
        let chars = self.chars(word);
        (chars.len() >= self.min).then(|| chars[..chars.len().min(self.max)].iter().collect())
    }
}

/// The key of the grams of the `autocomplete` field at `path`, apart from its text.
pub(crate) fn autocomplete_key(path: &str) -> String {
    format!("$type:autocomplete/{path}")
}

/// The `fuzzy` option of `text` and `term`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fuzzy {
    /// Most edits from the query term (1 or 2).
    pub max_edits: u8,
    /// Characters at the start that must match exactly.
    pub prefix_length: usize,
    /// Most terms a query term expands to.
    pub max_expansions: usize,
}

/// The most paths a query graph may expand to, RefLib's `maxClauseCount`.
const MAX_CLAUSES: usize = 1024;

/// A part of a query's token graph between two positions every path goes through.
enum Segment<'a> {
    /// One position: its terms, scored as one term or a synonym query.
    Position(Vec<&'a str>),
    /// Tokens spanning several positions: every path through them, each a conjunction.
    Paths(Vec<Vec<&'a str>>),
}

/// The query of `tokens` as RefLib's `QueryBuilder` builds it for a token graph (measured on
/// `refsearch` with shingles and `wordDelimiterGraph`): the graph is cut at the positions no token
/// spans; a part of one position gives its terms, and a part with longer tokens gives each of its
/// paths.
fn segments(tokens: &[crate::analysis::tokenizers::Token]) -> Result<Vec<Segment<'_>>, IndexError> {
    // A graph filter gives the tokens of one word out of position order.
    let mut sorted: Vec<&crate::analysis::tokenizers::Token> = tokens.iter().collect();
    sorted.sort_by_key(|t| t.position);
    let tokens = sorted;
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let start = tokens[i].position;
        let mut end = start + tokens[i].length.max(1);
        let mut j = i;
        while j < tokens.len() && tokens[j].position < end {
            end = end.max(tokens[j].position + tokens[j].length.max(1));
            j += 1;
        }
        let part = &tokens[i..j];
        if part.iter().all(|t| t.length <= 1) && part.iter().all(|t| t.position == start) {
            let mut texts: Vec<&str> = Vec::new();
            for t in part {
                if !texts.contains(&t.text.as_str()) {
                    texts.push(&t.text);
                }
            }
            out.push(Segment::Position(texts));
        } else {
            let mut paths = Vec::new();
            walk(part, start, end, &mut Vec::new(), &mut paths)?;
            out.push(Segment::Paths(paths));
        }
        i = j;
    }
    Ok(out)
}

/// Every path of `part` from position `at` to `end`, after `prefix`.
fn walk<'a>(
    part: &[&'a crate::analysis::tokenizers::Token],
    at: u32,
    end: u32,
    prefix: &mut Vec<&'a str>,
    paths: &mut Vec<Vec<&'a str>>,
) -> Result<(), IndexError> {
    if at >= end {
        if paths.len() == MAX_CLAUSES {
            return Err(IndexError::Analysis(format!("maxClauseCount is set to {MAX_CLAUSES}")));
        }
        paths.push(prefix.clone());
        return Ok(());
    }
    for t in part.iter().filter(|t| t.position == at) {
        prefix.push(&t.text);
        walk(part, at + t.length.max(1), end, prefix, paths)?;
        prefix.pop();
    }
    Ok(())
}

/// The weight of several terms: their idfs, each a 32-bit float, added in double precision as
/// RefLib's `BM25Similarity.idfExplain` adds them.
pub(crate) fn idf_sum(n: u64, dfs: impl IntoIterator<Item = u64>) -> f32 {
    dfs.into_iter().map(|df| f64::from(idf(n, df) as f32)).sum::<f64>() as f32
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

mod dump;
mod explain;
mod phrases;
mod synonyms;
pub use dump::{DumpError, applied_of};
pub use explain::Explanation;
pub(crate) use explain::{FreqKind, Weight, java_double, java_float};
pub(crate) use phrases::{MultiPhrase, PhraseAt};
pub(crate) use synonyms::Segment as SynonymSegment;
pub use synonyms::{EQUIVALENT, EXPLICIT};

#[cfg(test)]
mod tests;
