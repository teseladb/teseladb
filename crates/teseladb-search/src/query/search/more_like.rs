//! The `moreLikeThis` operator (011 FR-002) as the reference search server 1.70.6 runs it (measured 02-11-2026,
//! `scratchpad/lane3j/m3.js`..`m5.js`): the strings of the `like` documents on indexed text
//! fields give their distinct terms, those found in at least two documents of the field, up to
//! 25 of them by `tf · idf`, and each term is an optional clause scored as `text` scores it.

use teseladb_bson::{Document, Value};

use super::{Operator, Score, SearchError, parse_score, shape};
use crate::index::MemIndex;

/// Fewest documents a term must be in to be asked for.
const MIN_DOC_FREQ: usize = 2;
/// Most terms asked for.
const MAX_QUERY_TERMS: usize = 25;

/// The strings of `doc` by dotted path.
fn strings(doc: &Document, prefix: &str, out: &mut Vec<(String, String)>) {
    for (name, v) in doc.iter() {
        let path = if prefix.is_empty() { name.to_owned() } else { format!("{prefix}.{name}") };
        value_strings(v, &path, out);
    }
}

fn value_strings(v: &Value, path: &str, out: &mut Vec<(String, String)>) {
    match v {
        Value::String(s) => out.push((path.to_owned(), s.clone())),
        Value::Array(items) => items.iter().for_each(|i| value_strings(i, path, out)),
        Value::Document(d) => strings(d, path, out),
        _ => {}
    }
}

/// `moreLikeThis` with its options `d` at `path` of the query.
pub(super) fn parse(index: &MemIndex, d: &Document, path: &str) -> Result<Operator, SearchError> {
    if let Some((k, _)) = d.iter().find(|(k, _)| *k != "like" && *k != "score") {
        return Err(SearchError(format!("\"{path}\" unrecognized field \"{k}\"")));
    }
    let at = format!("{path}.like");
    let likes: Vec<&Document> = match d.get("like") {
        None => return Err(shape(&at, "is required")),
        Some(Value::Document(l)) => vec![l],
        Some(Value::Array(items)) if items.is_empty() => return Err(shape(&at, "cannot be empty")),
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| match i {
                Value::Document(l) => Ok(l),
                _ => Err(shape(&at, "must be a document")),
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(shape(&at, "must be a document")),
    };
    let score = parse_score(d.get("score"), &format!("{path}.score"))?;
    let mut texts = Vec::new();
    for l in likes {
        strings(l, "", &mut texts);
    }
    // Each distinct term of each field, with its frequency in the `like` documents.
    let mut terms: Vec<(String, String, usize)> = Vec::new();
    for (field, text) in texts {
        if !index.has_text(&field) {
            continue;
        }
        for t in index.query_terms(&field, &text)? {
            match terms.iter_mut().find(|(f, term, _)| *f == field && *term == t) {
                Some(found) => found.2 += 1,
                None => terms.push((field.clone(), t, 1)),
            }
        }
    }
    let docs = index.len().max(1) as f64;
    let mut kept: Vec<(f64, String, String)> = terms
        .into_iter()
        .filter_map(|(field, term, tf)| {
            let df = index.positions(&field, &term).map_or(0, |d| d.len());
            let idf = (docs / (df as f64 + 1.0)).ln() + 1.0;
            (df >= MIN_DOC_FREQ).then_some((tf as f64 * idf, field, term))
        })
        .collect();
    kept.sort_by(|a, b| b.0.total_cmp(&a.0));
    let (boost, rest) = match score {
        Score::Boost(b) => (b, Score::Plain),
        other => (1.0, other),
    };
    // The reference search library's `MoreLikeThis` adds the kept terms from its queue, the lowest score first
    // (measured with `scoreDetails`).
    let kept: Vec<(f64, String, String)> =
        kept.into_iter().take(MAX_QUERY_TERMS).collect::<Vec<_>>().into_iter().rev().collect();
    let should: Vec<Operator> = kept
        .into_iter()
        .map(|(_, field, term)| Operator::Text {
            query: vec![term],
            paths: vec![field],
            fuzzy: None,
            score: Score::Boost(boost),
        })
        .collect();
    if should.is_empty() {
        return Ok(Operator::Empty);
    }
    let op = Operator::Compound {
        must: Vec::new(),
        should,
        filter: Vec::new(),
        must_not: Vec::new(),
        minimum: 0,
        weighted: false,
    };
    Ok(match rest {
        Score::Plain => op,
        other => Operator::Scored(Box::new(op), other),
    })
}
