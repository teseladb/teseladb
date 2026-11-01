//! The `$search` specification over an index (011 FR-002, T022): its operator, checked with the
//! errors `refsearch` 1.70.6 gives (measured on a 9.0.4 replica set, 01-11-2026), and the scored
//! documents it selects. `compound` adds the scores of its `must` and `should` clauses in 32-bit
//! floats, `filter` matches without scoring, `mustNot` excludes, and a `compound` with only
//! `mustNot` returns every other document at score 0.

use std::collections::BTreeMap;

use teseladb_bson::{Document, Value};

use crate::index::{Hit, IndexError, MemIndex};

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
            IndexError::Analysis => "analysis ran out of steps".to_owned(),
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
}

/// An operator.
#[derive(Clone, Debug, PartialEq)]
enum Operator {
    Text {
        query: Vec<String>,
        paths: Vec<String>,
        score: Score,
    },
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
        "compound" => parse_compound(index, d, path),
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
    let query = match d.get("query") {
        None => return Err(shape(&at("query"), "is required")),
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| match i {
                Value::String(s) => Ok(s.clone()),
                _ => Err(shape(&at("query"), "must be a string")),
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(shape(&at("query"), "must be a string")),
    };
    let paths = match d.get("path") {
        None => return Err(shape(&at("path"), "is required")),
        Some(p) => paths(index, p, &at("path"))?,
    };
    Ok(Operator::Text { query, paths, score: parse_score(d.get("score"), &at("score"))? })
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

/// The number at `path` of `doc`, 0 without one.
fn number_at(doc: Option<&Document>, path: &str) -> f32 {
    let mut cur = doc.map(|d| Value::Document(d.clone()));
    for part in path.split('.') {
        cur = match cur {
            Some(Value::Document(d)) => d.get(part).cloned(),
            _ => None,
        };
    }
    cur.as_ref().and_then(number).unwrap_or(0.0)
}

fn eval(index: &MemIndex, op: &Operator) -> Result<BTreeMap<u32, f32>, SearchError> {
    match op {
        Operator::Text { query, paths, score } => {
            let boost = if let Score::Boost(b) = score { *b } else { 1.0 };
            let mut s = index.text_scores(query, paths, boost)?;
            match score {
                Score::Constant(c) => s.values_mut().for_each(|v| *v = *c),
                Score::BoostPath(p) => {
                    for (doc, v) in s.iter_mut() {
                        *v *= number_at(index.document(*doc), p);
                    }
                }
                Score::Plain | Score::Boost(_) => {}
            }
            Ok(s)
        }
        Operator::Compound { must, should, filter, must_not, minimum } => {
            eval_compound(index, must, should, filter, must_not, *minimum)
        }
    }
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

#[cfg(test)]
mod tests;
