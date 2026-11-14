//! `highlight` (011 FR-003, FR-015, T035) as the reference search server 1.70.6 gives it (the reference search library's
//! `UnifiedHighlighter`, measured 02-11-2026, `scratchpad/lane3k/m17.js` and
//! `scratchpad/lane3l/m25.js`), computed only for the documents of the final result. The text
//! of each path (the values of an array joined by U+2029, where passages are also cut) is cut
//! into sentences ([`sentences`]); each sentence with a term of the query is a passage, scored
//! as the reference search library's `PassageScorer`: the sum over its terms of
//! `(k1 + 1) · ln(1 + (1 + len / 87 + 0.5) / (ttf + 0.5)) · f / (f + k1 · (1 − b + b · plen / 87))`
//! times `1 + 1 / ln(87 + start)`, in 32-bit floats, with `k1 = 1.2`, `b = 0.75`, `len` the
//! length of the text, `ttf` the term's highlighted occurrences in the field, `f` those in the
//! passage and `plen` the passage's length, all in UTF-16 units. The best `maxNumPassages`
//! passages are given in text order.

use teseladb_bson::{Document, Value};

use super::pattern::Pattern;
use super::{Operator, SearchError, paths, shape, value_at};
use crate::index::MemIndex;
use crate::query::regex::Regex;

/// `maxNumPassages` and `maxCharsToExamine` by default.
const DEFAULT_PASSAGES: usize = 5;
const DEFAULT_CHARS: usize = 500_000;
/// The reference search library's `PassageScorer` constants.
const K1: f32 = 1.2;
const B: f32 = 0.75;
const PIVOT: f32 = 87.0;
/// What joins the values of an array, and where passages are cut.
const SEPARATOR: char = '\u{2029}';

/// The `highlight` option.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Highlight {
    paths: Vec<String>,
    passages: usize,
    chars: usize,
}

/// What the query asks for on a field.
enum Wanted {
    Term(String),
    Phrase(Vec<String>, u32),
    Pattern(Pattern),
}

/// The `highlight` option `v`.
pub(super) fn parse(index: &MemIndex, v: &Value) -> Result<Highlight, SearchError> {
    let Value::Document(d) = v else { return Err(shape("highlight", "must be a document")) };
    if let Some((k, _)) =
        d.iter().find(|(k, _)| !["path", "maxNumPassages", "maxCharsToExamine"].contains(k))
    {
        return Err(SearchError(format!("\"highlight\" unrecognized field \"{k}\"")));
    }
    let paths = match d.get("path") {
        None => return Err(shape("highlight.path", "is required")),
        Some(p) => paths(index, p, "highlight.path")?,
    };
    let positive = |name: &str, default: usize| -> Result<usize, SearchError> {
        let at = format!("highlight.{name}");
        let n = match d.get(name) {
            None | Some(Value::Null) => return Ok(default),
            Some(v) => int32(v, &at)?,
        };
        if n <= 0 {
            return Err(shape(&at, "must be positive"));
        }
        // Measured: the reference search library refuses the largest 32-bit integer, each option with its message.
        if n == i32::MAX {
            return Err(SearchError(
                if name == "maxCharsToExamine" {
                    "maxLength must be < Integer.MAX_VALUE"
                } else {
                    "IllegalArgumentException"
                }
                .to_owned(),
            ));
        }
        Ok(usize::try_from(n).unwrap_or(usize::MAX))
    };
    let passages = positive("maxNumPassages", DEFAULT_PASSAGES)?;
    let chars = positive("maxCharsToExamine", DEFAULT_CHARS)?;
    Ok(Highlight { paths, passages, chars })
}

/// The 32-bit integer `v` of the option at `at`, with the reference search server's messages (measured 02-11-2026,
/// `scratchpad/lane3m/m27.js` to `m29.js`): a double must be whole and a number must fit.
fn int32(v: &Value, at: &str) -> Result<i32, SearchError> {
    let wide = match v {
        Value::Int32(n) => return Ok(*n),
        Value::Int64(n) => *n as f64,
        Value::Double(f) if f.is_nan() => return Err(shape(at, "must be an integer")),
        Value::Double(f) if f.is_infinite() => *f,
        Value::Double(f) if f.fract() != 0.0 => return Err(shape(at, "must be an integer")),
        Value::Double(f) => *f,
        _ => return Err(shape(at, "must be a integer")),
    };
    if wide > f64::from(i32::MAX) {
        return Err(shape(at, "overflowed, must fit in a 32 bit integer"));
    }
    if wide < f64::from(i32::MIN) {
        return Err(shape(at, "underflowed, must fit in a 32 bit integer"));
    }
    Ok(wide as i32)
}

/// What `op` asks for, added to `out` (the clauses that exclude ask for nothing).
fn wanted(index: &MemIndex, op: &Operator, out: &mut Vec<Wanted>) {
    match op {
        Operator::Text { query, paths, fuzzy, .. } => {
            for p in paths {
                for q in query {
                    for t in index.query_terms(p, q).unwrap_or_default() {
                        match fuzzy {
                            Some(f) => out.extend(
                                index
                                    .fuzzy_terms(p, &t, f)
                                    .into_iter()
                                    .map(|(t, _)| Wanted::Term(t)),
                            ),
                            None => out.push(Wanted::Term(t)),
                        }
                    }
                }
            }
        }
        Operator::Phrase { query, paths, slop, .. } => {
            for p in paths {
                for q in query {
                    let terms = index.query_terms(p, q).unwrap_or_default();
                    out.push(Wanted::Phrase(terms, *slop));
                }
            }
        }
        Operator::Synonyms(s) => {
            for p in &s.paths {
                for q in &s.query {
                    for t in index.query_terms(p, q).unwrap_or_default() {
                        let more = index
                            .synonym_map(&s.mapping)
                            .and_then(|m| m.expand(&t, false))
                            .unwrap_or_else(|| vec![t.clone()]);
                        out.extend(more.into_iter().map(Wanted::Term));
                    }
                }
            }
        }
        Operator::Pattern(p) => out.push(Wanted::Pattern(p.clone())),
        Operator::Compound { must, should, filter, .. } => {
            for o in must.iter().chain(should).chain(filter) {
                wanted(index, o, out);
            }
        }
        Operator::Scored(inner, _) | Operator::AllTerms(inner) => wanted(index, inner, out),
        _ => {}
    }
}

/// A token of the text: its term, its UTF-16 offsets and its position.
struct Tok {
    term: String,
    start: usize,
    end: usize,
    position: u32,
}

/// The UTF-16 offset of each byte offset of `text` (one more for its end).
fn utf16_offsets(text: &str) -> Vec<usize> {
    let mut out = vec![0; text.len() + 1];
    let mut at = 0;
    for (i, c) in text.char_indices() {
        out[i] = at;
        at += c.len_utf16();
    }
    out[text.len()] = at;
    out
}

impl Highlight {
    /// The highlights of document `doc` of `index` for the query `op`.
    pub(super) fn of(&self, index: &MemIndex, op: &Operator, doc: &Document) -> Value {
        let mut asked = Vec::new();
        wanted(index, op, &mut asked);
        let mut out = Vec::new();
        for path in &self.paths {
            out.extend(self.of_path(index, path, &asked, doc));
        }
        Value::Array(out)
    }

    fn of_path(
        &self,
        index: &MemIndex,
        path: &str,
        asked: &[Wanted],
        doc: &Document,
    ) -> Vec<Value> {
        let (values, split) = match value_at(doc, path) {
            Some(Value::String(s)) => (vec![s.as_str()], false),
            Some(Value::Array(items)) => (
                items
                    .iter()
                    .filter_map(|v| if let Value::String(s) = v { Some(s.as_str()) } else { None })
                    .collect(),
                true,
            ),
            _ => return Vec::new(),
        };
        let whole: String = values.join(&SEPARATOR.to_string());
        let analyzer = index.query_analyzer(path).to_owned();
        // The tokens of every value, with offsets in the joined text.
        let mut toks: Vec<Tok> = Vec::new();
        let mut base = 0;
        let mut position_base = 0;
        let units = utf16_offsets(&whole);
        for v in &values {
            let tokens = index.analyze_tokens(&analyzer, v).unwrap_or_default();
            for t in &tokens {
                toks.push(Tok {
                    term: t.text.clone(),
                    start: units[base + t.start],
                    end: units[base + t.end],
                    position: position_base + t.position,
                });
            }
            position_base += tokens.iter().map(|t| t.position + 1).max().unwrap_or(0) + 100;
            base += v.len() + SEPARATOR.len_utf8();
        }
        let matchers: Vec<Vec<Regex>> = asked
            .iter()
            .map(|w| if let Wanted::Pattern(p) = w { p.matchers(index, path) } else { Vec::new() })
            .collect();
        let mut hits: Vec<usize> = Vec::new();
        for (i, tok) in toks.iter().enumerate() {
            let hit = asked.iter().zip(&matchers).any(|(w, m)| match w {
                Wanted::Term(t) => *t == tok.term,
                Wanted::Pattern(_) => m.iter().any(|r| r.is_full_match(&tok.term)),
                Wanted::Phrase(terms, slop) => in_phrase(&toks, i, terms, *slop),
            });
            if hit {
                hits.push(i);
            }
        }
        // The text the highlighter looks at, at most `maxCharsToExamine` UTF-16 units.
        let text: Vec<u16> = whole.encode_utf16().take(self.chars).collect();
        let len = text.len();
        let all_hits: Vec<&Tok> = hits.into_iter().map(|i| &toks[i]).collect();
        let hits: Vec<&Tok> = all_hits.iter().copied().filter(|t| t.end <= len).collect();
        let mut passages: Vec<(f32, usize, usize)> = Vec::new();
        for (s, e) in sentences(&text, split) {
            let inside: Vec<&&Tok> = hits.iter().filter(|t| t.start >= s && t.end <= e).collect();
            if inside.is_empty() {
                continue;
            }
            let mut score = 0.0f32;
            let mut seen: Vec<&str> = Vec::new();
            for t in &inside {
                if seen.contains(&t.term.as_str()) {
                    continue;
                }
                seen.push(&t.term);
                let f = inside.iter().filter(|o| o.term == t.term).count() as f32;
                // Measured: the term's highlighted occurrences in the whole field (with a
                // phrase, only those of its matches).
                let ttf = all_hits.iter().filter(|o| o.term == t.term).count() as f32;
                score += tf(f, (e - s) as f32) * weight(len as f32, ttf);
            }
            score *= 1.0 + 1.0 / (f64::from(PIVOT + s as f32).ln() as f32);
            passages.push((score, s, e));
        }
        passages.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        passages.truncate(self.passages);
        passages.sort_by_key(|p| p.1);
        passages
            .into_iter()
            .map(|(score, s, e)| {
                let mut texts = Vec::new();
                let mut at = s;
                for t in hits.iter().filter(|t| t.start >= s && t.end <= e) {
                    if t.start < at {
                        continue;
                    }
                    if t.start > at {
                        texts.push(piece(&text[at..t.start], "text"));
                    }
                    texts.push(piece(&text[t.start..t.end], "hit"));
                    at = t.end;
                }
                if at < e {
                    texts.push(piece(&text[at..e], "text"));
                }
                Value::Document(
                    Document::new()
                        .with("score", f64::from(score))
                        .with("path", path)
                        .with("texts", Value::Array(texts)),
                )
            })
            .collect()
    }
}

fn piece(units: &[u16], kind: &str) -> Value {
    Value::Document(
        Document::new().with("value", String::from_utf16_lossy(units)).with("type", kind),
    )
}

/// The reference search library's `PassageScorer.tf`.
fn tf(freq: f32, passage: f32) -> f32 {
    let norm = K1 * ((1.0 - B) + B * (passage / PIVOT));
    freq / (freq + norm)
}

/// The reference search library's `PassageScorer.weight`.
fn weight(length: f32, ttf: f32) -> f32 {
    let docs = 1.0 + length / PIVOT;
    (K1 + 1.0) * (1.0 + (f64::from(docs) + 0.5) / (f64::from(ttf) + 0.5)).ln() as f32
}

/// Whether token `i` of `toks` is part of a match of the phrase `terms` (at most `slop` extra
/// positions between them).
fn in_phrase(toks: &[Tok], i: usize, terms: &[String], slop: u32) -> bool {
    let Some(k) = terms.iter().position(|t| *t == toks[i].term) else { return false };
    let at = i64::from(toks[i].position) - k as i64;
    let slop = i64::from(slop);
    terms.iter().enumerate().all(|(j, term)| {
        toks.iter()
            .any(|o| o.term == *term && (i64::from(o.position) - (at + j as i64)).abs() <= slop)
    })
}

/// Whether a UTF-16 unit is a space for the sentence rules.
fn space(c: u16) -> bool {
    matches!(c, 0x09..=0x0D | 0x20 | 0xA0 | 0x1680 | 0x2000..=0x200A | 0x2028 | 0x202F | 0x205F | 0x3000)
}

/// Closing punctuation that may follow a sentence's end.
fn closing(c: u16) -> bool {
    matches!(
        c,
        0x22 | 0x27
            | 0x29
            | 0x5D
            | 0x7D
            | 0xBB
            | 0x2019
            | 0x201D
            | 0x203A
            | 0x3009
            | 0x300B
            | 0x300D
            | 0x300F
            | 0x3011
            | 0xFF09
    )
}

fn period(c: u16) -> bool {
    matches!(c, 0x2E | 0xFF0E)
}

fn terminator(c: u16) -> bool {
    matches!(c, 0x21 | 0x3F | 0x3002 | 0xFF01 | 0xFF1F | 0xFF61)
}

/// Whether `c` keeps a sentence going after a period and one space: a lower-case letter or a
/// digit (measured).
fn continues(c: u16) -> bool {
    char::from_u32(u32::from(c)).is_some_and(|c| c.is_lowercase() || c.is_numeric())
}

/// The sentences of `text` as `[start, end)` in UTF-16 units, by the rules the reference search server's highlighter
/// follows (Java's sentence `BreakIterator`, measured in `m25.js`): a `!`, `?` or `。` ends a
/// sentence after the closing marks and spaces that follow; a period does when spaces follow it
/// and the next character after one space is no lower-case letter or digit, or more than one
/// space follows; U+2029 ends one after it. With `split` (the values of an array) U+2029 is cut
/// out between two passages.
pub(super) fn sentences(text: &[u16], split: bool) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let n = text.len();
    while i < n {
        let c = text[i];
        if c == 0x2029 {
            if split {
                if i > start {
                    out.push((start, i));
                }
                start = i + 1;
            } else {
                out.push((start, i + 1));
                start = i + 1;
            }
            i += 1;
            continue;
        }
        let is_term = terminator(c);
        if !is_term && !period(c) {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < n && (terminator(text[j]) || period(text[j]) || closing(text[j])) {
            j += 1;
        }
        let spaces_from = j;
        while j < n && space(text[j]) {
            j += 1;
        }
        let spaces = j - spaces_from;
        let ends = is_term || j >= n || (spaces > 0 && (spaces > 1 || !continues(text[j])));
        if ends {
            out.push((start, j));
            start = j;
        }
        i = j.max(i + 1);
    }
    if start < n {
        out.push((start, n));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cut(text: &str) -> Vec<String> {
        let units: Vec<u16> = text.encode_utf16().collect();
        sentences(&units, false)
            .into_iter()
            .map(|(s, e)| String::from_utf16_lossy(&units[s..e]))
            .collect()
    }

    /// Each separator of `m25.js` between `uno`, `gato dos` and `tres`: whether it ends a
    /// sentence (measured).
    #[test]
    fn sentences_break_where_the_reference_breaks() {
        let cases: [(&str, bool); 30] = [
            (". ", false),
            (".  ", true),
            (". a ", false),
            ("! ", true),
            ("? ", true),
            ("?! ", true),
            ("... ", false),
            (".\n", false),
            ("\n", false),
            ("\n\n", false),
            ("\r\n", false),
            ("; ", false),
            (": ", false),
            (", ", false),
            (".\" ", false),
            (".) ", false),
            (".\u{a0}", false),
            (".\t", false),
            ("。", true),
            (". 3 ", false),
            (". \"", true),
            (".' ", false),
            ("\u{2029}", true),
            ("\u{2028}", false),
            ("\u{85}", false),
            (". ¿", true),
            ("?\n", true),
            (" - ", false),
            (".]", false),
            (". (", true),
        ];
        for (sep, breaks) in cases {
            let text = format!("uno{sep}gato dos{sep}tres");
            let got = cut(&text);
            assert_eq!(got.len() == 3, breaks, "{sep:?}: {got:?}");
            assert_eq!(got.concat(), text);
        }
        assert_eq!(cut("uno.\u{3000}gato"), vec!["uno.\u{3000}gato"]);
    }

    #[test]
    fn values_of_an_array_are_cut_apart() {
        let units: Vec<u16> = "gato uno\u{2029}dos gatos".encode_utf16().collect();
        assert_eq!(sentences(&units, true), vec![(0, 8), (9, 18)]);
        assert_eq!(sentences(&units, false), vec![(0, 9), (9, 18)]);
    }
}
