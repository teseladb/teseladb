//! `$rankFusion` and `$scoreFusion` (010 FR-004, T042), as a reference router 9.0.4 runs them (measured
//! in `crates/teseladb-exec/tests/agg/fusion.out`): each input pipeline runs over the collection,
//! and every document any of them gives comes out once with a score that combines them, the
//! highest first (equal scores by `_id`). `$rankFusion` adds `weight / (60 + rank)` over the
//! pipelines that give the document; `$scoreFusion` averages over all the pipelines the weighted
//! scores of `$score` (normalized as asked), or computes its `expression` with each pipeline's
//! score in a variable of its name.

use std::cmp::Ordering;
use std::collections::HashMap;

use teseladb_bson::{Document, RawValue, Value};
use teseladb_mql::{ErrorCode, MqlError};

use super::errors::{BAD_VALUE, FAILED_TO_PARSE, error, missing_field, unknown_field};
use super::meta::{Meta, SCORE, SCORE_DETAILS};
use super::{Env, Pipeline};
use crate::expr::{Ctx, Expr, Scope};

/// A location code.
macro_rules! loc {
    ($n:literal) => {
        ErrorCode { code: $n, name: concat!("Location", $n) }
    };
}

const NOT_RANKED: ErrorCode = loc!(12108702);
const TRANSFORMS: ErrorCode = loc!(12108704);
const NOT_SCORED: ErrorCode = loc!(12108712);
const BAD_WEIGHT_NAME: ErrorCode = loc!(9967500);
const NOT_FIRST: ErrorCode = loc!(10170100);

/// The rank constant of reciprocal rank fusion.
const RANK_CONSTANT: f64 = 60.0;

/// The stages an input pipeline may hold: those that find, order or cut documents.
const ALLOWED: [&str; 11] = [
    "$match",
    "$sort",
    "$limit",
    "$skip",
    "$sample",
    "$score",
    "$search",
    "$vectorSearch",
    "$geoNear",
    "$searchMeta",
    "$rankFusion",
];

/// Which fusion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Rank,
    Score,
}

/// How `$scoreFusion` normalizes each pipeline's scores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Normalization {
    None,
    Sigmoid,
    MinMaxScaler,
}

/// A `$rankFusion` or `$scoreFusion`.
#[derive(Clone, Debug)]
pub struct Fusion {
    kind: Kind,
    pipelines: Vec<(String, Pipeline)>,
    weights: Vec<f64>,
    normalization: Normalization,
    expression: Option<(Expr, String)>,
    details: bool,
}

/// The error of a fusion anywhere but first.
pub(crate) fn not_first() -> MqlError {
    error(
        NOT_FIRST,
        "$rankFusion/$scoreFusion can only be the first stage of an aggregation pipeline.",
    )
}

/// The edit distance of two names.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(row[j + 1] + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

impl Fusion {
    /// The metadata it gives every document: the score, and its details when asked for.
    pub(crate) fn gives(&self) -> &'static [&'static str] {
        match self.details {
            true => &[SCORE, SCORE_DETAILS],
            false => &[SCORE],
        }
    }

    /// Reads `$rankFusion` (`score` false) or `$scoreFusion`.
    pub fn parse(name: &str, v: RawValue<'_>, ctx: &Ctx) -> Result<Fusion, MqlError> {
        let kind = if name == "$scoreFusion" { Kind::Score } else { Kind::Rank };
        let RawValue::Document(d) = v else {
            return Err(error(
                FAILED_TO_PARSE,
                format!(
                    "{name} must take a nested object but found: {name}: {}",
                    crate::expr::values::display(&Value::from(v))
                ),
            ));
        };
        let (mut input, mut combination, mut details) = (None, None, false);
        for (k, x) in d.iter() {
            match (k, x) {
                ("input", RawValue::Document(i)) => input = Some(i),
                ("combination", RawValue::Document(c)) => combination = Some(c),
                ("scoreDetails", x) => details = crate::expr::truthy(&Some(Value::from(x))),
                (other, _) => return Err(unknown_field(name, other)),
            }
        }
        let input = input.ok_or_else(|| missing_field(name, "input"))?;
        let mut pipelines = Vec::new();
        let mut normalization = None;
        for (k, x) in input.iter() {
            match (k, x) {
                ("pipelines", RawValue::Document(p)) => {
                    for (pname, stages) in p.iter() {
                        let RawValue::Array(a) = stages else { continue };
                        let stages: Vec<RawValue<'_>> = a.iter().collect();
                        check_input(kind, &stages)?;
                        pipelines.push((pname.to_owned(), Pipeline::parse(&stages, ctx)?));
                    }
                }
                ("normalization", RawValue::String(s)) if kind == Kind::Score => {
                    normalization = Some(match s {
                        "none" => Normalization::None,
                        "sigmoid" => Normalization::Sigmoid,
                        "minMaxScaler" => Normalization::MinMaxScaler,
                        other => {
                            return Err(error(
                                BAD_VALUE,
                                format!(
                                    "Enumeration value '{other}' for field '{name}.input.normalization' is not a valid value."
                                ),
                            ));
                        }
                    });
                }
                (other, _) => return Err(unknown_field(&format!("{name}.input"), other)),
            }
        }
        if pipelines.is_empty() {
            return Err(error(
                BAD_VALUE,
                "A hybrid scoring stage should be run with at least one pipeline.",
            ));
        }
        let normalization = match kind {
            Kind::Rank => Normalization::None,
            Kind::Score => normalization
                .ok_or_else(|| missing_field(&format!("{name}.input"), "normalization"))?,
        };
        let mut weights = vec![1.0; pipelines.len()];
        let mut expression = None;
        if let Some(c) = combination {
            for (k, x) in c.iter() {
                match (k, x) {
                    ("weights", RawValue::Document(w)) => {
                        let mut wrong = Vec::new();
                        for (wn, wv) in w.iter() {
                            match pipelines.iter().position(|(p, _)| p == wn) {
                                Some(i) => {
                                    weights[i] = crate::expr::values::f64_of(&Value::from(wv))
                                        .unwrap_or(0.0);
                                }
                                None => wrong.push(wn.to_owned()),
                            }
                        }
                        if !wrong.is_empty() {
                            let hints: Vec<String> = wrong
                                .iter()
                                .map(|n| {
                                    let near = pipelines
                                        .iter()
                                        .min_by_key(|(p, _)| distance(n, p))
                                        .map_or("", |(p, _)| p.as_str());
                                    format!("(provided: '{n}' -> suggested: '{near}')")
                                })
                                .collect();
                            return Err(error(
                                BAD_WEIGHT_NAME,
                                format!(
                                    "${name} stage contained ({}) weight(s) in 'combination.weights' that did not reference valid pipeline names. Suggestions for valid pipeline names: {}",
                                    wrong.len(),
                                    hints.join(", ")
                                ),
                            ));
                        }
                    }
                    ("method", _) => {}
                    ("expression", e) if kind == Kind::Score => {
                        let names: Vec<&str> = pipelines.iter().map(|(p, _)| p.as_str()).collect();
                        expression = Some((
                            Expr::parse_in(e, &ctx.with(names))?,
                            super::meta::shown_expression(e)?,
                        ));
                    }
                    (other, _) => return Err(unknown_field(&format!("{name}.combination"), other)),
                }
            }
        }
        Ok(Fusion { kind, pipelines, weights, normalization, expression, details })
    }

    /// Each document any input pipeline gives, once, the highest score first, with its score.
    pub(crate) fn run(
        &self,
        docs: Vec<Document>,
        env: &dyn Env,
    ) -> Result<(Vec<Document>, Vec<Meta>), MqlError> {
        let mut found: Vec<(Document, Vec<Option<f64>>)> = Vec::new();
        let mut index: HashMap<Vec<u8>, usize> = HashMap::new();
        let n = self.pipelines.len();
        for (p, (_, pipeline)) in self.pipelines.iter().enumerate() {
            let (out, metas) = pipeline.run_with_meta(docs.clone(), None, env, false)?;
            let raw: Vec<f64> = match self.kind {
                Kind::Rank => ranks(&out, pipeline.last_sort(), env)?,
                Kind::Score => metas
                    .unwrap_or_default()
                    .iter()
                    .map(|m| m.get(SCORE).and_then(crate::expr::values::f64_of).unwrap_or(0.0))
                    .collect(),
            };
            let normal = self.normalized(&raw);
            for (d, s) in out.into_iter().zip(normal) {
                let key = super::group_key(d.get("_id").unwrap_or(&Value::Null), None);
                let i = *index.entry(key).or_insert_with(|| {
                    found.push((d, vec![None; n]));
                    found.len() - 1
                });
                if found[i].1[p].is_none() {
                    found[i].1[p] = Some(s);
                }
            }
        }
        let mut scored = Vec::with_capacity(found.len());
        for (d, per) in found {
            let value = self.combined(&d, &per, env)?;
            let mut meta = Meta::new();
            meta.insert(SCORE.to_owned(), Value::Double(value));
            if self.details {
                meta.insert(SCORE_DETAILS.to_owned(), self.details_of(value, &per));
            }
            scored.push((value, d, meta));
        }
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal).then_with(|| {
                crate::expr::order(&a.1.get("_id").cloned(), &b.1.get("_id").cloned(), None)
                    .unwrap_or(Ordering::Equal)
            })
        });
        Ok(scored.into_iter().map(|(_, d, m)| (d, m)).unzip())
    }

    /// One pipeline's scores as fusion takes them: ranks as they are, scores normalized.
    fn normalized(&self, raw: &[f64]) -> Vec<f64> {
        let (lo, hi) = raw
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), x| (lo.min(*x), hi.max(*x)));
        raw.iter()
            .map(|x| match self.normalization {
                Normalization::None => *x,
                Normalization::Sigmoid => 1.0 / (1.0 + (-x).exp()),
                Normalization::MinMaxScaler if hi > lo => (x - lo) / (hi - lo),
                Normalization::MinMaxScaler => 0.0,
            })
            .collect()
    }

    /// The combined score of a document from its score in each pipeline (`None` where absent).
    fn combined(&self, d: &Document, per: &[Option<f64>], env: &dyn Env) -> Result<f64, MqlError> {
        if let (Kind::Score, Some((e, _))) = (self.kind, &self.expression) {
            let mut scope: Scope = env.scope(d);
            for ((name, _), s) in self.pipelines.iter().zip(per) {
                scope.vars.insert(name.clone(), Some(Value::Double(s.unwrap_or(0.0))));
            }
            let v = e.eval(d, &scope)?;
            return Ok(v.as_ref().and_then(crate::expr::values::f64_of).unwrap_or(0.0));
        }
        let weighted = per.iter().zip(&self.weights).filter_map(|(s, w)| s.map(|s| (s, *w)));
        Ok(match self.kind {
            Kind::Rank => weighted.map(|(rank, w)| w / (RANK_CONSTANT + rank)).sum(),
            Kind::Score => weighted.map(|(s, w)| w * s).sum::<f64>() / self.pipelines.len() as f64,
        })
    }

    /// `scoreDetails` of a document.
    fn details_of(&self, value: f64, per: &[Option<f64>]) -> Value {
        let details: Vec<Value> = self
            .pipelines
            .iter()
            .zip(per)
            .zip(&self.weights)
            .filter_map(|(((name, _), s), w)| {
                let s = (*s)?;
                let d = match self.kind {
                    Kind::Rank => Document::new()
                        .with("inputPipelineName", name.as_str())
                        .with("rank", s as i32)
                        .with("weight", *w),
                    Kind::Score => Document::new()
                        .with("inputPipelineName", name.as_str())
                        .with("inputPipelineRawScore", s)
                        .with("weight", *w)
                        .with("value", s * w),
                };
                Some(Value::Document(d.with("details", Value::Array(Vec::new()))))
            })
            .collect();
        let d = match self.kind {
            Kind::Rank => Document::new().with("value", value).with(
                "description",
                "value output by reciprocal rank fusion algorithm, computed as sum of (weight * (1 / (60 + rank))) across input pipelines from which this document is output, from:",
            ),
            Kind::Score => {
                let combination = match &self.expression {
                    Some((_, shown)) => Document::new()
                        .with("method", "custom expression")
                        .with("expression", shown.clone()),
                    None => Document::new().with("method", "average"),
                };
                let normalization = match self.normalization {
                    Normalization::None => "none",
                    Normalization::Sigmoid => "sigmoid",
                    Normalization::MinMaxScaler => "minMaxScaler",
                };
                Document::new()
                    .with("value", value)
                    .with(
                        "description",
                        "the value calculated by combining the scores (either normalized or raw) across input pipelines from which this document is output from:",
                    )
                    .with("normalization", normalization)
                    .with("combination", combination)
            }
        };
        Value::Document(d.with("details", Value::Array(details)))
    }
}

/// The rank of each document a ranked pipeline gives: one more than the documents before it
/// whose sort key differs from its own, equal keys sharing a rank (measured).
fn ranks(
    docs: &[Document],
    sort: Option<&super::sort::SortSpec>,
    env: &dyn Env,
) -> Result<Vec<f64>, MqlError> {
    let collation = env.collation();
    let mut out = Vec::with_capacity(docs.len());
    let mut previous: Option<Vec<Value>> = None;
    for (i, d) in docs.iter().enumerate() {
        let key = match sort {
            Some(s) => Some(s.key(d, collation.get())?),
            None => None,
        };
        let same = match (&key, &previous, sort) {
            (Some(k), Some(p), Some(s)) => s.compare(k, p, collation.get())? == Ordering::Equal,
            _ => false,
        };
        let rank = match (same, out.last()) {
            (true, Some(r)) => *r,
            _ => (i + 1) as f64,
        };
        out.push(rank);
        previous = key;
    }
    Ok(out)
}

/// Checks an input pipeline: only stages that find, order or cut documents, and a `$sort` (or a
/// ranked first stage) for `$rankFusion`, a `$score` (or a scored first stage) for `$scoreFusion`.
fn check_input(kind: Kind, stages: &[RawValue<'_>]) -> Result<(), MqlError> {
    let names: Vec<&str> = stages
        .iter()
        .filter_map(|s| match s {
            RawValue::Document(d) => d.iter().next().map(|(k, _)| k),
            _ => None,
        })
        .collect();
    let fusion = if kind == Kind::Rank { "$rankFusion" } else { "$scoreFusion" };
    if let Some(bad) = names.iter().find(|n| !ALLOWED.contains(n)) {
        return Err(error(
            TRANSFORMS,
            format!(
                "{fusion} input pipelines must not contain {bad} because it modifies or transforms the input documents. Only stages that retrieve, limit, or order documents are allowed."
            ),
        ));
    }
    let first = names.first().copied().unwrap_or_default();
    match kind {
        Kind::Rank
            if !matches!(first, "$search" | "$vectorSearch" | "$geoNear")
                && !names.contains(&"$sort") =>
        {
            Err(error(
                NOT_RANKED,
                "Pipeline did not begin with a ranked stage and did not contain an explicit $sort stage. All input pipelines to the $rankFusion stage must begin with one of $search, $vectorSearch, $geoNear, or have a $sort in the pipeline.",
            ))
        }
        Kind::Score
            if !matches!(first, "$search" | "$vectorSearch") && !names.contains(&"$score") =>
        {
            Err(error(
                NOT_SCORED,
                "Pipeline did not begin with a scored stage and did not contain an explicit $score stage. All input pipelines to the $scoreFusion stage must begin with one of $search, $vectorSearch, or have a custom $score in the pipeline.",
            ))
        }
        _ => Ok(()),
    }
}
