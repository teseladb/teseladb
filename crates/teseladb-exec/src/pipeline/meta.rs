//! The metadata of each document through a pipeline (010 FR-004, with 011): what `$score`,
//! `$search`, `$vectorSearch` and the other stages that score documents attach to each one, which
//! `$meta` reads and `$sort: {f: {$meta: …}}` sorts by. As a `refrouter` 9.0.4 does (measured in
//! `crates/teseladb-exec/tests/agg/meta.out`), a document keeps its metadata through the stages
//! that change, filter, reorder, unwind or join it, and loses it in the stages that make new
//! documents from many (`$group`, `$bucket`, `$bucketAuto`, `$sortByCount`, `$count`) and in
//! `$unionWith`.

use std::collections::BTreeMap;

use teseladb_bson::{Document, RawValue, Value};
use teseladb_mql::{ErrorCode, Extensions, MqlError};

use super::Env;
use super::errors::{
    BAD_VALUE, FAILED_TO_PARSE, TYPE_MISMATCH, error, missing_field, unknown_field,
};
use crate::expr::{Ctx, Expr, Scope};

/// The metadata of one document, by `$meta` name.
pub type Meta = BTreeMap<String, Value>;

/// The name of the metadata `$score` sets.
pub const SCORE: &str = "score";
/// The name of the metadata of a score's details.
pub const SCORE_DETAILS: &str = "scoreDetails";

/// An environment whose scopes carry one document's metadata.
pub(crate) struct MetaEnv<'a> {
    pub(crate) inner: &'a dyn Env,
    pub(crate) meta: &'a Meta,
}

impl Env for MetaEnv<'_> {
    fn ext(&self) -> &dyn Extensions {
        self.inner.ext()
    }

    fn vars(&self) -> Option<&Document> {
        self.inner.vars()
    }

    fn collection(&self, name: &str) -> Result<Vec<Document>, MqlError> {
        self.inner.collection(name)
    }

    fn random(&self) -> f64 {
        self.inner.random()
    }

    fn system_stage(&self, name: &str, spec: &Document) -> Result<Vec<Document>, MqlError> {
        self.inner.system_stage(name, spec)
    }

    fn collation(&self) -> crate::collation::Collation {
        self.inner.collation()
    }

    fn geo_indexes(&self) -> Result<(String, Vec<super::geo_near::GeoIndex>), MqlError> {
        self.inner.geo_indexes()
    }

    fn js(&self) -> Option<std::rc::Rc<dyn crate::expr::js::JsEngine>> {
        self.inner.js()
    }

    fn allow_disk_use(&self) -> bool {
        self.inner.allow_disk_use()
    }

    fn scope(&self, doc: &Document) -> Scope {
        let mut s = self.inner.scope(doc);
        s.meta.extend(self.meta.iter().map(|(k, v)| (k.clone(), v.clone())));
        s
    }
}

/// How `$score` normalizes the scores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Normalization {
    None,
    Sigmoid,
    MinMaxScaler,
}

const NONE: &str = "none";
const SIGMOID: &str = "sigmoid";
const MIN_MAX_SCALER: &str = "minMaxScaler";

/// A `$score`.
#[derive(Clone, Debug)]
pub struct Score {
    score: Expr,
    /// The `score` argument as `scoreDetails` writes it.
    shown: String,
    normalization: Normalization,
    weight: f64,
    details: bool,
}

/// The `$score` stage's name in its errors.
const STAGE: &str = "$score";

impl Score {
    /// Reads `$score` (measured on a `refrouter` 9.0.4).
    pub fn parse(v: RawValue<'_>, ctx: &Ctx) -> Result<Score, MqlError> {
        let RawValue::Document(d) = v else {
            return Err(error(
                FAILED_TO_PARSE,
                format!(
                    "The $score stage specification must be an object, found {}",
                    crate::expr::arith::raw_type_name(v)
                ),
            ));
        };
        let (mut score, mut normalization, mut weight, mut details) =
            (None, Normalization::None, 1.0, false);
        let mut shown = String::new();
        for (k, x) in d.iter() {
            match k {
                "score" => {
                    score = Some(Expr::parse_in(x, ctx)?);
                    shown = shown_expression(x)?;
                }
                "normalization" => {
                    normalization = match x {
                        RawValue::String(NONE) => Normalization::None,
                        RawValue::String(SIGMOID) => Normalization::Sigmoid,
                        RawValue::String(MIN_MAX_SCALER) => Normalization::MinMaxScaler,
                        other => {
                            return Err(error(
                                BAD_VALUE,
                                format!(
                                    "Enumeration value '{}' for field '$score.normalization' is not a valid value.",
                                    crate::expr::values::display(&Value::from(other))
                                        .trim_matches('"')
                                ),
                            ));
                        }
                    };
                }
                "weight" => {
                    let w = match x {
                        RawValue::Int32(_)
                        | RawValue::Int64(_)
                        | RawValue::Double(_)
                        | RawValue::Decimal128(_) => {
                            crate::expr::values::f64_of(&Value::from(x)).unwrap_or(0.0)
                        }
                        other => {
                            return Err(super::errors::wrong_type(
                                "$score.weight",
                                other,
                                "types '[double, int, decimal, long]'",
                            ));
                        }
                    };
                    let shown = crate::expr::values::display(&Value::from(x));
                    if w < 0.0 {
                        return Err(error(
                            BAD_VALUE,
                            format!(
                                "BSON field 'weight' value must be >= 0, actual value '{shown}'"
                            ),
                        ));
                    }
                    if w > 1.0 {
                        return Err(error(
                            BAD_VALUE,
                            format!(
                                "BSON field 'weight' value must be <= 1, actual value '{shown}'"
                            ),
                        ));
                    }
                    weight = w;
                }
                "scoreDetails" => details = crate::expr::truthy(&Some(Value::from(x))),
                other => return Err(unknown_field(STAGE, other)),
            }
        }
        let score = score.ok_or_else(|| missing_field(STAGE, "score"))?;
        Ok(Score { score, shown, normalization, weight, details })
    }

    /// The metadata of each document: its score (and details), from its own metadata `metas`.
    pub(crate) fn run(
        &self,
        docs: &[Document],
        metas: &[Meta],
        env: &dyn Env,
    ) -> Result<Vec<Meta>, MqlError> {
        let mut raw = Vec::with_capacity(docs.len());
        for (d, m) in docs.iter().zip(metas) {
            let menv = MetaEnv { inner: env, meta: m };
            let v = self.score.eval(d, &menv.scope(d))?;
            let x = match &v {
                Some(n) if crate::expr::values::is_number(n) => {
                    crate::expr::values::f64_of(n).unwrap_or(0.0)
                }
                other => {
                    let got = other.as_ref().map_or("missing", crate::expr::arith::type_name_of);
                    return Err(error(
                        TYPE_MISMATCH,
                        format!("Meta field 'score' requires numeric type, found {got}"),
                    ));
                }
            };
            raw.push(x);
        }
        let (lo, hi) = raw
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), x| (lo.min(*x), hi.max(*x)));
        Ok(raw
            .iter()
            .zip(metas)
            .map(|(x, m)| {
                let normal = match self.normalization {
                    Normalization::None => *x,
                    Normalization::Sigmoid => 1.0 / (1.0 + (-x).exp()),
                    Normalization::MinMaxScaler if hi > lo => (x - lo) / (hi - lo),
                    Normalization::MinMaxScaler => 0.0,
                };
                let value = normal * self.weight;
                let mut out = m.clone();
                out.insert(SCORE.to_owned(), Value::Double(value));
                if self.details {
                    out.insert(SCORE_DETAILS.to_owned(), self.details_of(value, *x));
                }
                out
            })
            .collect())
    }

    /// `scoreDetails` of one document.
    fn details_of(&self, value: f64, raw: f64) -> Value {
        let normalization = match self.normalization {
            Normalization::None => NONE,
            Normalization::Sigmoid => SIGMOID,
            Normalization::MinMaxScaler => MIN_MAX_SCALER,
        };
        Value::Document(
            Document::new()
                .with("value", value)
                .with(
                    "description",
                    "the score calculated from multiplying a weight in the range [0,1] with either a normalized or nonnormalized value:",
                )
                .with("rawScore", raw)
                .with("normalization", normalization)
                .with("weight", self.weight)
                .with("expression", self.shown.clone())
                .with("details", Value::Array(Vec::new())),
        )
    }
}

/// The `expression` of `scoreDetails`: the argument under a `string` field, written as the
/// server writes BSON in its messages with its strings in single quotes (measured:
/// `{ string: { $add: [ '$_id', 1 ] } }`).
pub(crate) fn shown_expression(x: RawValue<'_>) -> Result<String, MqlError> {
    let holder = Document::new().with("string", Value::from(x));
    let bytes = crate::raw::bytes_of(&holder)?;
    let raw = teseladb_bson::RawDoc::from_bytes(&bytes).map_err(crate::raw::bad)?;
    Ok(teseladb_mql::shell(RawValue::Document(raw)).replace('"', "'"))
}

/// RefDb's code for metadata a stage asks for and no stage before gives.
pub const META_MISSING: ErrorCode = ErrorCode { code: 40_218, name: "Location40218" };

/// The metadata `$score` sets, which a pipeline checks before it runs.
const OWN: [&str; 2] = [SCORE, SCORE_DETAILS];

/// The score `$search` gives each document.
pub const SEARCH_SCORE: &str = "searchScore";
/// The score `$vectorSearch` gives each document.
pub const VECTOR_SEARCH_SCORE: &str = "vectorSearchScore";
/// The metadata of `returnScope` searches, which no stage of this engine gives.
const SEARCH_ROOT_DOCUMENT_ID: &str = "searchRootDocumentId";
/// The metadata a pipeline refuses unless a search stage before gave it, whatever stages
/// follow (measured on 9.0.4: after `$group` a read of it gives nothing).
const SEARCH_OWN: [&str; 3] = [SEARCH_SCORE, VECTOR_SEARCH_SCORE, SEARCH_ROOT_DOCUMENT_ID];

/// The score metadata search stage `name` gives, if it scores documents.
pub fn search_score_of(name: &str) -> Option<&'static str> {
    match name {
        "$search" => Some(SEARCH_SCORE),
        "$vectorSearch" => Some(VECTOR_SEARCH_SCORE),
        _ => None,
    }
}

/// The metadata of a document a search stage gave: `score` is the stage's score `own` unless
/// the stage set it (measured: `$meta: "score"` after `$search` is its `searchScore`).
pub fn with_score(mut m: Meta, own: Option<&str>) -> Meta {
    if let Some(v) = own.and_then(|o| m.get(o)).cloned() {
        m.entry(SCORE.to_owned()).or_insert(v);
    }
    m
}

/// An environment whose scopes give nothing, not an error, for the metadata `names`.
pub(crate) struct MissingOkEnv<'a> {
    pub(crate) inner: &'a dyn Env,
    pub(crate) names: &'a [String],
}

impl Env for MissingOkEnv<'_> {
    fn ext(&self) -> &dyn Extensions {
        self.inner.ext()
    }

    fn vars(&self) -> Option<&Document> {
        self.inner.vars()
    }

    fn collection(&self, name: &str) -> Result<Vec<Document>, MqlError> {
        self.inner.collection(name)
    }

    fn random(&self) -> f64 {
        self.inner.random()
    }

    fn system_stage(&self, name: &str, spec: &Document) -> Result<Vec<Document>, MqlError> {
        self.inner.system_stage(name, spec)
    }

    fn search_source(
        &self,
        stage: &str,
        spec: &Document,
    ) -> Result<Vec<super::search::Scored>, MqlError> {
        self.inner.search_source(stage, spec)
    }

    fn collation(&self) -> crate::collation::Collation {
        self.inner.collation()
    }

    fn geo_indexes(&self) -> Result<(String, Vec<super::geo_near::GeoIndex>), MqlError> {
        self.inner.geo_indexes()
    }

    fn js(&self) -> Option<std::rc::Rc<dyn crate::expr::js::JsEngine>> {
        self.inner.js()
    }

    fn allow_disk_use(&self) -> bool {
        self.inner.allow_disk_use()
    }

    fn scope(&self, doc: &Document) -> Scope {
        let mut s = self.inner.scope(doc);
        s.meta_missing_ok.extend(self.names.iter().cloned());
        s
    }
}

/// The stages after which documents have no metadata.
const DROPPING: [&str; 6] =
    ["$group", "$bucket", "$bucketAuto", "$sortByCount", "$count", "$unionWith"];

/// Refuses a stage that reads `score` or `scoreDetails` with `$meta` where no `$score` before it
/// gives them (40218, measured on a `refrouter` 9.0.4).
pub(crate) fn check_available(stages: &[RawValue<'_>]) -> Result<(), MqlError> {
    let mut available: Vec<&str> = Vec::new();
    let mut searched: Vec<&str> = Vec::new();
    let usable = |n: &str, available: &[&str], searched: &[&str]| {
        if OWN.contains(&n) {
            available.contains(&n)
        } else if SEARCH_OWN.contains(&n) {
            searched.contains(&n)
        } else {
            true
        }
    };
    for st in stages {
        let RawValue::Document(d) = st else { continue };
        let Some((name, arg)) = d.iter().next() else { continue };
        let mut read = Vec::new();
        metas_read(arg, &mut read);
        for r in &read {
            if !usable(r, &available, &searched) {
                return Err(crate::expr::missing_meta(r));
            }
        }
        if let Some(score) = search_score_of(name) {
            available.push(SCORE);
            searched.push(score);
        }
        if matches!(name, "$score" | "$rankFusion" | "$scoreFusion") {
            available.push(SCORE);
            if let RawValue::Document(s) = arg
                && s.get("scoreDetails").is_some_and(|v| crate::expr::truthy(&Some(Value::from(v))))
            {
                available.push(SCORE_DETAILS);
            }
        }
        if DROPPING.contains(&name) {
            available.clear();
        }
        // A `$sort` by metadata reads it too.
        if name == "$sort"
            && let RawValue::Document(s) = arg
        {
            for (_, x) in s.iter() {
                if let RawValue::Document(m) = x
                    && let Some(RawValue::String(n)) = m.get("$meta")
                    && !usable(n, &available, &searched)
                {
                    return Err(crate::expr::missing_meta(n));
                }
            }
        }
    }
    Ok(())
}

/// The required metadata names `v` reads with `$meta` (anywhere in a stage's specification).
pub(crate) fn metas_read(v: RawValue<'_>, out: &mut Vec<String>) {
    match v {
        RawValue::Document(d) => {
            for (k, x) in d.iter() {
                match (k, x) {
                    ("$meta", RawValue::String(name)) => out.push(name.to_owned()),
                    _ => metas_read(x, out),
                }
            }
        }
        RawValue::Array(a) => a.iter().for_each(|x| metas_read(x, out)),
        _ => {}
    }
}
