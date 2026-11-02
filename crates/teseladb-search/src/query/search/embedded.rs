//! The `embeddedDocument` operator (011 FR-002) over an `embeddedDocuments` field, as `refsearch`
//! 1.70.6 runs it (measured 02-11-2026, `scratchpad/lane3j/m3.js`, `m4.js`): each element of
//! the array is a document of its own, with statistics among the elements alone, its operator
//! runs over them, and a document scores the `sum` (default), `maximum`, `minimum` or `mean` of
//! its elements' scores, changed then by `score.embedded.outerScore` or by `score` itself.

use std::collections::BTreeMap;

use teseladb_bson::{Document, Value};

use super::{Operator, Score, SearchError, eval, parse_root, parse_score, shape, value_at};
use crate::definition::{Dynamic, FieldMapping, Mapping, StoredSource};
use crate::index::MemIndex;

/// How the scores of a document's elements make its score.
const AGGREGATES: [&str; 4] = ["maximum", "mean", "minimum", "sum"];

/// An `embeddedDocument` operator.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Embedded {
    path: String,
    operator: Document,
    aggregate: String,
    pub(super) score: Score,
}

/// The paths an operator names, at any depth.
fn named_paths(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Document(d) => {
            for (k, v) in d.iter() {
                match (k, v) {
                    ("path", Value::String(p)) => out.push(p.clone()),
                    ("path", Value::Array(items)) => items.iter().for_each(|i| match i {
                        Value::String(p) => out.push(p.clone()),
                        other => named_paths(other, out),
                    }),
                    ("path", Value::Document(p)) => {
                        if let Some(Value::String(p)) = p.get("value") {
                            out.push(p.clone());
                        }
                    }
                    // `hasRoot` names the paths of the documents that hold the elements.
                    ("hasRoot", _) => {}
                    _ => named_paths(v, out),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|i| named_paths(i, out)),
        _ => {}
    }
}

/// `embeddedDocument` with its options `d` at `path` of the query.
pub(super) fn parse(d: &Document, path: &str) -> Result<Operator, SearchError> {
    const OPTIONS: [&str; 3] = ["path", "operator", "score"];
    if let Some((k, _)) = d.iter().find(|(k, _)| !OPTIONS.contains(k)) {
        return Err(SearchError(format!("\"{path}\" unrecognized field \"{k}\"")));
    }
    let at = |f: &str| format!("{path}.{f}");
    let field = match d.get("path") {
        None => return Err(shape(&at("path"), "is required")),
        Some(Value::String(p)) => p.clone(),
        Some(_) => return Err(shape(&at("path"), "must be a string")),
    };
    let operator = match d.get("operator") {
        None => return Err(shape(&at("operator"), "is required")),
        Some(Value::Document(o)) => o.clone(),
        Some(_) => return Err(shape(&at("operator"), "must be a document")),
    };
    let mut named = Vec::new();
    named_paths(&Value::Document(operator.clone()), &mut named);
    let prefix = format!("{field}.");
    if let Some(p) = named.iter().find(|p| !p.starts_with(&prefix)) {
        return Err(shape(path, &format!("{p} is not a subfield of {field}")));
    }
    let (aggregate, score) = match d.get("score") {
        Some(Value::Document(s)) if s.get("embedded").is_some() => {
            let e_at = format!("{}.embedded", at("score"));
            let Some(Value::Document(e)) = s.get("embedded") else {
                return Err(shape(&e_at, "must be a document"));
            };
            let aggregate = match e.get("aggregate") {
                None => "sum".to_owned(),
                Some(Value::String(a)) if AGGREGATES.contains(&a.as_str()) => a.clone(),
                Some(_) => {
                    return Err(shape(
                        &format!("{e_at}.aggregate"),
                        &format!("must be one of [{}]", AGGREGATES.join(", ")),
                    ));
                }
            };
            (aggregate, parse_score(e.get("outerScore"), &format!("{e_at}.outerScore"))?)
        }
        other => ("sum".to_owned(), parse_score(other, &at("score"))?),
    };
    Ok(Operator::Embedded(Embedded { path: field, operator, aggregate, score }))
}

/// The mapping of an index whose documents are the elements of `path`, mapped by `inner`.
fn element_mapping(path: &str, inner: Mapping) -> Mapping {
    let mut parts: Vec<&str> = path.split('.').collect();
    let last = parts.pop().unwrap_or_default();
    let wrap = |name: &str, m: Mapping| Mapping {
        dynamic: Dynamic::Off,
        fields: vec![(name.to_owned(), vec![FieldMapping::Document(m)])],
        stored_source: StoredSource::None,
    };
    parts.into_iter().rev().fold(wrap(last, inner), |m, name| wrap(name, m))
}

/// `op` with each `hasRoot` replaced by the scores of its operator on the documents of `root`,
/// given to their elements in `elements` plus 1 (measured with `scoreDetails`: the root's
/// score and a constant 1 for being an element of the path).
fn resolve_roots(
    op: Operator,
    root: &MemIndex,
    elements: &MemIndex,
) -> Result<Operator, SearchError> {
    let all = |ops: Vec<Operator>| {
        ops.into_iter().map(|o| resolve_roots(o, root, elements)).collect::<Result<Vec<_>, _>>()
    };
    Ok(match op {
        Operator::HasRoot(inner) => {
            let scores = eval(root, &parse_root(root, &inner)?)?;
            let by_element = elements
                .live()
                .filter_map(|id| {
                    let key = elements.key_of(id)?;
                    let parent = u32::from_be_bytes([key[0], key[1], key[2], key[3]]);
                    Some((id, scores.get(&parent)? + 1.0))
                })
                .collect();
            Operator::Precomputed(by_element)
        }
        Operator::Compound { must, should, filter, must_not, minimum } => Operator::Compound {
            must: all(must)?,
            should: all(should)?,
            filter: all(filter)?,
            must_not: all(must_not)?,
            minimum,
        },
        other => other,
    })
}

/// `doc` with `value` at dotted `path`.
fn nested(path: &str, value: Value) -> Document {
    let mut parts: Vec<&str> = path.split('.').collect();
    let last = parts.pop().unwrap_or_default();
    let leaf = Document::new().with(last, value);
    parts.into_iter().rev().fold(leaf, |d, name| Document::new().with(name, d))
}

impl Embedded {
    /// The scores of the documents of `index` whose elements the operator matches.
    pub(super) fn scores(
        &self,
        index: &MemIndex,
        boost: f32,
    ) -> Result<BTreeMap<u32, f32>, SearchError> {
        let types = super::values::types_at(index.definition(), &self.path);
        let Some(inner) = types.into_iter().find_map(|t| match t {
            FieldMapping::EmbeddedDocuments(m) => Some(m),
            _ => None,
        }) else {
            return Err(SearchError(format!(
                "embeddedDocument requires path '{}' to be indexed as 'embeddedDocuments'",
                self.path
            )));
        };
        let mut definition = index.definition().clone();
        definition.mappings = element_mapping(&self.path, inner);
        let mut elements = MemIndex::new(definition);
        for doc in index.live() {
            let Some(Value::Array(items)) =
                index.document(doc).and_then(|d| value_at(d, &self.path))
            else {
                continue;
            };
            for (i, item) in items.iter().enumerate() {
                let Value::Document(_) = item else { continue };
                let mut key = doc.to_be_bytes().to_vec();
                key.extend_from_slice(&(i as u32).to_be_bytes());
                elements.put(&key, &nested(&self.path, item.clone()))?;
            }
        }
        // A boost weighs the terms of the elements' operator, as RefLib's `BoostQuery` does.
        let op = resolve_roots(parse_root(&elements, &self.operator)?, index, &elements)?;
        let op = super::boosted(op, boost);
        let mut per_doc: BTreeMap<u32, Vec<f32>> = BTreeMap::new();
        let mut hits = elements.hits(eval(&elements, &op)?);
        // The elements in their order, as the scores of a document's elements are added.
        hits.sort_by(|a, b| a.key.cmp(&b.key));
        for hit in hits {
            let parent = u32::from_be_bytes([hit.key[0], hit.key[1], hit.key[2], hit.key[3]]);
            per_doc.entry(parent).or_default().push(hit.score);
        }
        Ok(per_doc
            .into_iter()
            .map(|(doc, s)| {
                let v = match self.aggregate.as_str() {
                    "maximum" => s.iter().copied().fold(f32::MIN, f32::max),
                    "minimum" => s.iter().copied().fold(f32::MAX, f32::min),
                    "mean" => s.iter().sum::<f32>() / s.len() as f32,
                    _ => s.iter().sum(),
                };
                (doc, v)
            })
            .collect())
    }
}
