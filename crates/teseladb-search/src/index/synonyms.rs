//! Synonym mappings (011 FR-003, FR-006, T036): the documents of a synonym collection analyzed
//! by the mapping's analyzer, as the reference search server 1.70.6 reads them (measured 02-11-2026,
//! `scratchpad/lane3l/m21.js`, `m22.js`). An `equivalent` document makes each of its words
//! stand for all of them, in the document's order; an `explicit` one makes each word of `input`
//! stand for its `synonyms` and then for itself. A document of another shape leaves the whole
//! mapping invalid until it is gone.

use teseladb_bson::{Document, Value};

use super::{IndexError, MemIndex, analysis};

/// `mappingType` of a document whose words all stand for each other.
pub const EQUIVALENT: &str = "equivalent";
/// `mappingType` of a document whose `input` words stand for its `synonyms`.
pub const EXPLICIT: &str = "explicit";

/// One synonym document, each entry as its analyzed tokens.
#[derive(Clone, Debug, PartialEq)]
enum Rule {
    Equivalent(Vec<Vec<String>>),
    Explicit { input: Vec<Vec<String>>, synonyms: Vec<Vec<String>> },
}

/// One position of a `text` query with `synonyms` and `matchCriteria` (measured 02-11-2026,
/// `scratchpad/lane3m/m27.js`, `synonyms_all.js`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Segment {
    /// A token and, when a rule of single words names it, what it stands for with the token
    /// again at the end (as [`SynonymMap::expand`] with `keep_original`).
    Word(String, Option<Vec<String>>),
    /// A run of tokens a rule with an entry of several words names: the rule's entries, then
    /// the run again. The reference search library asks for any of them (`DisjunctionMax`).
    Group(Vec<Vec<String>>),
}

/// A synonym mapping as the index holds it.
#[derive(Clone, Debug, PartialEq, Default)]
pub(crate) struct SynonymMap {
    rules: Vec<Rule>,
    /// Whether a document of the collection is not a synonym document.
    invalid: bool,
}

/// The strings of field `name` of `d`, if it is an array of strings.
fn strings(d: &Document, name: &str) -> Option<Vec<String>> {
    let Some(Value::Array(items)) = d.get(name) else { return None };
    items.iter().map(|v| if let Value::String(s) = v { Some(s.clone()) } else { None }).collect()
}

impl SynonymMap {
    /// The words `token` stands for, without repeats (`text`), or with the token added at the
    /// end of an `equivalent` group as the reference search library's graph keeps it (`phrase`, `keep_original`).
    /// `None` when no rule names the token.
    pub(crate) fn expand(&self, token: &str, keep_original: bool) -> Option<Vec<String>> {
        let single = |words: &[Vec<String>]| -> Vec<String> {
            words.iter().filter(|w| w.len() == 1).map(|w| w[0].clone()).collect()
        };
        let named = |words: &[Vec<String>]| words.iter().any(|w| w.len() == 1 && w[0] == token);
        let mut out: Vec<String> = Vec::new();
        let mut found = false;
        for rule in &self.rules {
            match rule {
                Rule::Equivalent(words) if named(words) => {
                    found = true;
                    out.extend(single(words));
                }
                Rule::Explicit { input, synonyms } if named(input) => {
                    found = true;
                    out.extend(single(synonyms));
                }
                _ => {}
            }
        }
        if !found {
            return None;
        }
        out.push(token.to_owned());
        if keep_original {
            return Some(out);
        }
        let mut unique: Vec<String> = Vec::new();
        for w in out {
            if !unique.contains(&w) {
                unique.push(w);
            }
        }
        Some(unique)
    }

    /// The token sequences `tokens` stands for when a rule of several words is involved (the reference search library
    /// then builds a `TermAutomatonQuery`, measured): each run of tokens a rule names, longest
    /// first, is replaced by every entry of the rule, `None` when no such rule is involved.
    pub(crate) fn multi_word(&self, tokens: &[String]) -> Option<Vec<Vec<String>>> {
        const MOST_PATHS: usize = 1024;
        let mut segments: Vec<Vec<Vec<String>>> = Vec::new();
        let mut involved = false;
        let mut i = 0;
        while i < tokens.len() {
            let rest = &tokens[i..];
            let mut best: Option<(usize, Vec<Vec<String>>)> = None;
            for rule in &self.rules {
                let (names, alternatives): (&[Vec<String>], Vec<Vec<String>>) = match rule {
                    Rule::Equivalent(words) => (words, words.clone()),
                    Rule::Explicit { input, synonyms } => (input, synonyms.clone()),
                };
                for entry in names.iter().filter(|e| !e.is_empty() && rest.starts_with(e)) {
                    if best.as_ref().is_none_or(|(len, _)| entry.len() > *len) {
                        let mut alts = alternatives.clone();
                        if !alts.contains(entry) {
                            alts.push(entry.clone());
                        }
                        best = Some((entry.len(), alts));
                    }
                }
            }
            match best {
                Some((len, alts)) => {
                    involved |= len > 1 || alts.iter().any(|a| a.len() > 1);
                    segments.push(alts);
                    i += len;
                }
                None => {
                    segments.push(vec![vec![tokens[i].clone()]]);
                    i += 1;
                }
            }
        }
        if !involved {
            return None;
        }
        let mut paths: Vec<Vec<String>> = vec![Vec::new()];
        for alts in &segments {
            paths = paths
                .iter()
                .flat_map(|p| alts.iter().map(move |a| p.iter().chain(a).cloned().collect()))
                .take(MOST_PATHS)
                .collect();
        }
        Some(paths)
    }

    /// The positions of `tokens` for `matchCriteria`: at each token, the longest entry of a
    /// rule with an entry of several words that starts there makes a [`Segment::Group`] of
    /// every rule naming it; any other token is a [`Segment::Word`].
    pub(crate) fn segments(&self, tokens: &[String]) -> Vec<Segment> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < tokens.len() {
            let rest = &tokens[i..];
            let mut best: Option<(&[String], Vec<Vec<String>>)> = None;
            for rule in &self.rules {
                let (names, alternatives) = match rule {
                    Rule::Equivalent(words) => (words, words),
                    Rule::Explicit { input, synonyms } => (input, synonyms),
                };
                let several = names.iter().chain(alternatives).any(|w| w.len() > 1);
                for entry in
                    names.iter().filter(|e| several && !e.is_empty() && rest.starts_with(e))
                {
                    match &mut best {
                        Some((b, alts)) if b.len() == entry.len() => {
                            alts.extend_from_slice(alternatives)
                        }
                        Some((b, _)) if b.len() > entry.len() => {}
                        _ => best = Some((entry.as_slice(), alternatives.clone())),
                    }
                }
            }
            match best {
                Some((entry, mut alts)) => {
                    alts.push(entry.to_vec());
                    out.push(Segment::Group(alts));
                    i += entry.len();
                }
                None => {
                    out.push(Segment::Word(tokens[i].clone(), self.expand(&tokens[i], true)));
                    i += 1;
                }
            }
        }
        out
    }

    /// Whether the collection held a document that is not a synonym document.
    pub(crate) fn is_invalid(&self) -> bool {
        self.invalid
    }
}

impl MemIndex {
    /// Loads the documents of the collection of synonym mapping `name`, analyzed by the
    /// mapping's analyzer. A mapping the definition does not name is ignored.
    pub fn set_synonyms(&mut self, name: &str, docs: &[Document]) -> Result<(), IndexError> {
        let Some(mapping) = self.definition.synonyms.iter().find(|s| s.name == name) else {
            return Ok(());
        };
        let analyzer = self.analyzer(&mapping.analyzer)?.into_owned();
        let words = |list: Vec<String>| -> Result<Vec<Vec<String>>, IndexError> {
            list.iter()
                .map(|w| {
                    let tokens = analyzer.analyze(w).map_err(analysis)?;
                    Ok(tokens.into_iter().map(|t| t.text).collect())
                })
                .collect()
        };
        let mut map = SynonymMap::default();
        for d in docs {
            let kind = match d.get("mappingType") {
                Some(Value::String(k)) => k.as_str(),
                _ => "",
            };
            let rule = match (kind, strings(d, "synonyms"), strings(d, "input")) {
                (EQUIVALENT, Some(s), _) => Rule::Equivalent(words(s)?),
                (EXPLICIT, Some(s), Some(i)) => {
                    Rule::Explicit { input: words(i)?, synonyms: words(s)? }
                }
                _ => {
                    map.invalid = true;
                    continue;
                }
            };
            map.rules.push(rule);
        }
        self.synonyms.insert(name.to_owned(), map);
        Ok(())
    }

    /// Synonym mapping `name` as loaded, empty before its collection is read.
    pub(crate) fn synonym_map(&self, name: &str) -> Option<&SynonymMap> {
        self.synonyms.get(name)
    }
}
