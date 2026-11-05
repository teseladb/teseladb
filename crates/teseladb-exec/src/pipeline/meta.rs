//! The metadata of each document through a pipeline (010 FR-004, with 011): what `$score`,
//! `$search`, `$vectorSearch` and the other stages that score documents attach to each one, which
//! `$meta` reads and `$sort: {f: {$meta: …}}` sorts by. As a `refrouter` 9.0.4 does (measured in
//! `crates/teseladb-exec/tests/agg/meta.out`), a document keeps its metadata through the stages
//! that change, filter, reorder, unwind or join it, and loses it in the stages that make new
//! documents from many (`$group`, `$bucket`, `$bucketAuto`, `$sortByCount`, `$count`) and in
//! `$unionWith`.

use std::collections::BTreeMap;

use teseladb_mql::{ErrorCode, Extensions, MqlError};
use teseladb_bson::{Document, RawValue, Value};

use super::Env;
use super::errors::{
    BAD_VALUE, FAILED_TO_PARSE, TYPE_MISMATCH, error, missing_field, unknown_field,
};
use crate::expr::{Ctx, Expr, Scope};

/// The metadata of one document, by `$meta` name.
pub type Meta = BTreeMap<String, Value>;

/// The name of the metadata `$score` sets.
pub const SCORE: &str = "score";
/// The name of the random value `$sample` gives each document it takes.
pub const RAND_VAL: &str = "randVal";
/// The name of the key of the index entry a document was read by.
pub const INDEX_KEY: &str = "indexKey";
/// The name of the metadata of a score's details.
pub const SCORE_DETAILS: &str = "scoreDetails";

/// The score `$search` gives each document.
pub const SEARCH_SCORE: &str = "searchScore";
/// The score of a `$text` query.
pub const TEXT_SCORE: &str = "textScore";
/// The score `$vectorSearch` gives each document.
pub const VECTOR_SEARCH_SCORE: &str = "vectorSearchScore";

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

/// An environment whose scopes carry one document's metadata.
pub(crate) struct MetaEnv<'a> {
    pub(crate) inner: &'a dyn Env,
    pub(crate) meta: &'a Meta,
    /// The metadata of documents marked with their place ([`MARK`]), for stages that see many
    /// documents at once: a marked document's scope has its own metadata, without the mark.
    pub(crate) marked: &'a [Meta],
}

/// The field that marks a document's place while a stage that sees many documents runs over
/// documents with metadata (a name that starts with a control character, taken off after it).
pub(crate) const MARK: &str = "\u{1}meta";

/// Takes the marks off `v` and the documents and arrays in it.
pub(crate) fn unmark(v: &mut Value) {
    match v {
        Value::Document(d) => unmark_doc(d),
        Value::Array(a) => a.iter_mut().for_each(unmark),
        _ => {}
    }
}

/// Takes the marks off `d` and the documents and arrays in it.
pub(crate) fn unmark_doc(d: &mut Document) {
    d.remove(MARK);
    let keys: Vec<String> = d.iter().map(|(k, _)| k.to_owned()).collect();
    for k in keys {
        d.get_mut(&k).into_iter().for_each(unmark);
    }
}

/// An environment whose scopes refuse a missing read of the metadata `names` (011: the scores
/// of a search stage before, for the documents that did not come from it).
pub(crate) struct RequiredEnv<'a> {
    pub(crate) inner: &'a dyn Env,
    pub(crate) names: &'a [String],
    /// The variables with `SEARCH_META` set, after a `$search` whose metadata the pipeline
    /// reads (011 FR-004).
    pub(crate) vars: Option<&'a Document>,
}

impl Env for RequiredEnv<'_> {
    fn ext(&self) -> &dyn Extensions {
        self.inner.ext()
    }

    fn vars(&self) -> Option<&Document> {
        self.vars.or_else(|| self.inner.vars())
    }

    fn collection_matching(
        &self,
        name: &str,
        filter: &Document,
    ) -> Result<Vec<Document>, MqlError> {
        self.inner.collection_matching(name, filter)
    }

    fn operation_memory_limit(&self) -> usize {
        self.inner.operation_memory_limit()
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

    fn check_search(&self, stage: &str, spec: &Document) -> Result<(), MqlError> {
        self.inner.check_search(stage, spec)
    }

    fn search_source(
        &self,
        stage: &str,
        spec: &Document,
    ) -> Result<Vec<super::search::Scored>, MqlError> {
        self.inner.search_source(stage, spec)
    }

    fn joined_text_source(
        &self,
        name: &str,
        filter: &Document,
        vars: Option<&Document>,
    ) -> Result<Vec<super::search::Scored>, MqlError> {
        self.inner.joined_text_source(name, filter, vars)
    }

    fn joined_search_source(
        &self,
        name: &str,
        query: (&str, &Document),
        from_lookup: bool,
    ) -> Result<Vec<super::search::Scored>, MqlError> {
        self.inner.joined_search_source(name, query, from_lookup)
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

    fn stage_memory_limit(&self) -> usize {
        self.inner.stage_memory_limit()
    }

    fn spill(&self) -> Option<&dyn super::spill::SpillStore> {
        self.inner.spill()
    }

    fn scope(&self, doc: &Document) -> Scope {
        let mut s = self.inner.scope(doc);
        s.meta_required.extend(self.names.iter().cloned());
        if let Some(meta) = self.vars.and_then(|v| v.get(SEARCH_META)) {
            s.vars.insert(SEARCH_META.to_owned(), Some(meta.clone()));
        }
        s
    }
}

/// The variable a `$search` sets for the stages after it.
pub(crate) const SEARCH_META: &str = "SEARCH_META";

impl Env for MetaEnv<'_> {
    fn ext(&self) -> &dyn Extensions {
        self.inner.ext()
    }

    fn joined_text_source(
        &self,
        name: &str,
        filter: &Document,
        vars: Option<&Document>,
    ) -> Result<Vec<super::search::Scored>, MqlError> {
        self.inner.joined_text_source(name, filter, vars)
    }

    fn joined_search_source(
        &self,
        name: &str,
        query: (&str, &Document),
        from_lookup: bool,
    ) -> Result<Vec<super::search::Scored>, MqlError> {
        self.inner.joined_search_source(name, query, from_lookup)
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

    fn stage_memory_limit(&self) -> usize {
        self.inner.stage_memory_limit()
    }

    fn spill(&self) -> Option<&dyn super::spill::SpillStore> {
        self.inner.spill()
    }

    fn scope(&self, doc: &Document) -> Scope {
        let place = match doc.get(MARK) {
            Some(Value::Int64(i)) => usize::try_from(*i).ok(),
            _ => None,
        };
        let Some(own) = place.and_then(|i| self.marked.get(i)) else {
            let mut s = self.inner.scope(doc);
            s.meta.extend(self.meta.iter().map(|(k, v)| (k.clone(), v.clone())));
            return s;
        };
        let mut root = doc.clone();
        root.remove(MARK);
        let mut s = self.inner.scope(&root);
        s.meta.extend(own.iter().map(|(k, v)| (k.clone(), v.clone())));
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
    /// The `weight` argument as given (`1` when absent), which `scoreDetails` shows when every
    /// score is the same under `minMaxScaler`.
    weight_given: Value,
    details: bool,
}

/// The `$score` stage's name in its errors.
const STAGE: &str = "$score";

impl Score {
    /// The metadata it gives every document: the score, and its details when asked for.
    pub(crate) fn gives(&self) -> &'static [&'static str] {
        match self.details {
            true => &[SCORE, SCORE_DETAILS],
            false => &[SCORE],
        }
    }

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
        let mut weight_given = Value::Int32(1);
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
                    weight_given = Value::from(x);
                }
                "scoreDetails" => details = crate::expr::truthy(&Some(Value::from(x))),
                other => return Err(unknown_field(STAGE, other)),
            }
        }
        let score = score.ok_or_else(|| missing_field(STAGE, "score"))?;
        Ok(Score { score, shown, normalization, weight, weight_given, details })
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
            let menv = MetaEnv { inner: env, meta: m, marked: &[] };
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
            raw.push((x, v));
        }
        let (lo, hi) = raw
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), (x, _)| (lo.min(*x), hi.max(*x)));
        Ok(raw
            .iter()
            .zip(metas)
            .map(|((x, v), m)| {
                let normal = match self.normalization {
                    Normalization::None => *x,
                    Normalization::Sigmoid => 1.0 / (1.0 + (-x).exp()),
                    Normalization::MinMaxScaler if hi > lo => (x - lo) / (hi - lo),
                    // Every score the same: the integer 0, with the score and the weight as
                    // given (measured on 9.0.4, `scratchpad/lane3s/m92.js` to `m94.js`).
                    Normalization::MinMaxScaler => {
                        let raw = v.clone().unwrap_or(Value::Null);
                        return self.scored(m, Value::Int32(0), raw, self.weight_given.clone());
                    }
                };
                let value = normal * self.weight;
                self.scored(m, Value::Double(value), Value::Double(*x), Value::Double(self.weight))
            })
            .collect())
    }

    /// `m` with the score `value`, and its `scoreDetails` when asked.
    fn scored(&self, m: &Meta, value: Value, raw: Value, weight: Value) -> Meta {
        let mut out = m.clone();
        if self.details {
            out.insert(SCORE_DETAILS.to_owned(), self.details_of(value.clone(), raw, weight));
        }
        out.insert(SCORE.to_owned(), value);
        out
    }

    /// `scoreDetails` of one document.
    fn details_of(&self, value: Value, raw: Value, weight: Value) -> Value {
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
                .with("weight", weight)
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

/// The stages after which documents have no metadata.
const DROPPING: [&str; 5] = ["$group", "$bucket", "$bucketAuto", "$sortByCount", "$count"];

/// The metadata a stage gives its documents (`$match` only with `$text`, which goes first).
fn given_by(name: &str, arg: RawValue<'_>) -> Vec<&'static str> {
    let details = matches!(arg, RawValue::Document(s)
        if s.get("scoreDetails").is_some_and(|v| crate::expr::truthy(&Some(Value::from(v)))));
    let mut given = match name {
        "$score" | "$rankFusion" | "$scoreFusion" => vec![SCORE],
        // `searchRootDocumentId` needs `returnScope`, which no stage of this engine runs
        // (measured on 9.0.4: 40218 after `$search`).
        "$search" => vec![SEARCH_SCORE, SCORE],
        "$vectorSearch" => vec![VECTOR_SEARCH_SCORE, SCORE],
        "$geoNear" => vec!["geoNearDistance", "geoNearPoint"],
        "$match" if matches!(arg, RawValue::Document(m) if super::has_text(m)) => {
            vec!["textScore", SCORE]
        }
        _ => Vec::new(),
    };
    if details && !given.is_empty() {
        given.push(SCORE_DETAILS);
    }
    given
}

/// Refuses a stage that reads with `$meta` metadata that no stage before it gives (40218,
/// measured on a `refrouter` 9.0.4 for every name): `$facet`'s pipelines see the metadata of the
/// stage's input, and `$redact` reads a missing value instead. The pipelines of `$lookup` and
/// `$unionWith` start without any and are checked when they run ([`check_joined`]).
pub(crate) fn check_available(stages: &[RawValue<'_>]) -> Result<(), MqlError> {
    check_pipeline(stages, Vec::new(), true)
}

/// [`check_available`] for the pipeline of a `$lookup` or `$unionWith` on a collection, which a
/// `refrouter` 9.0.4 checks when it first runs it (measured: a `$lookup` with no input document
/// gives no error).
pub(crate) fn check_joined(stages: &[RawValue<'_>]) -> Result<(), MqlError> {
    check_pipeline(stages, Vec::new(), true)
}

/// The words a `refrouter` 9.0.4 puts before the missing metadata of a `$sort` that only `$match`
/// and `$sort` stages precede in a pipeline on a collection: it asks the query system whether it
/// can run the pipeline's start, and that fails first (measured 01-11-2026 for each sortable name;
/// not in `$facet`, nor after any other stage).
const DISTINCT_SCAN_CHECK: &str = "Failed to determine whether query system can provide a DISTINCT_SCAN grouping :: caused by :: ";

fn check_pipeline(
    stages: &[RawValue<'_>],
    mut available: Vec<&str>,
    on_collection: bool,
) -> Result<(), MqlError> {
    let mut leading = on_collection;
    // Whether a stage before gave the text score, which a `$match` with `$text` in a `$facet`
    // reads (measured on 9.0.4: 40218 without a first one, also after a `$group`).
    let mut texted = false;
    for st in stages {
        let RawValue::Document(d) = st else { continue };
        let Some((name, arg)) = d.iter().next() else { continue };
        match name {
            "$facet" => {
                if let RawValue::Document(f) = arg {
                    for (_, p) in f.iter() {
                        if let RawValue::Array(p) = p {
                            let p: Vec<_> = p.iter().collect();
                            if !texted && p.first().is_some_and(|s| text_match(*s)) {
                                return Err(crate::expr::missing_meta(TEXT_SCORE));
                            }
                            check_pipeline(&p, available.clone(), false)?;
                        }
                    }
                }
            }
            "$lookup" | "$unionWith" | "$redact" => {}
            _ => {
                let mut read = Vec::new();
                metas_read(arg, &mut read);
                // The `$match` with the `$text` reads the score as nothing (measured on 9.0.4).
                if text_match(*st) {
                    read.retain(|n| n != TEXT_SCORE);
                }
                for r in &read {
                    if crate::expr::meta_is_required(r) && !available.contains(&r.as_str()) {
                        let mut e = crate::expr::missing_meta(r);
                        if leading && name == "$sort" {
                            e.message = format!("{DISTINCT_SCAN_CHECK}{}", e.message);
                        }
                        return Err(e);
                    }
                }
            }
        }
        leading = leading && matches!(name, "$match" | "$sort");
        let given = given_by(name, arg);
        texted |= given.contains(&TEXT_SCORE);
        available.extend(given);
        if DROPPING.contains(&name) {
            // The scores of a search stage and of a `$text` stay readable, as nothing (measured
            // on 9.0.4 after `$group`); the other metadata goes.
            available.retain(|n| [SEARCH_SCORE, VECTOR_SEARCH_SCORE, TEXT_SCORE].contains(n));
        }
        if name == "$unionWith" {
            // The documents keep their metadata and those it adds bring their own, missing
            // where its pipeline gives none (measured on 9.0.4); this engine gives no
            // `$geoNear` metadata to read then.
            available.retain(|n| !n.starts_with("geoNear"));
        }
    }
    Ok(())
}

/// Whether stage `st` is a `$match` with `$text`.
fn text_match(st: RawValue<'_>) -> bool {
    matches!(st, RawValue::Document(d)
        if matches!(d.get("$match"), Some(RawValue::Document(m)) if super::has_text(m)))
}

/// The metadata names `v` reads with `$meta` (anywhere in a stage's specification).
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

#[cfg(test)]
mod env_tests {
    use super::*;
    use crate::reference::Reference;

    /// What `env` answers, to compare with the environment it wraps.
    fn answers(env: &dyn Env) -> String {
        let _ = env.ext();
        let _ = env.collation();
        let none = Document::new();
        format!(
            "{:?} {:?} {:?} {} {} {:?} {:?} {:?} {} {} {} {} {} {:?} {:?} {:?} {:?}",
            env.vars(),
            env.collection("c"),
            env.collection_matching("c", &none),
            env.operation_memory_limit(),
            env.random(),
            env.system_stage("$x", &none).is_ok(),
            env.check_search("$search", &none).is_ok(),
            env.search_source("$search", &none).is_ok(),
            env.geo_indexes().is_ok(),
            env.js().is_none(),
            env.allow_disk_use(),
            env.stage_memory_limit(),
            env.spill().is_none(),
            env.text_source(&none).map_err(|e| e.code.code),
            env.joined_text_source("c", &none, None).map_err(|e| e.code.code),
            env.joined_search_source("c", ("$search", &none), true).map_err(|e| e.code.code),
            env.scope(&none).root,
        )
    }

    fn reference() -> Reference {
        Reference::new(7).with_collection("c", vec![Document::new().with("a", 1)])
    }

    /// The metadata environments pass every question on to the one they wrap.
    #[test]
    fn wrapped_environments_answer_as_their_inner_one() {
        let direct = answers(&reference());
        let inner = reference();
        let names = vec!["searchScore".to_owned()];
        let required = RequiredEnv { inner: &inner, names: &names, vars: None };
        assert_eq!(answers(&required), direct);
        let inner = reference();
        let meta = Meta::new();
        let wrapped = MetaEnv { inner: &inner, meta: &meta, marked: &[] };
        assert_eq!(answers(&wrapped), direct);
        // And the environment of a joined pipeline, whose `$text` and search stages read the
        // joined collection's indexes, which a reference without a node has not.
        let inner = reference();
        let joined = super::super::joined::Joined { inner: &inner, coll: "c", from_lookup: true };
        assert_eq!(answers(&joined), direct);
        // And the environment of `$lookup`'s `let`, which only adds its variables.
        let inner = reference();
        let vars = super::super::joined::LetVars::default();
        let with_vars = super::super::joined::WithVars { inner: &inner, vars: &vars };
        assert_eq!(with_vars.vars(), Some(&Document::new()));
        let set = format!("{:?} ", Some(&Document::new()));
        let answered = answers(&with_vars);
        let rest = direct.strip_prefix("None ").expect("the reference has no variables");
        assert_eq!(answered.strip_prefix(&set), Some(rest));
    }

    /// Malformed stages are left to the stage parsers, which run after the check.
    #[test]
    fn malformed_stages_are_left_to_their_parsers() {
        // The stages as the values of one document, read back as raw values.
        let holder = Document::new()
            .with("0", 5)
            .with("1", Document::new())
            .with("2", Document::new().with("$facet", 5))
            .with("3", Document::new().with("$facet", Document::new().with("a", 5)));
        let bytes = crate::raw::bytes_of(&holder).expect("bytes");
        let raw = teseladb_bson::RawDoc::from_bytes(&bytes).expect("raw");
        let list: Vec<RawValue<'_>> = raw.iter().map(|(_, v)| v).collect();
        assert!(check_joined(&list).is_ok());
    }
}
