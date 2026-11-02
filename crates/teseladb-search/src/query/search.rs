//! The `$search` specification over an index (011 FR-002, T022): its operator, checked with the
//! errors `refsearch` 1.70.6 gives (measured on a 9.0.4 replica set, 01-11-2026), and the scored
//! documents it selects. `compound` adds the scores of its `must` and `should` clauses in 32-bit
//! floats, `filter` matches without scoring, `mustNot` excludes, and a `compound` with only
//! `mustNot` returns every other document at score 0.

use std::collections::BTreeMap;

use teseladb_bson::{Document, Value};

use crate::index::{Fuzzy, Hit, IndexError, MemIndex};

/// The operators `refsearch` knows, in its order.
pub const OPERATORS: [&str; 23] = [
    "autocomplete",
    "compound",
    "embeddedDocument",
    "equals",
    "exists",
    "geoShape",
    "geoWithin",
    "hasAncestor",
    "hasRoot",
    "in",
    "knnBeta",
    "moreLikeThis",
    "near",
    "phrase",
    "queryString",
    "range",
    "regex",
    "search",
    "span",
    "term",
    "text",
    "vectorSearch",
    "wildcard",
];

/// The options of a `$search` beside its operator that this layer leaves to the stage.
const STAGE_OPTIONS: [&str; 10] = [
    "index",
    "highlight",
    "count",
    "sort",
    "scoreDetails",
    "returnStoredSource",
    "searchAfter",
    "searchBefore",
    "concurrent",
    "tracking",
];

/// A `$search` the index cannot run: `refsearch`'s message (code 8, `UnknownError`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchError(pub String);

impl From<IndexError> for SearchError {
    fn from(e: IndexError) -> SearchError {
        SearchError(match e {
            IndexError::Analyzer(name) => format!("references non-existent analyzers: {name}"),
            IndexError::Analysis(message) => message,
        })
    }
}

fn shape(path: &str, what: &str) -> SearchError {
    SearchError(format!("\"{path}\" {what}"))
}

/// How an operator's score is changed.
#[derive(Clone, Debug, PartialEq)]
enum Score {
    /// As computed.
    Plain,
    /// Multiplied by a number.
    Boost(f32),
    /// Multiplied by the number at a path of the document (0 without one).
    BoostPath(String),
    /// Replaced by a number.
    Constant(f32),
    /// Replaced by an expression over it.
    Function(function::Expr),
}

/// An operator.
#[derive(Clone, Debug, PartialEq)]
enum Operator {
    Text {
        query: Vec<String>,
        paths: Vec<String>,
        fuzzy: Option<Fuzzy>,
        score: Score,
    },
    Exists {
        path: String,
        score: Score,
    },
    Phrase {
        query: Vec<String>,
        paths: Vec<String>,
        slop: u32,
        score: Score,
    },
    /// `wildcard` or `regex`.
    Pattern(pattern::Pattern),
    /// `equals`, `in`, `range` or `near`.
    Values(values::ValueOp),
    /// `knnBeta`.
    Knn(knn::Knn),
    /// The documents with a term of `path` between the bounds, each with whether it is
    /// inclusive (`queryString`'s `[a TO b]`): a constant score.
    TermRange {
        path: String,
        lower: Option<(String, bool)>,
        upper: Option<(String, bool)>,
        score: Score,
    },
    /// Nothing (a `queryString` of only excluded clauses).
    Empty,
    /// An operator whose scores `score` changes as a whole.
    Scored(Box<Operator>, Score),
    /// `span`, with its score option.
    Span(span::Span, Score),
    Compound {
        must: Vec<Operator>,
        should: Vec<Operator>,
        filter: Vec<Operator>,
        must_not: Vec<Operator>,
        minimum: usize,
    },
}

/// The scored documents `spec` selects in `index`, best first.
pub fn run(index: &MemIndex, spec: &Document) -> Result<Vec<Hit>, SearchError> {
    let op = parse_root(index, spec)?;
    let scores = eval(index, &op)?;
    Ok(index.hits(scores))
}

/// The document of `$searchMeta` (and `$$SEARCH_META`) for `spec`: the count of the documents
/// it selects, as `lowerBound` unless `count.type` is `total` (measured: the count is exact).
pub fn meta(index: &MemIndex, spec: &Document) -> Result<Document, SearchError> {
    let total = count_total(spec)?;
    Ok(count_document(total, run(index, spec)?.len()))
}

/// Whether `count.type` of `spec` asks for `total` rather than `lowerBound`.
pub fn count_total(spec: &Document) -> Result<bool, SearchError> {
    match spec.get("count") {
        None => Ok(false),
        Some(Value::Document(c)) => match c.get("type") {
            None => Ok(false),
            Some(Value::String(t)) if t == "lowerBound" => Ok(false),
            Some(Value::String(t)) if t == "total" => Ok(true),
            Some(_) => Err(shape("count.type", "must be one of [lowerBound, total]")),
        },
        Some(_) => Err(shape("count", "must be a document")),
    }
}

/// The `$searchMeta` document of `n` selected documents.
pub fn count_document(total: bool, n: usize) -> Document {
    let n = i64::try_from(n).unwrap_or(i64::MAX);
    let name = if total { "total" } else { "lowerBound" };
    Document::new().with("count", Document::new().with(name, n))
}

fn parse_root(index: &MemIndex, spec: &Document) -> Result<Operator, SearchError> {
    let ops: Vec<(&str, &Value)> = spec.iter().filter(|(k, _)| OPERATORS.contains(k)).collect();
    match ops.as_slice() {
        [] => Err(SearchError(format!(
            "Query should contain either an operator [{}] or a collector [facet]",
            OPERATORS.join(", ")
        ))),
        [(name, v)] => parse(index, name, v, name),
        _ => Err(SearchError(format!("at most one of [{}] may be present", OPERATORS.join(", ")))),
    }
    .and_then(|op| {
        match spec.iter().find(|(k, _)| !OPERATORS.contains(k) && !STAGE_OPTIONS.contains(k)) {
            Some((k, _)) => Err(SearchError(format!("unrecognized field \"{k}\""))),
            None => Ok(op),
        }
    })
}

/// Operator `name` with argument `v` at `path` of the query.
fn parse(index: &MemIndex, name: &str, v: &Value, path: &str) -> Result<Operator, SearchError> {
    let Value::Document(d) = v else { return Err(shape(path, "must be a document")) };
    match name {
        "text" => parse_text(index, d, path),
        "phrase" => parse_phrase(index, d, path),
        "exists" => parse_exists(d, path),
        "wildcard" | "regex" => pattern::parse(index, name, d, path),
        "compound" => parse_compound(index, d, path),
        "equals" => values::parse_equals(d, path),
        "term" | "search" => parse_term(index, d, path),
        "knnBeta" => knn::parse(index, d, path),
        "span" => span::parse(d, path),
        "queryString" => query_string::parse(index, d, path),
        "moreLikeThis" => more_like::parse(index, d, path),
        "in" => values::parse_in(d, path),
        "range" => values::parse_range(d, path),
        "near" => values::parse_near(d, path),
        other => {
            Err(SearchError(format!("\"{other}\" is not supported by this node's search engine")))
        }
    }
}

fn parse_text(index: &MemIndex, d: &Document, path: &str) -> Result<Operator, SearchError> {
    if let Some((k, _)) = d.iter().find(|(k, _)| {
        !["query", "path", "score", "synonyms", "fuzzy", "matchCriteria"].contains(k)
    }) {
        return Err(SearchError(format!("\"{path}\" unrecognized field \"{k}\"")));
    }
    let at = |f: &str| format!("{path}.{f}");
    let query = queries(d, &at("query"))?;
    let paths = match d.get("path") {
        None => return Err(shape(&at("path"), "is required")),
        Some(p) => paths(index, p, &at("path"))?,
    };
    let fuzzy = parse_fuzzy(d.get("fuzzy"), &at("fuzzy"))?;
    Ok(Operator::Text { query, paths, fuzzy, score: parse_score(d.get("score"), &at("score"))? })
}

/// The `fuzzy` option of `text` and `term`: RefLib's defaults of `FuzzyQuery` as `refsearch` uses
/// them (two edits, no prefix, 50 expansions).
fn parse_fuzzy(v: Option<&Value>, at: &str) -> Result<Option<Fuzzy>, SearchError> {
    let Some(v) = v else { return Ok(None) };
    let Value::Document(d) = v else { return Err(shape(at, "must be a document")) };
    let int = |k: &str, default: f32| match d.get(k) {
        None => Ok(default),
        Some(v) => number(v).ok_or_else(|| shape(&format!("{at}.{k}"), "must be a number")),
    };
    let max_edits = int("maxEdits", 2.0)?;
    if !(1.0..=2.0).contains(&max_edits) {
        return Err(shape(&format!("{at}.maxEdits"), "must be within bounds [1..2]"));
    }
    Ok(Some(Fuzzy {
        max_edits: max_edits as u8,
        prefix_length: int("prefixLength", 0.0)?.max(0.0) as usize,
        max_expansions: int("maxExpansions", 50.0)?.max(0.0) as usize,
    }))
}

/// `term` (deprecated): `text` without synonyms, or a pattern with `prefix`, `wildcard` or
/// `regex` (measured: a constant score of 1, as `wildcard` and `regex` score).
fn parse_term(index: &MemIndex, d: &Document, path: &str) -> Result<Operator, SearchError> {
    const OPTIONS: [&str; 7] = ["query", "path", "prefix", "wildcard", "regex", "fuzzy", "score"];
    if let Some((k, _)) = d.iter().find(|(k, _)| !OPTIONS.contains(k)) {
        return Err(SearchError(format!("\"{path}\" unrecognized field \"{k}\"")));
    }
    let on = |k: &str| matches!(d.get(k), Some(Value::Boolean(true)));
    let kind = if on("regex") {
        "regex"
    } else if on("wildcard") || on("prefix") {
        "wildcard"
    } else {
        let mut text = Document::new();
        for (k, v) in d.iter().filter(|(k, _)| !["prefix", "wildcard", "regex"].contains(k)) {
            text.push(k, v.clone());
        }
        return parse_text(index, &text, path);
    };
    let escape = |q: &str| -> String {
        let mut out: String = q
            .chars()
            .flat_map(|c| if matches!(c, '*' | '?' | '\\') { vec!['\\', c] } else { vec![c] })
            .collect();
        out.push('*');
        out
    };
    let prefix = kind == "wildcard" && !on("wildcard");
    let query = match d.get("query") {
        Some(Value::String(q)) if prefix => Value::from(escape(q)),
        Some(Value::Array(items)) if prefix => Value::Array(
            items
                .iter()
                .map(|i| match i {
                    Value::String(q) => Value::from(escape(q)),
                    other => other.clone(),
                })
                .collect(),
        ),
        Some(q) => q.clone(),
        None => return Err(shape(&format!("{path}.query"), "is required")),
    };
    let mut p = Document::new().with("query", query).with("allowAnalyzedField", true);
    for k in ["path", "score"] {
        if let Some(v) = d.get(k) {
            p.push(k, v.clone());
        }
    }
    pattern::parse(index, kind, &p, path)
}

/// The `query` option of `text` and `phrase`: a string or an array of strings.
fn queries(d: &Document, at: &str) -> Result<Vec<String>, SearchError> {
    match d.get("query") {
        None => Err(shape(at, "is required")),
        Some(Value::String(s)) => Ok(vec![s.clone()]),
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| match i {
                Value::String(s) => Ok(s.clone()),
                _ => Err(shape(at, "must be a string")),
            })
            .collect(),
        Some(_) => Err(shape(at, "must be a string")),
    }
}

fn parse_exists(d: &Document, path: &str) -> Result<Operator, SearchError> {
    if let Some((k, _)) = d.iter().find(|(k, _)| *k != "path" && *k != "score") {
        return Err(SearchError(format!("\"{path}\" unrecognized field \"{k}\"")));
    }
    let at = format!("{path}.path");
    match d.get("path") {
        None => Err(shape(&at, "is required")),
        Some(Value::String(p)) => Ok(Operator::Exists {
            path: p.clone(),
            score: parse_score(d.get("score"), &format!("{path}.score"))?,
        }),
        Some(_) => Err(shape(&at, "must be a string")),
    }
}

fn parse_phrase(index: &MemIndex, d: &Document, path: &str) -> Result<Operator, SearchError> {
    const OPTIONS: [&str; 5] = ["query", "path", "slop", "score", "synonyms"];
    if let Some((k, _)) = d.iter().find(|(k, _)| !OPTIONS.contains(k)) {
        return Err(SearchError(format!("\"{path}\" unrecognized field \"{k}\"")));
    }
    let at = |f: &str| format!("{path}.{f}");
    let query = queries(d, &at("query"))?;
    let paths = match d.get("path") {
        None => return Err(shape(&at("path"), "is required")),
        Some(p) => paths(index, p, &at("path"))?,
    };
    let slop = match d.get("slop") {
        None => 0,
        Some(v) => match number(v) {
            Some(n) if n < 0.0 => return Err(shape(&at("slop"), "cannot be negative")),
            Some(n) => n as u32,
            None => return Err(shape(&at("slop"), "must be a number")),
        },
    };
    Ok(Operator::Phrase { query, paths, slop, score: parse_score(d.get("score"), &at("score"))? })
}

/// The indexed paths a `path` option names: a string, `{wildcard}` or an array of them.
fn paths(index: &MemIndex, v: &Value, at: &str) -> Result<Vec<String>, SearchError> {
    match v {
        Value::String(s) => Ok(vec![s.clone()]),
        Value::Document(d) => match d.get("wildcard") {
            Some(Value::String(w)) => Ok(index.paths(w)),
            _ => Err(shape(at, "must be a string, document or array")),
        },
        Value::Array(items) => {
            let mut out = Vec::new();
            for i in items {
                for p in paths(index, i, at)? {
                    if !out.contains(&p) {
                        out.push(p);
                    }
                }
            }
            Ok(out)
        }
        _ => Err(shape(at, "must be a string, document or array")),
    }
}

fn number(v: &Value) -> Option<f32> {
    match v {
        Value::Int32(n) => Some(*n as f32),
        Value::Int64(n) => Some(*n as f32),
        Value::Double(f) => Some(*f as f32),
        _ => None,
    }
}

fn parse_score(v: Option<&Value>, at: &str) -> Result<Score, SearchError> {
    let Some(v) = v else { return Ok(Score::Plain) };
    let Value::Document(d) = v else { return Err(shape(at, "must be a document")) };
    let kinds: Vec<&str> = d
        .iter()
        .map(|(k, _)| k)
        .filter(|k| ["boost", "constant", "function"].contains(k))
        .collect();
    if kinds.len() > 1 {
        return Err(shape(at, "only one of [boost, constant, function] may be present"));
    }
    let value = |k: &str| -> Result<Option<f32>, SearchError> {
        match d.get(k) {
            Some(Value::Document(o)) => match o.get("value") {
                Some(n) => number(n)
                    .map(Some)
                    .ok_or_else(|| shape(&format!("{at}.{k}.value"), "must be a number")),
                None => Ok(None),
            },
            Some(_) => Err(shape(&format!("{at}.{k}"), "must be a document")),
            None => Ok(None),
        }
    };
    match kinds.first() {
        Some(&"function") => {
            let f = d.get("function").expect("one of the keys");
            Ok(Score::Function(function::parse(f, &format!("{at}.function"))?))
        }
        Some(&"constant") => Ok(Score::Constant(value("constant")?.unwrap_or(1.0))),
        Some(&"boost") => match value("boost")? {
            Some(b) => Ok(Score::Boost(b)),
            None => match d.get("boost") {
                Some(Value::Document(o)) => match o.get("path") {
                    Some(Value::String(p)) => Ok(Score::BoostPath(p.clone())),
                    _ => Err(shape(&format!("{at}.boost"), "must have one of [path, value]")),
                },
                _ => Err(shape(&format!("{at}.boost"), "must be a document")),
            },
        },
        _ => Ok(Score::Plain),
    }
}

fn clauses(
    index: &MemIndex,
    d: &Document,
    field: &str,
    path: &str,
) -> Result<Vec<Operator>, SearchError> {
    let at = format!("{path}.{field}");
    let one = |v: &Value, at: &str| -> Result<Operator, SearchError> {
        let Value::Document(o) = v else { return Err(shape(at, "must be a document")) };
        parse_root(index, o)
    };
    match d.get(field) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items.iter().map(|i| one(i, &at)).collect(),
        Some(v @ Value::Document(_)) => Ok(vec![one(v, &at)?]),
        Some(_) => Err(shape(&at, "must be a document")),
    }
}

fn parse_compound(index: &MemIndex, d: &Document, path: &str) -> Result<Operator, SearchError> {
    const CLAUSES: [&str; 4] = ["filter", "must", "mustNot", "should"];
    if let Some((k, _)) =
        d.iter().find(|(k, _)| !CLAUSES.contains(k) && *k != "minimumShouldMatch" && *k != "score")
    {
        return Err(SearchError(format!("\"{path}\" unrecognized field \"{k}\"")));
    }
    if !CLAUSES.iter().any(|c| d.get(c).is_some()) {
        return Err(shape(path, "at least one of [filter, must, mustNot, should] must be present"));
    }
    let should = clauses(index, d, "should", path)?;
    let minimum = match d.get("minimumShouldMatch") {
        None => 0,
        Some(v) => number(v).map_or(0, |n| n.max(0.0) as usize),
    };
    if minimum > should.len() {
        return Err(shape(
            path,
            "minimumShouldMatch cannot be greater than number of should clauses",
        ));
    }
    Ok(Operator::Compound {
        must: clauses(index, d, "must", path)?,
        should,
        filter: clauses(index, d, "filter", path)?,
        must_not: clauses(index, d, "mustNot", path)?,
        minimum,
    })
}

/// The value at dotted `path` of `doc`.
pub(crate) fn value_at<'a>(doc: &'a Document, path: &str) -> Option<&'a Value> {
    let mut parts = path.split('.');
    let mut cur = doc.get(parts.next()?)?;
    for part in parts {
        cur = match cur {
            Value::Document(d) => d.get(part)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// The number at `path` of `doc`, 0 without one.
fn number_at(doc: Option<&Document>, path: &str) -> f32 {
    doc.and_then(|d| value_at(d, path)).and_then(number).unwrap_or(0.0)
}

fn eval(index: &MemIndex, op: &Operator) -> Result<BTreeMap<u32, f32>, SearchError> {
    match op {
        Operator::Text { query, paths, fuzzy, score } => {
            let s = match fuzzy {
                Some(f) => index.fuzzy_scores(query, paths, f, boost_of(score))?,
                None => index.text_scores(query, paths, boost_of(score))?,
            };
            rescored(index, s, score)
        }
        Operator::Exists { path, score } => {
            let found = index
                .live()
                .filter(|&doc| index.document(doc).is_some_and(|d| value_at(d, path).is_some()))
                .map(|doc| (doc, boost_of(score)))
                .collect();
            rescored(index, found, score)
        }
        Operator::Phrase { query, paths, slop, score } => {
            let s = index.phrase_scores(query, paths, *slop, boost_of(score))?;
            rescored(index, s, score)
        }
        Operator::Pattern(p) => {
            let s = p.scores(index, boost_of(p.score()))?;
            rescored(index, s, p.score())
        }
        Operator::Empty => Ok(BTreeMap::new()),
        Operator::Scored(op, score) => {
            let s = eval(index, op)?;
            rescored(index, s, score)
        }
        Operator::TermRange { path, lower, upper, score } => {
            let inside = |t: &str| {
                lower.as_ref().is_none_or(|(l, incl)| t > l.as_str() || (*incl && t == l))
                    && upper.as_ref().is_none_or(|(u, incl)| t < u.as_str() || (*incl && t == u))
            };
            let boost = boost_of(score);
            let s = index.matching_terms(path, inside).into_iter().map(|d| (d, boost)).collect();
            rescored(index, s, score)
        }
        Operator::Span(sp, score) => {
            let s = sp.scores(index, boost_of(score))?;
            rescored(index, s, score)
        }
        Operator::Knn(k) => {
            let s = k.scores(index, boost_of(&k.score))?;
            rescored(index, s, &k.score)
        }
        Operator::Values(v) => {
            let s = values::scores(index, v, boost_of(v.score()));
            rescored(index, s, v.score())
        }
        Operator::Compound { must, should, filter, must_not, minimum } => {
            eval_compound(index, must, should, filter, must_not, *minimum)
        }
    }
}

/// The factor `score` puts in a term's weight.
fn boost_of(score: &Score) -> f32 {
    if let Score::Boost(b) = score { *b } else { 1.0 }
}

/// The scores after `score`'s constant, field boost or function.
fn rescored(
    index: &MemIndex,
    mut s: BTreeMap<u32, f32>,
    score: &Score,
) -> Result<BTreeMap<u32, f32>, SearchError> {
    match score {
        Score::Constant(c) => s.values_mut().for_each(|v| *v = *c),
        Score::BoostPath(p) => {
            for (doc, v) in s.iter_mut() {
                *v *= number_at(index.document(*doc), p);
            }
        }
        Score::Function(f) => {
            f.check(index.definition())?;
            for (doc, v) in s.iter_mut() {
                *v = f.score(*v, index.document(*doc));
            }
        }
        Score::Plain | Score::Boost(_) => {}
    }
    Ok(s)
}

fn eval_compound(
    index: &MemIndex,
    must: &[Operator],
    should: &[Operator],
    filter: &[Operator],
    must_not: &[Operator],
    minimum: usize,
) -> Result<BTreeMap<u32, f32>, SearchError> {
    let must: Vec<_> = must.iter().map(|o| eval(index, o)).collect::<Result<_, _>>()?;
    let should: Vec<_> = should.iter().map(|o| eval(index, o)).collect::<Result<_, _>>()?;
    let filter: Vec<_> = filter.iter().map(|o| eval(index, o)).collect::<Result<_, _>>()?;
    let excluded: Vec<_> = must_not.iter().map(|o| eval(index, o)).collect::<Result<_, _>>()?;
    // Without a clause that selects, `should` alone selects, and `mustNot` alone keeps the rest.
    let required =
        minimum.max(usize::from(must.is_empty() && filter.is_empty() && !should.is_empty()));
    let mut out = BTreeMap::new();
    for doc in index.live() {
        if excluded.iter().any(|e| e.contains_key(&doc)) {
            continue;
        }
        if !must.iter().chain(filter.iter()).all(|m| m.contains_key(&doc)) {
            continue;
        }
        let matched = should.iter().filter(|s| s.contains_key(&doc)).count();
        if matched < required {
            continue;
        }
        let mut score = 0.0f32;
        for clause in must.iter().chain(should.iter()) {
            if let Some(s) = clause.get(&doc) {
                score += s;
            }
        }
        out.insert(doc, score);
    }
    Ok(out)
}

mod function;
mod knn;
mod more_like;
mod pattern;
mod query_string;
mod span;
mod values;

#[cfg(test)]
mod pattern_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod values_tests;
