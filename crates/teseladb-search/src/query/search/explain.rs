//! `scoreDetails` (011 FR-003, FR-016): the explanation of each result's score, as `refsearch`
//! 1.70.6 gives it (measured 02-11-2026, `scratchpad/lane3k/m15.js`) for `text` on single
//! terms (with `boost` and `constant`), `exists`, `wildcard`, `regex` and `compound`. The
//! other operators give their score with the operator's name and no parts (DV-083).

use std::collections::BTreeMap;

use super::{Operator, Score, SearchError, eval};
use crate::index::{Explanation, MemIndex};

/// The name `refsearch` gives the field at `path` of type `string` in its queries.
fn string_field(path: &str) -> String {
    format!("$type:string/{path}")
}

/// What RefLib calls query `op` in an explanation, when it is one of those measured.
fn query_name(index: &MemIndex, op: &Operator) -> Option<String> {
    match op {
        Operator::Text { query, paths, fuzzy: None, .. } => {
            let terms: Vec<String> = paths
                .iter()
                .flat_map(|p| {
                    query.iter().flat_map(move |q| {
                        index
                            .single_terms(p, q)
                            .into_iter()
                            .map(move |t| format!("{}:{t}", string_field(p)))
                    })
                })
                .collect();
            match terms.as_slice() {
                [one] => Some(one.clone()),
                _ => Some(format!("({})", terms.join(" "))),
            }
        }
        Operator::Exists { path, .. } => Some(format!("$meta/fieldNames:{path}")),
        _ => None,
    }
}

/// The name of an operator for the explanations of the operators not measured.
fn operator_name(op: &Operator) -> &'static str {
    match op {
        Operator::Text { .. } => "text",
        Operator::Phrase { .. } => "phrase",
        Operator::Exists { .. } => "exists",
        Operator::Pattern(_) => "pattern",
        Operator::Values(_) => "values",
        Operator::Knn(_) => "knnBeta",
        Operator::TermRange { .. } => "termRange",
        Operator::Empty => "empty",
        Operator::Scored(..) => "scored",
        Operator::Embedded(_) => "embeddedDocument",
        Operator::HasRoot(_) => "hasRoot",
        Operator::HasAncestor { .. } => "hasAncestor",
        Operator::Precomputed(_) => "precomputed",
        Operator::Geo(_) => "geo",
        Operator::Autocomplete(_) => "autocomplete",
        Operator::Span(..) => "span",
        Operator::Compound { .. } => "compound",
    }
}

/// The explanations of `op`'s scores for `docs`.
pub(super) fn explain_all(
    index: &MemIndex,
    op: &Operator,
    docs: &[u32],
) -> Result<BTreeMap<u32, Explanation>, SearchError> {
    let scores = eval(index, op)?;
    let mut out = BTreeMap::new();
    for &doc in docs {
        let Some(&score) = scores.get(&doc) else { continue };
        out.insert(doc, explain_one(index, op, doc, score)?);
    }
    Ok(out)
}

fn explain_one(
    index: &MemIndex,
    op: &Operator,
    doc: u32,
    score: f32,
) -> Result<Explanation, SearchError> {
    Ok(match op {
        Operator::Text { query, paths, fuzzy: None, score: how } => match how {
            Score::Constant(_) => constant(index, op, score),
            Score::Plain | Score::Boost(_) => {
                let boost = if let Score::Boost(b) = how { *b } else { 1.0 };
                let mut parts = Vec::new();
                for p in paths {
                    for q in query {
                        for t in index.single_terms(p, q) {
                            parts.extend(index.explain_term(p, &string_field(p), &t, boost, doc));
                        }
                    }
                }
                match parts.len() {
                    1 => parts.remove(0),
                    _ => Explanation::node(score, "sum of:", parts),
                }
            }
            _ => Explanation::leaf(score, operator_name(op)),
        },
        Operator::Exists { .. } => constant(index, op, score),
        Operator::Pattern(p) => Explanation::leaf(score, p.description()),
        Operator::Compound { must, should, filter, .. } => {
            let mut parts = Vec::new();
            for f in filter {
                let name = query_name(index, f).unwrap_or_else(|| operator_name(f).to_owned());
                parts.push(Explanation::node(
                    0.0,
                    "match on required clause, product of:",
                    vec![Explanation::leaf(0.0, "# clause"), Explanation::leaf(1.0, name)],
                ));
            }
            for o in must.iter().chain(should) {
                parts.extend(explain_all(index, o, &[doc])?.remove(&doc));
            }
            Explanation::node(score, "sum of:", parts)
        }
        other => Explanation::leaf(score, operator_name(other)),
    })
}

/// The explanation of a constant score.
fn constant(index: &MemIndex, op: &Operator, score: f32) -> Explanation {
    let name = query_name(index, op).unwrap_or_else(|| operator_name(op).to_owned());
    Explanation::leaf(score, format!("ConstantScore({name}) [BM25Similarity], result of:"))
}
