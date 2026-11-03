//! The text index and the `$text` operator (011 FR-001, FR-016, T029), as a RefDb 9.0.4 text
//! index of `textIndexVersion` 3 reads documents and queries (measured 02-11-2026 on the 9.0.4
//! replica set of `scratchpad/lane3m`, `m33.js` to `m38.js`):
//!
//! - a string splits into tokens at the delimiters of `data/text-delimiters.txt` (in `english`
//!   the apostrophe is not one); each token loses its case and its diacritics (the code points of
//!   `data/text-removed.txt` go), the stop words of its language go, and the rest is stemmed with
//!   the language's Snowball stemmer (`none`: neither);
//! - a document's language is the `language_override` field (`language`) of the document or of
//!   the embedded document that holds the string, else the index's `default_language`;
//! - the score of a string for one of its stems, with `n` stems in the string and the stem's
//!   occurrences numbered from 0, is `weight · Σ 2^-i · (0.5 · count / n + 0.5)`, times 1.1 when the
//!   whole string is the stem but for case; each element of an array is a string of its own, and
//!   a document's score is the sum over the query's distinct stems;
//! - a query is its terms, its negated terms (`-` after a space or at the start), its phrases
//!   (between quotes, their words being terms too) and its negated phrases; a document matches
//!   when it holds a term, every phrase and none of the negated ones, the phrases compared as
//!   substrings without case and diacritics unless `$caseSensitive` or `$diacriticSensitive`.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use teseladb_bson::{Document, Value};

use crate::analysis::snowball::Stemmer;
use crate::analysis::unicode;

/// The factor of a string that is the stem itself (measured: `"Cat"` for `cat` scores 1.1).
const WHOLE_STRING: f64 = 1.1;
/// The field naming a document's language unless the index says another.
pub const LANGUAGE_OVERRIDE: &str = "language";

/// The languages of a version 3 text index: name and stemmer (`none` has neither stemmer nor
/// stop words).
const LANGUAGES: &[(&str, Option<Stemmer>, &str)] = &[
    ("danish", Some(Stemmer::Danish), include_str!("../data/text-stopwords/danish.txt")),
    ("dutch", Some(Stemmer::Dutch), include_str!("../data/text-stopwords/dutch.txt")),
    ("english", Some(Stemmer::English), include_str!("../data/text-stopwords/english.txt")),
    ("finnish", Some(Stemmer::Finnish), include_str!("../data/text-stopwords/finnish.txt")),
    ("french", Some(Stemmer::French), include_str!("../data/text-stopwords/french.txt")),
    ("german", Some(Stemmer::German), include_str!("../data/text-stopwords/german.txt")),
    ("hungarian", Some(Stemmer::Hungarian), include_str!("../data/text-stopwords/hungarian.txt")),
    ("italian", Some(Stemmer::Italian), include_str!("../data/text-stopwords/italian.txt")),
    ("norwegian", Some(Stemmer::Norwegian), include_str!("../data/text-stopwords/norwegian.txt")),
    (
        "portuguese",
        Some(Stemmer::Portuguese),
        include_str!("../data/text-stopwords/portuguese.txt"),
    ),
    ("romanian", Some(Stemmer::Romanian), include_str!("../data/text-stopwords/romanian.txt")),
    ("russian", Some(Stemmer::Russian), include_str!("../data/text-stopwords/russian.txt")),
    ("spanish", Some(Stemmer::Spanish), include_str!("../data/text-stopwords/spanish.txt")),
    ("swedish", Some(Stemmer::Swedish), include_str!("../data/text-stopwords/swedish.txt")),
    ("turkish", Some(Stemmer::Turkish), include_str!("../data/text-stopwords/turkish.txt")),
    ("none", None, ""),
];

/// An error of a text index or of a `$text` query, with RefDb's code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextError {
    /// The error code.
    pub code: i32,
    /// The message.
    pub message: String,
}

fn bad_value(message: impl Into<String>) -> TextError {
    TextError { code: 2, message: message.into() }
}

/// A language of the text index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Language(usize);

impl Language {
    /// The language called `name`; an unknown one is RefDb's error.
    pub fn named(name: &str) -> Result<Language, TextError> {
        LANGUAGES.iter().position(|(n, _, _)| *n == name).map(Language).ok_or_else(|| {
            bad_value(format!("unsupported language: \"{name}\" for text index version 3"))
        })
    }

    /// The language's name.
    pub fn name(self) -> &'static str {
        LANGUAGES[self.0].0
    }

    fn stemmer(self) -> Option<Stemmer> {
        LANGUAGES[self.0].1
    }

    fn is_stop_word(self, word: &str) -> bool {
        static LISTS: OnceLock<Vec<Vec<&'static str>>> = OnceLock::new();
        let lists = LISTS.get_or_init(|| {
            LANGUAGES
                .iter()
                .map(|(_, _, text)| {
                    let mut words: Vec<&str> =
                        text.lines().filter(|l| !l.starts_with('#')).collect();
                    words.sort_unstable();
                    words
                })
                .collect()
        });
        lists[self.0].binary_search(&word).is_ok()
    }

    /// Whether `c` ends a token in this language.
    fn is_delimiter(self, c: char) -> bool {
        if c == '\'' {
            return self.name() != "english";
        }
        in_table(delimiters(), c)
    }
}

/// Code point ranges of a data file (`XXXX..YYYY` or `XXXX` per line, `#` comments).
fn ranges(text: &str) -> Vec<(u32, u32)> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let (a, b) = l.split_once("..").unwrap_or((l, l));
            Some((u32::from_str_radix(a, 16).ok()?, u32::from_str_radix(b, 16).ok()?))
        })
        .collect()
}

fn delimiters() -> &'static [(u32, u32)] {
    static TABLE: OnceLock<Vec<(u32, u32)>> = OnceLock::new();
    TABLE.get_or_init(|| ranges(include_str!("../data/text-delimiters.txt")))
}

fn removed() -> &'static [(u32, u32)] {
    static TABLE: OnceLock<Vec<(u32, u32)>> = OnceLock::new();
    TABLE.get_or_init(|| ranges(include_str!("../data/text-removed.txt")))
}

fn kept() -> &'static [(u32, u32)] {
    static TABLE: OnceLock<Vec<(u32, u32)>> = OnceLock::new();
    TABLE.get_or_init(|| ranges(include_str!("../data/text-kept.txt")))
}

fn in_table(table: &[(u32, u32)], c: char) -> bool {
    let c = u32::from(c);
    let i = table.partition_point(|r| r.1 < c);
    table.get(i).is_some_and(|r| r.0 <= c)
}

/// How a token keeps its case and diacritics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sensitivity {
    /// `$caseSensitive`.
    pub case: bool,
    /// `$diacriticSensitive`.
    pub diacritic: bool,
}

/// `c` without its case.
fn fold_case(c: char) -> char {
    if in_table(kept(), c) {
        return c;
    }
    match c {
        // Simple case folding where it differs from the lowercase mapping.
        'İ' => 'İ',
        'ς' => 'σ',
        'µ' => 'μ',
        'ſ' => 's',
        'ͅ' => 'ι',
        'ϐ' => 'β',
        'ϑ' => 'θ',
        'ϕ' => 'φ',
        'ϖ' => 'π',
        'ϰ' => 'κ',
        'ϱ' => 'ρ',
        'ϵ' => 'ε',
        'ẛ' => 'ṡ',
        'ι' => 'ι',
        _ => unicode::lowercase(c),
    }
}

/// `c` without its diacritics: the base of its canonical decomposition; `None` when `c` is a
/// mark that goes.
fn strip(c: char) -> Option<char> {
    if in_table(removed(), c) {
        return None;
    }
    if in_table(kept(), c) {
        return Some(c);
    }
    let mut base = c;
    while let Some(mut d) = unicode::decomposition(base, false) {
        match d.next() {
            Some(first) if first != base => base = first,
            _ => break,
        }
    }
    Some(base)
}

/// The token `word` as compared: without case and diacritics unless `s` keeps them.
fn normalized(word: &str, s: Sensitivity) -> String {
    word.chars()
        .filter_map(|c| {
            let c = if s.case { c } else { fold_case(c) };
            if s.diacritic { (!in_table(removed(), c)).then_some(c) } else { strip(c) }
        })
        .collect()
}

/// The words of `text` in `language`, as written.
fn words(text: &str, language: Language) -> Vec<&str> {
    text.split(|c| language.is_delimiter(c)).filter(|w| !w.is_empty()).collect()
}

/// The stems of `text` in `language` (stop words left out), compared with sensitivity `s`.
pub fn stems(text: &str, language: Language, s: Sensitivity) -> Vec<String> {
    // Measured: the stemmer reads the word with its diacritics, which go from the stem.
    let keep_marks = Sensitivity { case: s.case, diacritic: true };
    words(text, language)
        .into_iter()
        .filter_map(|w| {
            let lower = normalized(w, Sensitivity { case: false, diacritic: true });
            if normalized(&lower, Sensitivity::default()).is_empty()
                || language.is_stop_word(&lower)
            {
                return None;
            }
            let word = if s.case { normalized(w, keep_marks) } else { lower };
            let stem = match language.stemmer() {
                Some(stemmer) => stem_as_refnode(stemmer, &word),
                None => word,
            };
            Some(if s.diacritic { stem } else { normalized(&stem, keep_marks_off(s)) })
        })
        .collect()
}

/// The first private-use code point, where the letters a stemmer must not know stand in.
const STAND_IN: u32 = 0xE000;

/// `word` stemmed as RefDb's stemmers do (measured, `tests/text_stems_refnode.rs`): the Hungarian
/// one takes a letter beyond Latin-1 (`ő`, `ű`) for an unknown consonant, which stops its
/// suffixes, and the Romanian one knows `ş` and `ţ` with cedilla but not with comma below.
fn stem_as_refnode(stemmer: Stemmer, word: &str) -> String {
    let hide: fn(char) -> bool = match stemmer {
        Stemmer::Hungarian => |c| u32::from(c) > 0xFF,
        Stemmer::Romanian => |c| matches!(c, 'ș' | 'ț'),
        _ => return stemmer.stem(word),
    };
    // The letters it does not know are hidden before the ones it knows take the current
    // stemmer's spelling: a Romanian `ș` with comma below is unknown to it, its `ş` with cedilla
    // is the current `ș`.
    let known = |c: char| match (stemmer, c) {
        (Stemmer::Romanian, 'ş') => 'ș',
        (Stemmer::Romanian, 'ţ') => 'ț',
        _ => c,
    };
    let mut foreign: Vec<char> = Vec::new();
    let hidden: String = word
        .chars()
        .map(|c| {
            if !hide(c) {
                return known(c);
            }
            let i = foreign.iter().position(|f| *f == c).unwrap_or_else(|| {
                foreign.push(c);
                foreign.len() - 1
            });
            char::from_u32(STAND_IN + i as u32).unwrap_or(c)
        })
        .collect();
    stemmer
        .stem(&hidden)
        .chars()
        .map(|c| match u32::from(c).checked_sub(STAND_IN) {
            Some(i) if (i as usize) < foreign.len() => foreign[i as usize],
            _ => c,
        })
        .collect()
}

/// `s` with the diacritics dropped.
fn keep_marks_off(s: Sensitivity) -> Sensitivity {
    Sensitivity { case: true, diacritic: s.diacritic }
}

/// A text index: the weight of each field (dotted path, or `$**` for every string), the default
/// language and the field that overrides it.
#[derive(Clone, Debug, PartialEq)]
pub struct TextIndex {
    weights: Vec<(String, f64)>,
    language: Language,
    language_override: String,
}

impl TextIndex {
    /// The text index of the stored specification `spec` (`weights`, `default_language`,
    /// `language_override`).
    pub fn from_spec(spec: &Document) -> Result<TextIndex, TextError> {
        let weights = match spec.get("weights") {
            Some(Value::Document(w)) => {
                w.iter().map(|(k, v)| (k.to_string(), number(v).unwrap_or(1.0))).collect()
            }
            _ => Vec::new(),
        };
        let language = match spec.get("default_language") {
            Some(Value::String(l)) => Language::named(l)?,
            _ => Language::named("english")?,
        };
        let language_override = match spec.get("language_override") {
            Some(Value::String(l)) => l.clone(),
            _ => LANGUAGE_OVERRIDE.to_owned(),
        };
        Ok(TextIndex { weights, language, language_override })
    }

    /// The index's `default_language`.
    pub fn language(&self) -> Language {
        self.language
    }

    /// The strings of `doc` this index reads: each with its weight and language.
    fn strings<'a>(&self, doc: &'a Document) -> Vec<(&'a str, f64, Language)> {
        let mut out = Vec::new();
        let language = self.language_of(doc, self.language);
        self.walk(doc, "", language, &mut out);
        out
    }

    fn language_of(&self, doc: &Document, outer: Language) -> Language {
        match doc.get(&self.language_override) {
            Some(Value::String(l)) => Language::named(l).unwrap_or(outer),
            _ => outer,
        }
    }

    fn weight_of(&self, path: &str) -> Option<f64> {
        self.weights
            .iter()
            .find(|(p, _)| p == path)
            .or_else(|| self.weights.iter().find(|(p, _)| p == "$**"))
            .map(|(_, w)| *w)
    }

    fn walk<'a>(
        &self,
        doc: &'a Document,
        prefix: &str,
        language: Language,
        out: &mut Vec<(&'a str, f64, Language)>,
    ) {
        for (k, v) in doc.iter() {
            let path = if prefix.is_empty() { k.to_string() } else { format!("{prefix}.{k}") };
            self.value(v, &path, language, out);
        }
    }

    fn value<'a>(
        &self,
        v: &'a Value,
        path: &str,
        language: Language,
        out: &mut Vec<(&'a str, f64, Language)>,
    ) {
        match v {
            Value::String(s) => {
                if let Some(w) = self.weight_of(path) {
                    out.push((s.as_str(), w, language));
                }
            }
            Value::Document(d) => {
                let inner = self.language_of(d, language);
                self.walk(d, path, inner, out);
            }
            Value::Array(items) => {
                for item in items {
                    self.value(item, path, language, out);
                }
            }
            _ => {}
        }
    }

    /// The score of `doc` for the stems of `query`, `None` when the document does not match it.
    pub fn score(&self, query: &TextQuery, doc: &Document) -> Option<f64> {
        let strings = self.strings(doc);
        let found_with = |terms: &[String], sensitivity: Sensitivity| -> bool {
            strings
                .iter()
                .any(|(s, _, l)| stems(s, *l, sensitivity).iter().any(|t| terms.contains(t)))
        };
        let found = |terms: &[String]| found_with(terms, query.sensitivity);
        // The index finds the document by a term without case and diacritics, and only then do
        // `$caseSensitive` and `$diacriticSensitive` filter it (measured: `CAFÉ` in a Spanish
        // document is not found by an English case-sensitive `CAFÉ`).
        let indexed = found_with(&query.plain_terms, Sensitivity::default());
        if query.terms.is_empty() || !indexed || !found(&query.terms) || found(&query.negated) {
            return None;
        }
        let has_phrase = |p: &str| -> bool {
            let p = normalized(p, query.sensitivity);
            strings.iter().any(|(s, _, _)| normalized(s, query.sensitivity).contains(&p))
        };
        if !query.phrases.iter().all(|p| has_phrase(p))
            || query.negated_phrases.iter().any(|p| has_phrase(p))
        {
            return None;
        }
        let terms: Vec<String> = query.plain_terms.clone();
        // One running sum over every string and term, as `refnode` adds (measured: 7.5 + 2.1 +
        // 1.2 gives 10.799999999999999, and 10.8 when each string is added up first).
        let mut total = 0.0;
        for (s, weight, language) in &strings {
            add_string_score(&mut total, s, *weight, *language, &terms);
        }
        Some(total)
    }
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int32(n) => Some(f64::from(*n)),
        Value::Int64(n) => Some(*n as f64),
        Value::Double(f) => Some(*f),
        _ => None,
    }
}

/// Adds to `total` the score of string `s` (weight `weight`, language `language`) for the
/// distinct `terms`, term by term.
fn add_string_score(total: &mut f64, s: &str, weight: f64, language: Language, terms: &[String]) {
    let all = stems(s, language, Sensitivity::default());
    let n = all.len() as f64;
    let mut seen: BTreeMap<&str, (f64, f64)> = BTreeMap::new();
    for t in &all {
        let e = seen.entry(t.as_str()).or_insert((0.0, 0.0));
        e.0 += 0.5f64.powi(e.1 as i32);
        e.1 += 1.0;
    }
    for (t, (freq, count)) in seen {
        if !terms.iter().any(|q| q == t) {
            continue;
        }
        let coeff = 0.5 * count / n + 0.5;
        let whole = if s.chars().count() == t.chars().count() && s.to_lowercase() == t {
            WHOLE_STRING
        } else {
            1.0
        };
        *total += weight * freq * coeff * whole;
    }
}

/// A `$text` query read for one language.
#[derive(Clone, Debug, PartialEq)]
pub struct TextQuery {
    /// The stems a document must hold one of, compared with [`TextQuery::sensitivity`].
    pub terms: Vec<String>,
    /// The same stems without case and diacritics, which the score sums.
    pub plain_terms: Vec<String>,
    /// The stems a document must not hold.
    pub negated: Vec<String>,
    /// The phrases a document must hold.
    pub phrases: Vec<String>,
    /// The phrases a document must not hold.
    pub negated_phrases: Vec<String>,
    /// `$caseSensitive` and `$diacriticSensitive`.
    pub sensitivity: Sensitivity,
}

/// What the query parser sees of `$search`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Piece {
    Start,
    Space,
    Quote,
    Dash,
    Delimiter,
    Text,
}

impl TextQuery {
    /// `$search` read in `language` with sensitivity `s`: `-` right after a space or at the
    /// start negates the next term or phrase until a space, quotes open and close a phrase (one
    /// left open is only its words) and the words of a phrase are terms unless it is negated.
    pub fn parse(search: &str, language: Language, s: Sensitivity) -> TextQuery {
        let mut q = TextQuery {
            terms: Vec::new(),
            plain_terms: Vec::new(),
            negated: Vec::new(),
            phrases: Vec::new(),
            negated_phrases: Vec::new(),
            sensitivity: s,
        };
        let (mut prev, mut negate, mut phrase) = (Piece::Start, false, None::<(usize, bool)>);
        let mut text_start: Option<usize> = None;
        let chars: Vec<(usize, char)> = search.char_indices().collect();
        let mut i = 0;
        // Past the last character a space ends the last word.
        loop {
            let (at, c) = chars.get(i).copied().unwrap_or((search.len(), ' '));
            let piece = match c {
                '"' => Piece::Quote,
                '-' => Piece::Dash,
                c if c.is_whitespace() => Piece::Space,
                c if language.is_delimiter(c) => Piece::Delimiter,
                _ => Piece::Text,
            };
            let end = i == chars.len();
            if piece != Piece::Text
                && let Some(start) = text_start.take()
            {
                let word = &search[start..at];
                let in_negated_phrase = phrase.is_some_and(|(_, n)| n);
                if !in_negated_phrase {
                    q.add_term(word, language, negate && phrase.is_none());
                }
                prev = Piece::Text;
            }
            if end {
                break;
            }
            match piece {
                Piece::Text => {
                    if text_start.is_none() {
                        text_start = Some(at);
                    }
                }
                Piece::Quote => match phrase.take() {
                    Some((start, negated)) => {
                        let text = &search[start..at];
                        if !text.is_empty() {
                            let list =
                                if negated { &mut q.negated_phrases } else { &mut q.phrases };
                            list.push(text.to_owned());
                        }
                    }
                    None => phrase = Some((at + 1, negate)),
                },
                Piece::Dash if phrase.is_none() && matches!(prev, Piece::Start | Piece::Space) => {
                    negate = true;
                }
                Piece::Space if phrase.is_none() => negate = false,
                _ => {}
            }
            if piece != Piece::Text {
                prev = piece;
            }
            i += 1;
        }
        q
    }

    fn add_term(&mut self, word: &str, language: Language, negated: bool) {
        let push = |list: &mut Vec<String>, t: String| {
            if !list.contains(&t) {
                list.push(t);
            }
        };
        for t in stems(word, language, self.sensitivity) {
            push(if negated { &mut self.negated } else { &mut self.terms }, t);
        }
        if !negated {
            for t in stems(word, language, Sensitivity::default()) {
                push(&mut self.plain_terms, t);
            }
        }
    }
}

#[cfg(test)]
#[path = "text_index_tests.rs"]
mod tests;
