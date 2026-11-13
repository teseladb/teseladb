//! The other stages of a pipeline (measured on a `refrouter` 9.0.4): `$match`, `$limit`, `$skip`,
//! `$count`, `$unwind`, `$sample`, `$redact`, `$facet`, `$lookup`, `$unionWith` and
//! `$documents`.

use std::sync::Arc;

use teseladb_mql::{ErrorCode, Filter, FilterUse, MqlError};
use teseladb_bson::{Document, RawDoc, RawValue, Value};

use super::errors::{
    BAD_VALUE, COUNT_DOLLAR, COUNT_DOT, COUNT_EMPTY, COUNT_ID, COUNT_TYPE, FACET_ARRAY,
    FACET_EMPTY, FACET_STAGE, FAILED_TO_PARSE, LIMIT_ARGUMENT, LIMIT_ZERO, MATCH_NOT_OBJECT,
    MISSING_FIELD, NEAR_IN_MATCH, REDACT_RESULT, SAMPLE_MISSING, SAMPLE_SIZE, SKIP_ARGUMENT,
    TEXT_IN_FACET, TEXT_NOT_FIRST, UNKNOWN_FIELD, UNWIND_INDEX_PREFIX, UNWIND_NO_PATH,
    UNWIND_OPTION, UNWIND_PREFIX, UNWIND_PRESERVE, error,
};
use super::joined::{Joined, LetVars, WithVars};
use super::sort::check_path;
use super::{Env, Pipeline};
use crate::expr::arith::raw_type_name;
use crate::expr::values::display;
use crate::expr::{Ctx, Expr, Scope};

/// RefDb's message for a geospatial sort inside `$match`.
const NEAR_MESSAGE: &str = "$geoNear, $near, and $nearSphere are not allowed in this context, as these operators require sorting geospatial data. If you do not need sort, consider using $geoWithin instead. Check out https://dochub.refdb.org/core/near-sort-operation and https://dochub.refdb.org/core/nearSphere-sort-operationfor more details.";

use super::scoped::matches;

/// A `$match`.
#[derive(Clone, Debug)]
pub struct Match {
    filter: Arc<Filter>,
}

/// Whether filter `d` names `$where` among its clauses.
fn uses_where(d: RawDoc<'_>) -> bool {
    d.iter().any(|(k, v)| match (k, v) {
        ("$where", _) => true,
        ("$and" | "$or" | "$nor", RawValue::Array(a)) => {
            a.iter().any(|x| matches!(x, RawValue::Document(c) if uses_where(c)))
        }
        _ => false,
    })
}

impl Match {
    /// Reads a `$match`; the expressions of its `$expr`s are checked here, as RefDb parses them
    /// with the stage.
    pub fn parse(v: RawValue<'_>, ctx: &Ctx) -> Result<Match, MqlError> {
        let RawValue::Document(d) = v else {
            return Err(error(
                MATCH_NOT_OBJECT,
                "the match filter must be an expression in an object",
            ));
        };
        if !ctx.allows_where() && uses_where(d) {
            return Err(error(BAD_VALUE, "$where is not allowed in this context"));
        }
        let filter = Filter::parse(d, FilterUse::Query)?;
        if filter.uses_near() {
            return Err(error(NEAR_IN_MATCH, NEAR_MESSAGE));
        }
        check_exprs(d, ctx)?;
        Ok(Match { filter: Arc::new(filter) })
    }

    /// The `$match` of a filter already read.
    pub(crate) fn of(filter: Filter) -> Match {
        Match { filter: Arc::new(filter) }
    }

    /// The documents of `docs` that match, in their order: batches of up to
    /// [`super::BATCH_DOCS`] documents, each filtered 64 at a time by the register machine
    /// (010 FR-028, FR-031).
    pub fn run(&self, docs: Vec<Document>, env: &dyn Env) -> Result<Vec<Document>, MqlError> {
        let mut out = Vec::with_capacity(docs.len());
        let mut masks = Vec::with_capacity(super::BATCH_DOCS / teseladb_mql::vm::BATCH);
        let mut docs = docs.into_iter().peekable();
        let mut batch: Vec<Document> = Vec::with_capacity(super::BATCH_DOCS);
        while docs.peek().is_some() {
            batch.extend(docs.by_ref().take(super::BATCH_DOCS));
            masks.clear();
            for lanes in batch.chunks(teseladb_mql::vm::BATCH) {
                masks.push(super::scoped::matches_batch(&self.filter, lanes, env)?);
            }
            for (i, d) in batch.drain(..).enumerate() {
                let mask = masks[i / teseladb_mql::vm::BATCH];
                if mask >> (i % teseladb_mql::vm::BATCH) & 1 == 1 {
                    out.push(d);
                }
            }
        }
        Ok(out)
    }
}

/// Parses the `$expr`s of filter `d` (its top level and its `$and`, `$or` and `$nor`) for their
/// errors.
pub fn check_exprs(d: RawDoc<'_>, ctx: &Ctx) -> Result<(), MqlError> {
    for (k, v) in d.iter() {
        match (k, v) {
            ("$expr", e) => {
                Expr::parse_in(e, ctx)?;
            }
            ("$and" | "$or" | "$nor", RawValue::Array(a)) => {
                for x in a.iter() {
                    if let RawValue::Document(f) = x {
                        check_exprs(f, ctx)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The argument of `$limit` or `$skip` (`stage`): a whole number of 64 bits.
fn count_argument(v: RawValue<'_>, stage: &str, code: ErrorCode) -> Result<u64, MqlError> {
    let shown = display(&Value::from(v));
    let bad = |why: &str| {
        error(code, format!("invalid argument to {stage} stage: {why}{stage}: {shown}"))
    };
    // The least double past the 64-bit integers, 2^63.
    const PAST_I64: f64 = 9_223_372_036_854_775_808.0;
    // Why a number that is no 64-bit integer is refused.
    let not_whole = |x: f64| -> MqlError {
        if x.is_nan() {
            bad("Expected an integer, but found NaN in: ")
        } else if !(-PAST_I64..PAST_I64).contains(&x) {
            bad("Cannot represent as a 64-bit integer: ")
        } else {
            bad("Expected an integer: ")
        }
    };
    let n: i64 = match v {
        RawValue::Int32(n) => i64::from(n),
        RawValue::Int64(n) => n,
        RawValue::Double(x) if x.fract() == 0.0 && (-PAST_I64..PAST_I64).contains(&x) => x as i64,
        RawValue::Double(x) => return Err(not_whole(x)),
        RawValue::Decimal128(d) => match teseladb_mql::decimal_exact(d) {
            Some(n) => n,
            None => return Err(not_whole(teseladb_mql::decimal_to_f64(d))),
        },
        _ => return Err(bad("Expected a number in: ")),
    };
    u64::try_from(n).map_err(|_| bad("Expected a non-negative number in: "))
}

/// The argument of `$limit`, which may not be 0.
pub fn limit(v: RawValue<'_>) -> Result<u64, MqlError> {
    let n = count_argument(v, "$limit", LIMIT_ARGUMENT)?;
    if n == 0 {
        return Err(error(LIMIT_ZERO, "the limit must be positive"));
    }
    Ok(n)
}

/// The argument of `$skip`.
pub fn skip(v: RawValue<'_>) -> Result<u64, MqlError> {
    count_argument(v, "$skip", SKIP_ARGUMENT)
}

/// The field of `$count`.
pub fn count(v: RawValue<'_>) -> Result<String, MqlError> {
    let RawValue::String(s) = v else {
        return Err(error(COUNT_TYPE, "the count field must be a non-empty string"));
    };
    if s.is_empty() {
        return Err(error(COUNT_EMPTY, "the count field must be a non-empty string"));
    }
    if s.starts_with('$') {
        return Err(error(COUNT_DOLLAR, "the count field cannot be a $-prefixed path"));
    }
    if s.contains('.') {
        return Err(error(COUNT_DOT, "the count field cannot contain '.'"));
    }
    if s == "_id" {
        return Err(error(COUNT_ID, "the count field cannot be '_id'"));
    }
    Ok(s.to_owned())
}

/// The result of `$count` over `n` documents: none for none.
pub fn run_count(field: &str, n: usize) -> Vec<Document> {
    if n == 0 {
        return Vec::new();
    }
    let v = i32::try_from(n).map_or(Value::Int64(n as i64), Value::Int32);
    vec![Document::new().with(field, v)]
}

/// An `$unwind`.
#[derive(Clone, Debug)]
pub struct Unwind {
    path: Vec<String>,
    index: Option<String>,
    preserve: bool,
}

impl Unwind {
    /// Reads an `$unwind`: `"$path"` or `{path, includeArrayIndex, preserveNullAndEmptyArrays}`.
    pub fn parse(v: RawValue<'_>) -> Result<Unwind, MqlError> {
        let (mut path, mut index, mut preserve) = (None, None, false);
        match v {
            RawValue::String(s) => path = Some(s.to_owned()),
            RawValue::Document(d) => {
                for (k, x) in d.iter() {
                    match (k, x) {
                        ("path", RawValue::String(s)) => path = Some(s.to_owned()),
                        ("includeArrayIndex", RawValue::String(s)) => {
                            if s.starts_with('$') {
                                return Err(error(
                                    UNWIND_INDEX_PREFIX,
                                    format!(
                                        "includeArrayIndex option to $unwind stage should not be prefixed with a '$': {s}"
                                    ),
                                ));
                            }
                            index = Some(s.to_owned());
                        }
                        ("preserveNullAndEmptyArrays", RawValue::Boolean(b)) => preserve = b,
                        ("preserveNullAndEmptyArrays", other) => {
                            return Err(error(
                                UNWIND_PRESERVE,
                                format!(
                                    "expected a boolean for the preserveNullAndEmptyArrays option to $unwind stage, got {}",
                                    raw_type_name(other)
                                ),
                            ));
                        }
                        (other, _) => {
                            return Err(error(
                                UNWIND_OPTION,
                                format!("unrecognized option to $unwind stage: {other}"),
                            ));
                        }
                    }
                }
            }
            _ => {
                return Err(error(
                    super::errors::UNWIND_NOT_OBJECT,
                    format!(
                        "expected either a string or an object as specification for $unwind stage, got {}",
                        crate::expr::arith::raw_type_name(v)
                    ),
                ));
            }
        }
        let path =
            path.ok_or_else(|| error(UNWIND_NO_PATH, "no path specified to $unwind stage"))?;
        let Some(p) = path.strip_prefix('$') else {
            return Err(error(
                UNWIND_PREFIX,
                format!("path option to $unwind stage should be prefixed with a '$': {path}"),
            ));
        };
        check_path(p)?;
        Ok(Unwind { path: p.split('.').map(str::to_owned).collect(), index, preserve })
    }

    /// [`Unwind::run`], asking `env` whether to stop before each document it spreads.
    pub fn run_checked(
        &self,
        docs: Vec<Document>,
        env: &dyn Env,
    ) -> Result<Vec<Document>, MqlError> {
        let mut out = Vec::new();
        for d in docs {
            env.interrupted()?;
            out.extend(self.run(vec![d]));
        }
        Ok(out)
    }

    /// One document per element of the array at the path; any other value stays as it is.
    pub fn run(&self, docs: Vec<Document>) -> Vec<Document> {
        let segs: Vec<&str> = self.path.iter().map(String::as_str).collect();
        let mut out = Vec::new();
        for d in docs {
            match get_path(&d, &segs) {
                Some(Value::Array(items)) if !items.is_empty() => {
                    for (i, x) in items.into_iter().enumerate() {
                        let mut e = d.clone();
                        put_path(&mut e, &segs, Some(x));
                        if let Some(f) = &self.index {
                            e.insert(f.clone(), Value::Int64(i as i64));
                        }
                        out.push(e);
                    }
                }
                Some(Value::Array(_)) => {
                    if self.preserve {
                        let mut e = d;
                        put_path(&mut e, &segs, None);
                        self.with_null_index(&mut e);
                        out.push(e);
                    }
                }
                None | Some(Value::Null) => {
                    if self.preserve {
                        let mut e = d;
                        self.with_null_index(&mut e);
                        out.push(e);
                    }
                }
                Some(_) => {
                    let mut e = d;
                    self.with_null_index(&mut e);
                    out.push(e);
                }
            }
        }
        out
    }

    fn with_null_index(&self, d: &mut Document) {
        if let Some(f) = &self.index {
            d.insert(f.clone(), Value::Null);
        }
    }
}

/// The value at `segs` through documents only.
fn get_path(d: &Document, segs: &[&str]) -> Option<Value> {
    let (first, rest) = segs.split_first()?;
    let v = d.get(first)?;
    if rest.is_empty() {
        return Some(v.clone());
    }
    match v {
        Value::Document(sub) => get_path(sub, rest),
        _ => None,
    }
}

/// Sets (or removes) the value at `segs`, through documents.
fn put_path(d: &mut Document, segs: &[&str], v: Option<Value>) {
    let Some((first, rest)) = segs.split_first() else { return };
    if rest.is_empty() {
        match v {
            Some(v) => {
                d.insert(*first, v);
            }
            None => {
                d.remove(first);
            }
        }
        return;
    }
    let mut sub = match d.get(first) {
        Some(Value::Document(s)) => s.clone(),
        _ => Document::new(),
    };
    put_path(&mut sub, rest, v);
    d.insert(*first, Value::Document(sub));
}

/// A `$sample`.
#[derive(Clone, Debug)]
pub struct Sample {
    size: usize,
}

impl Sample {
    /// Reads `{size: n}`.
    pub fn parse(v: RawValue<'_>) -> Result<Sample, MqlError> {
        let RawValue::Document(d) = v else {
            return Err(error(
                super::errors::SAMPLE_NOT_OBJECT,
                "the $sample stage specification must be an object",
            ));
        };
        let mut size = None;
        for (k, x) in d.iter() {
            match k {
                "size" => {
                    let n = crate::expr::values::int64_of(&Value::from(x))
                        .filter(|n| *n > 0)
                        .and_then(|n| usize::try_from(n).ok());
                    size = Some(n.ok_or_else(|| {
                        error(SAMPLE_SIZE, "size argument to $sample must be a positive integer")
                    })?);
                }
                other => {
                    return Err(error(
                        UNKNOWN_FIELD,
                        format!("unrecognized option to $sample: {other}"),
                    ));
                }
            }
        }
        let size =
            size.ok_or_else(|| error(SAMPLE_MISSING, "$sample stage must specify a size"))?;
        Ok(Sample { size })
    }

    /// `size` documents of `docs` at random, in random order.
    pub fn run(&self, docs: Vec<Document>, env: &dyn Env) -> Vec<Document> {
        self.pick(docs.len(), env).into_iter().map(|(i, _)| docs[i].clone()).collect()
    }

    /// The places of the documents a sample of `n` takes and the random value of each, as
    /// RefDb samples: each document gets a random value, and the `size` largest go first,
    /// largest first (measured on a `refrouter` 9.0.4: `{$meta: "randVal"}` reads that value and
    /// descends through the sample).
    pub(crate) fn pick(&self, n: usize, env: &dyn Env) -> Vec<(usize, f64)> {
        let mut keyed: Vec<(usize, f64)> = (0..n).map(|i| (i, env.random())).collect();
        keyed.sort_by(|a, b| b.1.total_cmp(&a.1));
        keyed.truncate(self.size);
        keyed
    }
}

/// The values `$redact`'s expression may give (`$$KEEP`, `$$PRUNE`, `$$DESCEND`).
const KEEP: &str = "keep";
const PRUNE: &str = "prune";
const DESCEND: &str = "descend";

/// The documents of `$redact`: each level kept, pruned or descended into.
pub fn run_redact(e: &Expr, docs: Vec<Document>, env: &dyn Env) -> Result<Vec<Document>, MqlError> {
    let mut out = Vec::new();
    for d in docs {
        let mut scope = env.scope(&d);
        for (name, v) in [("KEEP", KEEP), ("PRUNE", PRUNE), ("DESCEND", DESCEND)] {
            scope.vars.insert(name.to_owned(), Some(Value::String(v.to_owned())));
        }
        if let Some(r) = redact(e, &d, &scope)? {
            out.push(r);
        }
    }
    Ok(out)
}

fn redact(e: &Expr, d: &Document, scope: &Scope) -> Result<Option<Document>, MqlError> {
    match e.eval(d, scope)? {
        Some(Value::String(s)) if s == KEEP => Ok(Some(d.clone())),
        Some(Value::String(s)) if s == PRUNE => Ok(None),
        Some(Value::String(s)) if s == DESCEND => {
            let mut out = Document::new();
            for (k, v) in d.iter() {
                match v {
                    Value::Document(sub) => {
                        if let Some(r) = redact(e, sub, scope)? {
                            out.push(k, Value::Document(r));
                        }
                    }
                    Value::Array(items) => {
                        out.push(k, Value::Array(redact_array(e, items, scope)?))
                    }
                    other => out.push(k, other.clone()),
                }
            }
            Ok(Some(out))
        }
        other => Err(error(
            REDACT_RESULT,
            format!(
                "$redact's expression should not return anything aside from the variables $$KEEP, $$DESCEND, and $$PRUNE, but returned {}",
                crate::expr::values::display_eval(&other)
            ),
        )),
    }
}

fn redact_array(e: &Expr, items: &[Value], scope: &Scope) -> Result<Vec<Value>, MqlError> {
    let mut out = Vec::new();
    for v in items {
        match v {
            Value::Document(sub) => {
                if let Some(r) = redact(e, sub, scope)? {
                    out.push(Value::Document(r));
                }
            }
            Value::Array(inner) => out.push(Value::Array(redact_array(e, inner, scope)?)),
            other => out.push(other.clone()),
        }
    }
    Ok(out)
}

/// The stages a `$facet` may not have.
const NOT_IN_FACET: [&str; 11] = [
    "$documents",
    "$out",
    "$merge",
    "$facet",
    "$collStats",
    "$indexStats",
    "$geoNear",
    "$search",
    "$searchMeta",
    "$vectorSearch",
    "$changeStream",
];

/// A `$facet`.
#[derive(Clone, Debug)]
pub struct Facet {
    facets: Vec<(String, Pipeline)>,
}

/// RefDb's error for a `$match` with `$text` that is not the first stage of its pipeline.
pub(crate) fn text_not_first() -> MqlError {
    error(TEXT_NOT_FIRST, "$match with $text is only allowed as the first pipeline stage")
}

/// RefDb's error for a `$match` with `$text` in a `$facet`, which has no text index to read
/// (measured on 9.0.4, 011 T054).
pub(crate) fn text_in_facet() -> MqlError {
    error(TEXT_IN_FACET, "Should never call getNext on a $match stage with $text clause")
}

impl Facet {
    /// Reads `{name: [stages]}`.
    pub fn parse(v: RawValue<'_>, ctx: &Ctx) -> Result<Facet, MqlError> {
        let shown = display(&Value::from(v));
        let empty = || {
            error(
                FACET_EMPTY,
                format!(
                    "the $facet specification must be a non-empty object, but found: $facet: {shown}"
                ),
            )
        };
        let RawValue::Document(d) = v else { return Err(empty()) };
        let mut facets = Vec::new();
        for (k, x) in d.iter() {
            check_path(k)?;
            let RawValue::Array(a) = x else {
                return Err(error(
                    FACET_ARRAY,
                    format!("arguments to $facet must be arrays, {k} is type {}", raw_type_name(x)),
                ));
            };
            let stages: Vec<RawValue<'_>> = a.iter().collect();
            for s in &stages {
                if let RawValue::Document(sd) = s
                    && let Some((name, _)) = sd.iter().next()
                    && NOT_IN_FACET.contains(&name)
                {
                    return Err(error(
                        FACET_STAGE,
                        format!("{name} is not allowed to be used within a $facet stage"),
                    ));
                }
            }
            facets.push((k.to_owned(), Pipeline::parse(&stages, ctx)?));
        }
        if facets.is_empty() {
            return Err(empty());
        }
        Ok(Facet { facets })
    }

    /// Its facets: each name with its pipeline.
    pub(super) fn facets(&self) -> &[(String, Pipeline)] {
        &self.facets
    }

    /// Whether one of its pipelines starts with a `$match` with `$text`.
    pub(crate) fn has_text(&self) -> bool {
        self.facets.iter().any(|(_, p)| p.starts_with_text())
    }

    /// [`Facet::run`] over documents with metadata, which each facet's stages see.
    pub(crate) fn run_with_meta(
        &self,
        docs: Vec<Document>,
        metas: Vec<super::meta::Meta>,
        env: &dyn Env,
    ) -> Result<Vec<Document>, MqlError> {
        if self.has_text() {
            return Err(text_in_facet());
        }
        let mut out = Document::new();
        for (name, p) in &self.facets {
            let (results, _) = p.run_with_meta(docs.clone(), Some(metas.clone()), env, false)?;
            out.push(
                name.clone(),
                Value::Array(results.into_iter().map(Value::Document).collect()),
            );
        }
        Ok(vec![out])
    }

    /// One document with each facet's results.
    pub fn run(&self, docs: Vec<Document>, env: &dyn Env) -> Result<Vec<Document>, MqlError> {
        if self.has_text() {
            return Err(text_in_facet());
        }
        let mut out = Document::new();
        for (name, p) in &self.facets {
            let results = p.run_in(docs.clone(), env, false)?;
            out.push(
                name.clone(),
                Value::Array(results.into_iter().map(Value::Document).collect()),
            );
        }
        Ok(vec![out])
    }
}

/// A `$lookup`.
#[derive(Clone, Debug)]
pub struct Lookup {
    from: Option<String>,
    local: Option<String>,
    foreign: Option<String>,
    as_path: Vec<String>,
    lets: Vec<(String, Expr)>,
    pipeline: Option<Pipeline>,
    /// The error its pipeline gives when it first runs (metadata no stage gives).
    joined_error: Option<MqlError>,
}

impl Lookup {
    /// Whether it joins by `localField` and `foreignField` alone, with no or an empty pipeline.
    pub(super) fn is_equality(&self) -> bool {
        self.local.is_some() && self.pipeline.as_ref().is_none_or(Pipeline::is_empty)
    }

    /// Reads a `$lookup`.
    pub fn parse(v: RawValue<'_>, ctx: &Ctx) -> Result<Lookup, MqlError> {
        let RawValue::Document(d) = v else {
            return Err(error(
                FAILED_TO_PARSE,
                format!(
                    "the $lookup stage specification must be an object, but found {}",
                    crate::expr::arith::raw_type_name(v)
                ),
            ));
        };
        let (mut from, mut local, mut foreign, mut as_name) = (None, None, None, None);
        let (mut lets, mut stages) = (Vec::new(), None);
        for (k, x) in d.iter() {
            match (k, x) {
                ("from", RawValue::String(s)) => from = Some(s.to_owned()),
                ("localField", RawValue::String(s)) => local = Some(s.to_owned()),
                ("foreignField", RawValue::String(s)) => foreign = Some(s.to_owned()),
                ("as", RawValue::String(s)) => as_name = Some(s.to_owned()),
                ("let", RawValue::Document(l)) => {
                    for (n, e) in l.iter() {
                        lets.push((n.to_owned(), Expr::parse_in(e, ctx)?));
                    }
                }
                ("pipeline", RawValue::Array(a)) => stages = Some(a.iter().collect::<Vec<_>>()),
                (other, _) => {
                    return Err(error(
                        UNKNOWN_FIELD,
                        format!("unknown argument to $lookup: {other}"),
                    ));
                }
            }
        }
        if from.is_none() && stages.is_none() {
            return Err(error(FAILED_TO_PARSE, "must specify 'pipeline' when 'from' is empty"));
        }
        if stages.is_none() && (local.is_none() || foreign.is_none()) {
            return Err(error(
                FAILED_TO_PARSE,
                "$lookup requires either 'pipeline' or both 'localField' and 'foreignField' to be specified",
            ));
        }
        let as_name = as_name.ok_or_else(|| {
            error(MISSING_FIELD, "BSON field '$lookup.as' is missing but a required field")
        })?;
        check_path(&as_name)?;
        let inner = ctx.with(lets.iter().map(|(n, _)| n.as_str()));
        super::out_merge::refuse_in(stages.as_deref().unwrap_or_default(), "$lookup")?;
        let mut joined_error = None;
        let pipeline = match (stages, &from) {
            (Some(s), Some(_)) => {
                joined_error = super::meta::check_joined(&s)
                    .and_then(|()| super::sort::check_random_sort(&s))
                    .err();
                let p = Pipeline::parse(&s, &inner)?;
                // The equality of `localField` and `foreignField` goes before the pipeline's
                // stages, and its `$text` is no longer first (measured on 9.0.4).
                if local.is_some() && p.starts_with_text() {
                    joined_error = Some(text_not_first());
                }
                Some(p)
            }
            (Some(s), None) => {
                super::meta::check_available(&s)?;
                Some(Pipeline::parse_from_documents(&s, &inner, "$lookup")?)
            }
            (None, _) => None,
        };
        let as_path = as_name.split('.').map(str::to_owned).collect();
        Ok(Lookup { from, local, foreign, as_path, lets, pipeline, joined_error })
    }

    /// The filter of the foreign documents an equality join of `docs` can reach (010 FR-009,
    /// T023): those whose field is one of the local values, read through the foreign
    /// collection's indexes. `None` where it would not be a superset of the join: a local value
    /// that is missing, `null`, `undefined`, a regular expression or an empty array, or a
    /// collation other than the binary one.
    fn wanted(&self, docs: &[Document], env: &dyn Env) -> Option<Document> {
        let (Some(_), Some(local), Some(foreign)) = (&self.from, &self.local, &self.foreign) else {
            return None;
        };
        if env.collation().get().is_some() {
            return None;
        }
        let segs: Vec<&str> = local.split('.').collect();
        let mut values: Vec<Value> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for d in docs {
            let mut own = Vec::new();
            collect_local(&Value::Document(d.clone()), &segs, &mut own);
            let unreachable = own.is_empty()
                || own
                    .iter()
                    .any(|v| matches!(v, Value::Null | Value::Undefined | Value::Regex { .. }));
            if unreachable {
                return None;
            }
            for v in own {
                if seen.insert(super::group_key(&v, None)) {
                    values.push(v);
                }
            }
        }
        let within = Document::new().with("$in", Value::Array(values));
        Some(Document::new().with(foreign.as_str(), within))
    }

    /// Each document with the documents it joins.
    pub fn run(&self, docs: Vec<Document>, env: &dyn Env) -> Result<Vec<Document>, MqlError> {
        if let Some(e) = self.joined_error.as_ref().filter(|_| !docs.is_empty()) {
            return Err(e.clone());
        }
        // A pipeline whose first stage reads no input (a `$text` reads the text index) needs
        // no foreign documents.
        let unread = self.pipeline.as_ref().is_some_and(|p| !p.reads_collection());
        let foreign_docs = match (&self.from, self.wanted(&docs, env)) {
            (Some(_), _) if unread => Vec::new(),
            (Some(f), Some(filter)) => env.collection_matching(f, &filter)?,
            (Some(f), None) => env.collection(f)?,
            (None, _) => Vec::new(),
        };
        let segs: Vec<&str> = self.as_path.iter().map(String::as_str).collect();
        // The foreign documents by the values of their field, read once for every document.
        let by_value = match (&self.from, &self.foreign) {
            (Some(_), Some(f)) => Some(ForeignIndex::new(&foreign_docs, f, env)),
            _ => None,
        };
        let mut out = Vec::with_capacity(docs.len());
        for d in docs {
            env.interrupted()?;
            // Without `from`, the equality applies to what the pipeline's `$documents` gives,
            // before its other stages (measured on a `refrouter` 9.0.4).
            let join = match (&self.local, &self.foreign) {
                (Some(l), Some(f)) => Some((l, f)),
                _ => None,
            };
            let mut joined = match join {
                Some((l, f)) if self.from.is_some() => match &by_value {
                    Some(index) => index.join(&d, (l, f), &foreign_docs, env)?,
                    None => equal_join(&d, l, f, &foreign_docs, env)?,
                },
                _ => foreign_docs.clone(),
            };
            if let Some(p) = &self.pipeline {
                let scope = env.scope(&d);
                let mut vars =
                    LetVars { set: env.vars().cloned().unwrap_or_default(), missing: Vec::new() };
                for (n, e) in &self.lets {
                    match e.eval(&d, &scope)? {
                        Some(v) => {
                            vars.set.insert(n.clone(), v);
                        }
                        None => vars.missing.push(n.clone()),
                    }
                }
                let with_vars = WithVars { inner: env, vars: &vars };
                joined = match (join, &self.from) {
                    (Some((l, f)), None) => {
                        p.run_split(joined, &with_vars, |docs| equal_join(&d, l, f, &docs, env))?
                    }
                    // The equality applies to what a first search stage gives, before the other
                    // stages (measured on 9.0.4).
                    (Some((l, f)), Some(coll)) if p.starts_with_search() => {
                        let p = p.with_match_after_first(Match::of(equality(&d, l, f)?));
                        p.run_in(
                            joined,
                            &Joined { inner: &with_vars, coll, from_lookup: true },
                            false,
                        )?
                    }
                    (_, Some(coll)) => p.run_in(
                        joined,
                        &Joined { inner: &with_vars, coll, from_lookup: true },
                        false,
                    )?,
                    (None, None) => p.run_in(joined, &with_vars, false)?,
                };
            }
            let mut e = d;
            let found = Value::Array(joined.into_iter().map(Value::Document).collect());
            super::shape_set(&mut e, &segs, found);
            out.push(e);
        }
        Ok(out)
    }
}

/// The foreign documents of an equality `$lookup` by the values their field reaches (each element
/// of an array and the array itself), as group keys in the command's collation (010 FR-009,
/// T023): a document's candidates are those of its local values, then the equality decides.
struct ForeignIndex {
    by_key: std::collections::HashMap<Vec<u8>, Vec<usize>>,
}

impl ForeignIndex {
    fn new(docs: &[Document], foreign: &str, env: &dyn Env) -> ForeignIndex {
        let collation = env.collation();
        let segs: Vec<&str> = foreign.split('.').collect();
        let mut by_key: std::collections::HashMap<Vec<u8>, Vec<usize>> =
            std::collections::HashMap::new();
        for (i, d) in docs.iter().enumerate() {
            let mut values = Vec::new();
            collect_foreign(&Value::Document(d.clone()), &segs, &mut values);
            for v in values {
                let list = by_key.entry(super::group_key(&v, collation.get())).or_default();
                if list.last() != Some(&i) {
                    list.push(i);
                }
            }
        }
        ForeignIndex { by_key }
    }

    /// The foreign documents that join `d`, in their order. A local value the index cannot look
    /// up (`null`, which also matches a missing field, or a regular expression) reads them all.
    fn join(
        &self,
        d: &Document,
        (local, foreign): (&str, &str),
        docs: &[Document],
        env: &dyn Env,
    ) -> Result<Vec<Document>, MqlError> {
        let mut values: Vec<Value> = Vec::new();
        let segs: Vec<&str> = local.split('.').collect();
        collect_local(&Value::Document(d.clone()), &segs, &mut values);
        let unindexed = values.is_empty()
            || values
                .iter()
                .any(|v| matches!(v, Value::Null | Value::Undefined | Value::Regex { .. }));
        if unindexed {
            return equal_join(d, local, foreign, docs, env);
        }
        let collation = env.collation();
        let mut picked: Vec<usize> = values
            .iter()
            .flat_map(|v| {
                self.by_key.get(&super::group_key(v, collation.get())).cloned().unwrap_or_default()
            })
            .collect();
        picked.sort_unstable();
        picked.dedup();
        let candidates: Vec<Document> = picked.into_iter().map(|i| docs[i].clone()).collect();
        equal_join(d, local, foreign, &candidates, env)
    }
}

/// The values `segs` reaches in a foreign document: each array on the way crossed, and at the
/// end an array itself and each of its elements; a numeric segment also picks an element.
fn collect_foreign(v: &Value, segs: &[&str], out: &mut Vec<Value>) {
    let Some((first, rest)) = segs.split_first() else {
        if let Value::Array(items) = v {
            out.extend(items.iter().cloned());
        }
        out.push(v.clone());
        return;
    };
    match v {
        Value::Document(d) => {
            if let Some(x) = d.get(first) {
                collect_foreign(x, rest, out);
            }
        }
        Value::Array(items) => {
            if let Ok(i) = first.parse::<usize>()
                && let Some(x) = items.get(i)
            {
                collect_foreign(x, rest, out);
            }
            for x in items {
                collect_foreign(x, segs, out);
            }
        }
        _ => {}
    }
}

/// The foreign documents whose `foreign` field equals a value of `local` in `d` (an array's
/// elements; a missing field or an empty array as `null`).
fn equal_join(
    d: &Document,
    local: &str,
    foreign: &str,
    docs: &[Document],
    env: &dyn Env,
) -> Result<Vec<Document>, MqlError> {
    let filter = equality(d, local, foreign)?;
    let mut out = Vec::new();
    for f in docs {
        if matches(&filter, f, env)? {
            out.push(f.clone());
        }
    }
    Ok(out)
}

/// The filter of the foreign documents whose `foreign` field equals a value of `local` in `d`.
fn equality(d: &Document, local: &str, foreign: &str) -> Result<Filter, MqlError> {
    let mut values: Vec<Value> = Vec::new();
    let segs: Vec<&str> = local.split('.').collect();
    collect_local(&Value::Document(d.clone()), &segs, &mut values);
    if values.is_empty() {
        values.push(Value::Null);
    }
    let spec = Document::new().with(foreign, Document::new().with("$in", Value::Array(values)));
    crate::raw::with_raw_doc(&spec, |raw| Filter::parse(raw, FilterUse::Query))
}

/// The values a local path reaches: through documents and arrays of documents, an array's
/// elements at the end.
fn collect_local(v: &Value, segs: &[&str], out: &mut Vec<Value>) {
    let Some((first, rest)) = segs.split_first() else {
        match v {
            Value::Array(items) => out.extend(items.iter().cloned()),
            other => out.push(other.clone()),
        }
        return;
    };
    match v {
        Value::Document(d) => {
            if let Some(x) = d.get(first) {
                collect_local(x, rest, out);
            }
        }
        Value::Array(items) => {
            for x in items {
                if matches!(x, Value::Document(_)) {
                    collect_local(x, segs, out);
                }
            }
        }
        _ => {}
    }
}

/// A `$unionWith`.
#[derive(Clone, Debug)]
pub struct UnionWith {
    coll: Option<String>,
    pipeline: Pipeline,
    /// The error its pipeline gives when it runs (metadata no stage gives).
    joined_error: Option<MqlError>,
}

impl UnionWith {
    /// Reads `"coll"` or `{coll, pipeline}`.
    pub fn parse(v: RawValue<'_>, ctx: &Ctx) -> Result<UnionWith, MqlError> {
        match v {
            RawValue::String(s) => Ok(UnionWith {
                coll: Some(s.to_owned()),
                pipeline: Pipeline::default(),
                joined_error: None,
            }),
            RawValue::Document(d) => {
                let (mut coll, mut stages) = (None, Vec::new());
                for (k, x) in d.iter() {
                    match (k, x) {
                        ("coll", RawValue::String(s)) => coll = Some(s.to_owned()),
                        ("pipeline", RawValue::Array(a)) => stages = a.iter().collect(),
                        (other, _) => {
                            return Err(error(
                                UNKNOWN_FIELD,
                                format!("BSON field '$unionWith.{other}' is an unknown field."),
                            ));
                        }
                    }
                }
                super::out_merge::refuse_in(&stages, "$unionWith")?;
                let pipeline = match coll {
                    Some(_) => Pipeline::parse(&stages, ctx)?,
                    None => Pipeline::parse_from_documents(&stages, ctx, "$unionWith")?,
                };
                let joined_error = match coll {
                    Some(_) => super::meta::check_joined(&stages)
                        .and_then(|()| super::sort::check_random_sort(&stages))
                        .err(),
                    None => {
                        super::meta::check_available(&stages)?;
                        None
                    }
                };
                Ok(UnionWith { coll, pipeline, joined_error })
            }
            _ => Err(error(
                FAILED_TO_PARSE,
                format!(
                    "the $unionWith stage specification must be an object or string, but found {}",
                    crate::expr::arith::raw_type_name(v)
                ),
            )),
        }
    }

    /// `docs` and then the results of the other collection's pipeline.
    pub fn run(&self, mut docs: Vec<Document>, env: &dyn Env) -> Result<Vec<Document>, MqlError> {
        docs.extend(self.joined(env)?.0);
        Ok(docs)
    }

    /// [`UnionWith::run`] over documents with metadata `metas`: the documents it adds bring the
    /// metadata of its pipeline, none where it gives none.
    pub(crate) fn run_with_meta(
        &self,
        mut docs: Vec<Document>,
        mut metas: Vec<super::meta::Meta>,
        env: &dyn Env,
    ) -> Result<(Vec<Document>, Vec<super::meta::Meta>), MqlError> {
        let (more, more_metas) = self.joined(env)?;
        let n = more.len();
        docs.extend(more);
        metas.extend(more_metas.unwrap_or_else(|| vec![super::meta::Meta::new(); n]));
        Ok((docs, metas))
    }

    /// The results of the other collection's pipeline, with their metadata.
    fn joined(
        &self,
        env: &dyn Env,
    ) -> Result<(Vec<Document>, Option<Vec<super::meta::Meta>>), MqlError> {
        if let Some(e) = &self.joined_error {
            return Err(e.clone());
        }
        match &self.coll {
            Some(c) => {
                // A `$text` first reads the text index instead of the documents.
                let input =
                    if self.pipeline.reads_collection() { env.collection(c)? } else { Vec::new() };
                let joined = Joined { inner: env, coll: c, from_lookup: false };
                self.pipeline.run_with_meta(input, None, &joined, true)
            }
            None => self.pipeline.run_with_meta(Vec::new(), None, env, true),
        }
    }
}

/// RefDb's code for a `$documents` that is no array.
const DOCUMENTS_ARRAY: ErrorCode = ErrorCode { code: 5_858_203, name: "Location5858203" };

/// The documents `$documents` gives: its expression's array, evaluated once.
pub fn run_documents(e: &Expr, env: &dyn Env) -> Result<Vec<Document>, MqlError> {
    let empty = Document::new();
    match e.eval(&empty, &env.scope(&empty))? {
        Some(Value::Array(items)) => items
            .into_iter()
            .map(|x| match x {
                Value::Document(d) => Ok(d),
                other => Err(error(
                    super::errors::NEW_ROOT,
                    format!(
                        "elements within the array passed to $documents must evaluate to an object, but resulting value was: {}. Type of resulting value: '{}'. Input document: {{_tempDocumentsField: {}}}",
                        display(&other),
                        crate::expr::arith::type_name_of(&other),
                        crate::expr::values::display_bson(&other)
                    ),
                )),
            })
            .collect(),
        _ => Err(error(DOCUMENTS_ARRAY, "an array is expected")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `$where` only in the filter of a `find` run as a pipeline (measured: `aggregate` refuses
    /// it with code 2, `find` with `$text` runs it).
    #[test]
    fn where_only_in_the_match_of_a_find() {
        let bytes = Document::new().with("$where", "true").to_bytes().expect("bytes");
        let filter = RawValue::Document(RawDoc::from_bytes(&bytes).expect("raw"));
        let ctx = Ctx::checked(std::iter::empty::<&str>());
        let refused = Match::parse(filter, &ctx).expect_err("refused");
        assert_eq!(refused.code.code, 2);
        assert!(Match::parse(filter, &ctx.with_where()).is_ok());
        assert!(ctx.with_where().allows_where() && !ctx.allows_where());
    }
}
