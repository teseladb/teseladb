//! The `embeddedDocument` operator (011 FR-002) over an `embeddedDocuments` field, as `refsearch`
//! 1.70.6 runs it (measured 02-11-2026, `scratchpad/lane3j/m3.js`, `m4.js`, and nested in
//! `scratchpad/lane3k/m10.js`, `m11.js`): each element of the array is a document of its own,
//! with statistics among the elements of its path alone, its operator runs over them, and a
//! document scores the `sum` (default), `maximum`, `minimum` or `mean` of its elements' scores,
//! changed then by `score.embedded.outerScore` or by `score` itself. Each `embeddedDocuments`
//! path above the operator's is a level of its own, which `hasAncestor` reaches; `hasRoot`
//! reaches the documents. Both give an element the score of its operator on the level plus 1.

use std::collections::BTreeMap;

use teseladb_bson::{Document, Value};

use super::{
    OPERATORS, Operator, Score, SearchError, boosted, eval, parse_at, parse_root, parse_score,
    shape, value_at,
};
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
    /// Where the operator is in the query, for the messages of the operators inside.
    at: String,
    pub(super) score: Score,
}

/// The paths an operator names, at any depth, each with the operator that names it (`op` for
/// those of `v` itself); those under `hasRoot` and `hasAncestor` belong to other levels.
fn named_paths(v: &Value, op: &str, out: &mut Vec<(String, String)>) {
    match v {
        Value::Document(d) => {
            for (k, v) in d.iter() {
                match (k, v) {
                    ("path", Value::String(p)) => out.push((op.to_owned(), p.clone())),
                    ("path", Value::Array(items)) => items.iter().for_each(|i| match i {
                        Value::String(p) => out.push((op.to_owned(), p.clone())),
                        other => named_paths(other, op, out),
                    }),
                    ("path", Value::Document(p)) => {
                        if let Some(Value::String(p)) = p.get("value") {
                            out.push((op.to_owned(), p.clone()));
                        }
                    }
                    ("hasRoot" | "hasAncestor", _) => {}
                    _ if OPERATORS.contains(&k) => named_paths(v, k, out),
                    _ => named_paths(v, op, out),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|i| named_paths(i, op, out)),
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
    named_paths(&Value::Document(operator.clone()), "", &mut named);
    let prefix = format!("{field}.");
    if let Some((_, p)) = named.iter().find(|(_, p)| !p.starts_with(&prefix)) {
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
    Ok(Operator::Embedded(Embedded {
        path: field,
        operator,
        aggregate,
        at: path.to_owned(),
        score,
    }))
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

/// `doc` with `value` at dotted `path`.
fn nested(path: &str, value: Value) -> Document {
    let mut parts: Vec<&str> = path.split('.').collect();
    let last = parts.pop().unwrap_or_default();
    let leaf = Document::new().with(last, value);
    parts.into_iter().rev().fold(leaf, |d, name| Document::new().with(name, d))
}

/// A level of documents: those of the index (path empty) or the elements of an
/// `embeddedDocuments` path, whose keys start with the number of the document of the level
/// above that holds them.
#[derive(Clone, Copy)]
pub(super) struct Level<'a> {
    pub(super) path: &'a str,
    pub(super) index: &'a MemIndex,
}

/// The scores an element takes from the document of level `level` that holds it (`hasRoot`,
/// `hasAncestor`): that document's score under `parent` plus `one`.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Passed {
    pub(super) scores: BTreeMap<u32, f32>,
    parent: Box<Operator>,
    level: usize,
    one: f32,
}

impl Passed {
    /// The explanation of element `doc`'s score (measured: the holder's explanation under
    /// `Score based on parent document`, then a constant for being an element of the path).
    pub(super) fn explain(
        &self,
        ctx: &super::explain::Ctx<'_>,
        doc: u32,
        score: f32,
    ) -> Result<Option<crate::index::Explanation>, SearchError> {
        use crate::index::Explanation;
        let levels = &ctx.levels;
        if levels.len() <= self.level {
            return Ok(None);
        }
        let last = levels.len() - 1;
        let Some(holder_id) = holder(levels, last, doc, self.level) else { return Ok(None) };
        let up = levels[self.level];
        let parent_score = eval(up.index, &self.parent)?.get(&holder_id).copied().unwrap_or(0.0);
        let prefix = if self.level == 0 {
            String::new()
        } else {
            format!("$embedded:{}/{}/", self.level, up.path)
        };
        let parent_ctx = super::explain::Ctx {
            index: up.index,
            prefix,
            weighted: ctx.weighted,
            levels: levels[..=self.level].to_vec(),
        };
        let inner = super::explain::explain(&parent_ctx, &self.parent, holder_id, parent_score)?;
        let parent = Explanation::node(
            parent_score,
            format!("Score based on parent document {holder_id}"),
            inner.into_iter().collect(),
        );
        let path = Explanation::leaf(
            self.one,
            format!(
                "ConstantScore($meta/embeddedPath:{}) [BM25Similarity], result of:",
                levels[last].path
            ),
        );
        let mut e = super::explain::sum(vec![parent, path]);
        e.value = score;
        Ok(Some(e))
    }
}

/// The number, in level `to`, of the document holding element `id` of level `from`.
fn holder(chain: &[Level<'_>], from: usize, id: u32, to: usize) -> Option<u32> {
    let mut id = id;
    for level in chain[to + 1..=from].iter().rev() {
        let key = level.index.key_of(id)?;
        id = u32::from_be_bytes([key[0], key[1], key[2], key[3]]);
    }
    Some(id)
}

/// The scores of the elements of the last level of `chain`: `scores` of their holders in level
/// `to`, plus `one` (measured with `scoreDetails`: the holder's score and a constant 1 for being
/// an element of the path, both times the boost).
fn passed_down(
    chain: &[Level<'_>],
    to: usize,
    parent: Operator,
    scores: &BTreeMap<u32, f32>,
    one: f32,
) -> Operator {
    let last = chain.len() - 1;
    let by_element = chain[last]
        .index
        .live()
        .filter_map(|id| Some((id, scores.get(&holder(chain, last, id, to)?)? + one)))
        .collect();
    Operator::Passed(Passed { scores: by_element, parent: Box::new(parent), level: to, one })
}

/// `op`, run on the last level of `chain` (named `here` in messages), with `hasRoot`,
/// `hasAncestor` and the `embeddedDocument` operators inside it replaced by their scores.
fn resolve(op: Operator, chain: &[Level<'_>], here: &str) -> Result<Operator, SearchError> {
    let all = |ops: Vec<Operator>| {
        ops.into_iter().map(|o| resolve(o, chain, here)).collect::<Result<Vec<_>, _>>()
    };
    Ok(match op {
        Operator::HasRoot(inner) => {
            let root = chain[0].index;
            let parent = parse_root(root, &inner)?;
            let scores = eval(root, &parent)?;
            passed_down(chain, 0, parent, &scores, 1.0)
        }
        Operator::HasAncestor { path, operator, score, at } => {
            let last = chain.len() - 1;
            let Some(to) = (1..last).find(|&l| chain[l].path == path) else {
                return Err(SearchError(format!(
                    "hasAncestor.ancestorPath '{path}' must be a parent of {here}"
                )));
            };
            let mut named = Vec::new();
            named_paths(&Value::Document(operator.clone()), "", &mut named);
            let prefix = format!("{path}.");
            if let Some((op, p)) = named.iter().find(|(_, p)| !p.starts_with(&prefix)) {
                return Err(SearchError(format!(
                    "{op} operator path '{p}' must be a descendant of hasAncestor.ancestorPath \
                     '{path}'"
                )));
            }
            let level = chain[to].index;
            let inner = parse_at(level, &operator, &format!("{at}.operator"))?;
            let here = format!("hasAncestor.ancestorPath '{path}'");
            let b = super::boost_of(&score);
            let inner = boosted(resolve(inner, &chain[..=to], &here)?, b);
            let scores = eval(level, &inner)?;
            let rest = if b == 1.0 { score } else { Score::Plain };
            Operator::Scored(Box::new(passed_down(chain, to, inner, &scores, b)), rest)
        }
        Operator::Embedded(e) => {
            let s = e.scores_in(chain, super::boost_of(&e.score))?;
            let rest = match e.score {
                Score::Boost(_) => Score::Plain,
                other => other,
            };
            Operator::Scored(Box::new(Operator::Precomputed(s)), rest)
        }
        Operator::Compound { must, should, filter, must_not, minimum, weighted } => {
            Operator::Compound {
                must: all(must)?,
                should: all(should)?,
                filter: all(filter)?,
                must_not: all(must_not)?,
                minimum,
                weighted,
            }
        }
        Operator::Scored(inner, score) => {
            Operator::Scored(Box::new(resolve(*inner, chain, here)?), score)
        }
        other => other,
    })
}

/// The elements of `path` of the documents of `holders`, each a document of an index mapped by
/// `inner`, keyed by its holder's number and its place in the array.
fn elements_of(holders: &MemIndex, path: &str, inner: Mapping) -> Result<MemIndex, SearchError> {
    let mut definition = holders.definition().clone();
    definition.mappings = element_mapping(path, inner);
    let mut elements = MemIndex::new(definition);
    for doc in holders.live() {
        let Some(Value::Array(items)) = holders.document(doc).and_then(|d| value_at(d, path))
        else {
            continue;
        };
        for (i, item) in items.iter().enumerate() {
            let Value::Document(_) = item else { continue };
            let mut key = doc.to_be_bytes().to_vec();
            key.extend_from_slice(&(i as u32).to_be_bytes());
            elements.put(&key, &nested(path, item.clone()))?;
        }
    }
    Ok(elements)
}

/// The mapping of `path` when it is an `embeddedDocuments` field of `index`.
fn embedded_mapping(index: &MemIndex, path: &str) -> Option<Mapping> {
    super::values::types_at(index.definition(), path).into_iter().find_map(|t| match t {
        FieldMapping::EmbeddedDocuments(m) => Some(m),
        _ => None,
    })
}

impl Embedded {
    /// The scores of the documents of `index` whose elements the operator matches.
    pub(super) fn scores(
        &self,
        index: &MemIndex,
        boost: f32,
    ) -> Result<BTreeMap<u32, f32>, SearchError> {
        self.scores_in(&[Level { path: "", index }], boost)
    }

    /// `f` run on the levels from `chain` down to the operator's elements, the number of the
    /// last one and the operator resolved there with weight `boost`.
    fn with_elements<R>(
        &self,
        chain: &[Level<'_>],
        boost: f32,
        f: impl FnOnce(&[Level<'_>], usize, &Operator) -> Result<R, SearchError>,
    ) -> Result<R, SearchError> {
        let top = chain[chain.len() - 1];
        // Every `embeddedDocuments` path from the top level down to the operator's.
        let mut built: Vec<(String, MemIndex)> = Vec::new();
        let ends = self.path.match_indices('.').map(|(i, _)| i).chain([self.path.len()]);
        for end in ends.filter(|&e| e > top.path.len()) {
            let p = &self.path[..end];
            let holders = built.last().map_or(top.index, |(_, i)| i);
            match embedded_mapping(holders, p) {
                Some(m) => {
                    let elements = elements_of(holders, p, m)?;
                    built.push((p.to_owned(), elements));
                }
                None if end == self.path.len() => {
                    return Err(SearchError(format!(
                        "embeddedDocument requires path '{}' to be indexed as \
                         'embeddedDocuments'",
                        self.path
                    )));
                }
                None => {}
            }
        }
        let mut levels = chain.to_vec();
        levels.extend(built.iter().map(|(path, index)| Level { path, index }));
        let last = levels.len() - 1;
        let elements = levels[last].index;
        let op = parse_at(elements, &self.operator, &format!("{}.operator", self.at))?;
        let here = format!("embeddedDocument.path '{}'", self.path);
        // A boost weighs the terms of the elements' operator, as RefLib's `BoostQuery` does.
        let op = boosted(resolve(op, &levels, &here)?, boost);
        f(&levels, last, &op)
    }

    /// The scores of the elements of `levels`' last level by the document of the level of
    /// `top` holding them, in the elements' order.
    fn per_doc(
        levels: &[Level<'_>],
        last: usize,
        top: usize,
        op: &Operator,
    ) -> Result<BTreeMap<u32, Vec<(u32, f32)>>, SearchError> {
        let mut per_doc: BTreeMap<u32, Vec<(u32, f32)>> = BTreeMap::new();
        // The elements in their order, as the scores of a document's elements are added.
        for (id, score) in eval(levels[last].index, op)? {
            if let Some(doc) = holder(levels, last, id, top) {
                per_doc.entry(doc).or_default().push((id, score));
            }
        }
        Ok(per_doc)
    }

    /// The scores of the documents of the last level of `chain` whose elements the operator
    /// matches.
    fn scores_in(
        &self,
        chain: &[Level<'_>],
        boost: f32,
    ) -> Result<BTreeMap<u32, f32>, SearchError> {
        let top = chain.len() - 1;
        let per_doc = self
            .with_elements(chain, boost, |levels, last, op| Self::per_doc(levels, last, top, op))?;
        Ok(per_doc
            .into_iter()
            .map(|(doc, s)| {
                let s: Vec<f32> = s.into_iter().map(|(_, v)| v).collect();
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

    /// The explanation of the score of `doc` (measured: the element that decides it, the
    /// lowest for `minimum` and the highest otherwise, explained on the elements).
    pub(super) fn explain(
        &self,
        ctx: &super::explain::Ctx<'_>,
        boost: f32,
        doc: u32,
        score: f32,
    ) -> Result<Option<crate::index::Explanation>, SearchError> {
        let chain = [Level { path: "", index: ctx.index }];
        self.with_elements(&chain, boost, |levels, last, op| {
            let per_doc = Self::per_doc(levels, last, 0, op)?;
            let Some(elements) = per_doc.get(&doc) else { return Ok(None) };
            let pick = if self.aggregate == "minimum" {
                elements.iter().min_by(|a, b| a.1.total_cmp(&b.1))
            } else {
                elements.iter().max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)))
            };
            let Some(&(id, s)) = pick else { return Ok(None) };
            let inner = super::explain::Ctx {
                index: levels[last].index,
                prefix: format!("{}$embedded:{last}/{}/", ctx.prefix, self.path),
                weighted: ctx.weighted || boost != 1.0,
                levels: levels.to_vec(),
            };
            let child = super::explain::explain(&inner, op, id, s)?;
            Ok(Some(crate::index::Explanation::node(
                score,
                "Score based on child docs, best match:",
                child.into_iter().collect(),
            )))
        })
    }
}
