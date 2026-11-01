//! The `wildcard` and `regex` operators (011 FR-002), as `refsearch` 1.70.6 runs them (measured
//! 02-11-2026): each query is a pattern over whole terms, normalized as the field's analyzer
//! normalizes text (lower case, but for `reflib.keyword`), and every query and path a document
//! matches adds the operator's score, 1 without a `score` option. A field whose query analyzer is
//! not `reflib.keyword` needs `allowAnalyzedField: true`.

use teseladb_bson::{Document, Value};

use super::{Operator, Score, SearchError, parse_score, paths, shape};
use crate::analysis::unicode::lowercase;
use crate::index::MemIndex;
use crate::query::regex::Regex;

/// The analyzer whose fields take patterns as they are.
const KEYWORD: &str = "reflib.keyword";

/// A `wildcard` or `regex` operator.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Pattern {
    /// `true` for `regex`.
    regex: bool,
    query: Vec<String>,
    paths: Vec<String>,
    score: Score,
}

/// Operator `name` (`wildcard` or `regex`) with its options `d` at `path` of the query.
pub(super) fn parse(
    index: &MemIndex,
    name: &str,
    d: &Document,
    path: &str,
) -> Result<Operator, SearchError> {
    const OPTIONS: [&str; 4] = ["query", "path", "allowAnalyzedField", "score"];
    if let Some((k, _)) = d.iter().find(|(k, _)| !OPTIONS.contains(k)) {
        return Err(SearchError(format!("\"{path}\" unrecognized field \"{k}\"")));
    }
    let at = |f: &str| format!("{path}.{f}");
    let query = match d.get("query") {
        None => return Err(shape(&at("query"), "is required")),
        Some(Value::String(s)) if s.is_empty() => {
            return Err(shape(&at("query"), "cannot be empty"));
        }
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(items)) if items.is_empty() => {
            return Err(shape(&at("query"), "cannot be empty"));
        }
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| match i {
                Value::String(s) => Ok(s.clone()),
                _ => Err(shape(&at("query"), "must be a string")),
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(shape(&at("query"), "must be a string")),
    };
    let named = match d.get("path") {
        None => return Err(shape(&at("path"), "is required")),
        Some(p) => p,
    };
    let allow = match d.get("allowAnalyzedField") {
        None => false,
        Some(Value::Boolean(b)) => *b,
        Some(_) => return Err(shape(&at("allowAnalyzedField"), "must be a boolean")),
    };
    let score = parse_score(d.get("score"), &at("score"))?;
    let paths = paths(index, named, &at("path"))?;
    let analyzed = paths.iter().find(|p| index.query_analyzer(p) != KEYWORD);
    if let (false, Some(p)) = (allow, analyzed) {
        return Err(SearchError(format!(
            "Field {p} is analyzed. Use a keyword analyzed field or set allowAnalyzedField to true."
        )));
    }
    Ok(Operator::Pattern(Pattern { regex: name == "regex", query, paths, score }))
}

impl Pattern {
    /// The documents each query and path match, with the number of matches times `boost`.
    pub(super) fn scores(
        &self,
        index: &MemIndex,
        boost: f32,
    ) -> Result<std::collections::BTreeMap<u32, f32>, SearchError> {
        let mut out = std::collections::BTreeMap::new();
        for path in &self.paths {
            let keyword = index.query_analyzer(path) == KEYWORD;
            for q in &self.query {
                let q: String =
                    if keyword { q.clone() } else { q.chars().map(lowercase).collect() };
                let compiled = if self.regex { Regex::new(&q) } else { Ok(wildcard(&q)) };
                let re = compiled.map_err(|e| SearchError(e.message(&q)))?;
                for doc in index.matching_terms(path, |t| re.is_full_match(t)) {
                    *out.entry(doc).or_insert(0.0) += boost;
                }
            }
        }
        Ok(out)
    }

    /// The score option.
    pub(super) fn score(&self) -> &Score {
        &self.score
    }
}

/// A `wildcard` pattern; a `\` at its end stands for itself (measured: `a\` finds nothing and
/// is no error).
fn wildcard(q: &str) -> Regex {
    let fixed = if q.ends_with('\\') && !q.ends_with("\\\\") { format!("{q}\\") } else { q.into() };
    Regex::wildcard(&fixed).unwrap_or_else(|_| Regex::new("#").expect("the empty language"))
}
