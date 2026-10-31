//! The stages that group documents (measured on a `refrouter` 9.0.4): `$group`, `$bucket`,
//! `$bucketAuto` and `$sortByCount`. Groups are keyed by values that compare equal (`1` and
//! `1.0` are one group, a missing `_id` is `null`) and come out in the order their first document
//! came.

use std::collections::HashMap;

use teseladb_bson::{Document, RawValue, Value};
use teseladb_mql::MqlError;

use super::Env;
use super::errors::{
    BUCKET_AUTO_BUCKETS, BUCKET_AUTO_INT, BUCKET_AUTO_NUMERIC, BUCKET_AUTO_OUTPUT,
    BUCKET_AUTO_REQUIRED, BUCKET_BOUNDARIES_ARRAY, BUCKET_DEFAULT, BUCKET_FEW, BUCKET_GROUP_BY,
    BUCKET_NO_MATCH, BUCKET_OUTPUT, BUCKET_REQUIRED, BUCKET_TYPES, BUCKET_UNSORTED, GROUP_DOLLAR,
    GROUP_DOT, GROUP_ID, SORT_BY_COUNT_FIELD, SORT_BY_COUNT_OBJECT, TYPE_MISMATCH, UNKNOWN_FIELD,
    error,
};
use super::granularity::Granularity;
use crate::accum::{Accumulator, State};
use crate::expr::arith::type_name_of;
use crate::expr::values::display;
use crate::expr::{Ctx, Expr, Scope, order};

/// The scope of an expression of a group over `doc`, `sbe` when RefDb evaluates it with its
/// slot-based engine.
fn group_scope(env: &dyn Env, doc: &Document, sbe: bool) -> Scope {
    let mut s = env.scope(doc);
    s.sbe = sbe;
    s
}

/// The accumulators of a group's fields, in the specification's order.
type Outputs = Vec<(String, Accumulator)>;

/// A `$group`.
#[derive(Clone, Debug)]
pub struct Group {
    id: Expr,
    outputs: Outputs,
}

/// Reads output fields (`$group`'s, `$bucket`'s `output`) from `d`, skipping `skip`.
fn read_outputs(d: teseladb_bson::RawDoc<'_>, skip: &str, ctx: &Ctx) -> Result<Outputs, MqlError> {
    let mut out = Vec::new();
    for (k, v) in d.iter() {
        if k == skip {
            continue;
        }
        if k.starts_with('$') {
            return Err(error(
                GROUP_DOLLAR,
                format!("The field name '{k}' cannot be an operator name"),
            ));
        }
        if k.contains('.') {
            return Err(error(GROUP_DOT, format!("The field name '{k}' cannot contain '.'")));
        }
        out.push((k.to_owned(), Accumulator::parse(k, v, ctx)?));
    }
    Ok(out)
}

impl Group {
    /// Reads the specification of a `$group`.
    pub fn parse(v: RawValue<'_>, ctx: &Ctx) -> Result<Group, MqlError> {
        let RawValue::Document(d) = v else {
            return Err(error(
                super::errors::GROUP_SPEC_NOT_OBJECT,
                "a group's fields must be specified in an object",
            ));
        };
        let id = d
            .get("_id")
            .ok_or_else(|| error(GROUP_ID, "a group specification must include an _id"))?;
        Ok(Group { id: Expr::parse_in(id, ctx)?, outputs: read_outputs(d, "_id", ctx)? })
    }

    /// The groups of `docs`.
    pub fn run(
        &self,
        docs: Vec<Document>,
        env: &dyn Env,
        sbe: bool,
    ) -> Result<Vec<Document>, MqlError> {
        let collation = env.collation();
        let col = collation.get();
        let mut groups: Groups = Groups::default();
        for doc in docs {
            let scope = group_scope(env, &doc, sbe);
            let key = self.id.eval(&doc, &scope)?.unwrap_or(Value::Null);
            let states = groups.entry(key, &self.outputs, col)?;
            for ((_, acc), st) in self.outputs.iter().zip(states.iter_mut()) {
                acc.add(st, &doc, &scope)?;
            }
        }
        groups.finish("_id", &self.outputs, col, sbe)
    }
}

/// Groups in the order they appear, with their accumulator states.
#[derive(Default)]
struct Groups {
    index: HashMap<Vec<u8>, usize>,
    list: Vec<(Value, Vec<State>)>,
}

impl Groups {
    fn entry(
        &mut self,
        key: Value,
        outputs: &Outputs,
        col: Option<&dyn teseladb_bson::Collator>,
    ) -> Result<&mut Vec<State>, MqlError> {
        let canon = canonical(&key, col);
        let i = match self.index.get(&canon) {
            Some(i) => *i,
            None => {
                self.list.push((key, outputs.iter().map(|(_, a)| a.start()).collect()));
                self.index.insert(canon, self.list.len() - 1);
                self.list.len() - 1
            }
        };
        Ok(&mut self.list[i].1)
    }

    fn finish(
        self,
        id: &str,
        outputs: &Outputs,
        col: Option<&dyn teseladb_bson::Collator>,
        sbe: bool,
    ) -> Result<Vec<Document>, MqlError> {
        let mut out = Vec::with_capacity(self.list.len());
        for (key, states) in self.list {
            let mut d = Document::new().with(id, key);
            for ((name, acc), st) in outputs.iter().zip(states) {
                d.push(name.clone(), acc.finish(st, col, sbe)?);
            }
            out.push(d);
        }
        Ok(out)
    }
}

fn less(a: &Value, b: &Value, col: Option<&dyn teseladb_bson::Collator>) -> Result<bool, MqlError> {
    Ok(order(&Some(a.clone()), &Some(b.clone()), col)? == std::cmp::Ordering::Less)
}

fn equal(
    a: &Value,
    b: &Value,
    col: Option<&dyn teseladb_bson::Collator>,
) -> Result<bool, MqlError> {
    Ok(order(&Some(a.clone()), &Some(b.clone()), col)? == std::cmp::Ordering::Equal)
}

/// A key that is the same for values that compare equal: numbers by their value, the other
/// types by their bytes, documents and arrays element by element.
pub(super) fn canonical(v: &Value, col: Option<&dyn teseladb_bson::Collator>) -> Vec<u8> {
    let mut out = Vec::new();
    encode(v, col, &mut out);
    out
}

fn encode(v: &Value, col: Option<&dyn teseladb_bson::Collator>, out: &mut Vec<u8>) {
    use teseladb_mql::decimal_math::from_f64_exact;
    let number = |d: teseladb_bson::Decimal128, out: &mut Vec<u8>| {
        out.push(b'n');
        // The shortest form of a decimal: equal numbers write the same text.
        let text = normal_number(&d.to_string());
        out.extend_from_slice(text.as_bytes());
        out.push(0);
    };
    match v {
        Value::Null | Value::Undefined => out.push(b'0'),
        Value::Int32(n) => number(
            teseladb_bson::Decimal128::from_parts(*n < 0, u128::from(n.unsigned_abs()), 0)
                .unwrap_or_else(teseladb_bson::Decimal128::nan),
            out,
        ),
        Value::Int64(n) => number(
            teseladb_bson::Decimal128::from_parts(*n < 0, u128::from(n.unsigned_abs()), 0)
                .unwrap_or_else(teseladb_bson::Decimal128::nan),
            out,
        ),
        Value::Double(x) if x.is_nan() => out.extend_from_slice(b"nNaN\0"),
        Value::Double(x) => number(from_f64_exact(*x), out),
        Value::Decimal128(d) => number(*d, out),
        // In a collation, strings that compare equal share their sort key.
        Value::String(s) => match col {
            Some(c) => {
                let key = c.sort_key(s);
                out.push(b'k');
                out.extend_from_slice(&u32::try_from(key.len()).unwrap_or(u32::MAX).to_le_bytes());
                out.extend_from_slice(&key);
            }
            None => {
                out.push(b's');
                out.extend_from_slice(s.as_bytes());
                out.push(0);
            }
        },
        Value::Document(d) => {
            out.push(b'd');
            for (k, x) in d.iter() {
                out.extend_from_slice(k.as_bytes());
                out.push(0);
                encode(x, col, out);
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'a');
            for x in items {
                encode(x, col, out);
            }
            out.push(b']');
        }
        other => {
            out.push(b'o');
            let d = Document::new().with("v", other.clone());
            out.extend_from_slice(&d.to_bytes().unwrap_or_default());
        }
    }
}

/// A decimal's text with the exponent and trailing zeros normalized (`1.50E+1` and `15` agree).
fn normal_number(text: &str) -> String {
    match text {
        "NaN" | "Infinity" | "-Infinity" => return text.to_owned(),
        _ => {}
    }
    let (neg, body) = match text.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, text),
    };
    let (mant, exp) = match body.split_once(['E', 'e']) {
        Some((m, e)) => (m, e.parse::<i64>().unwrap_or(0)),
        None => (body, 0),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    let digits = format!("{int}{frac}");
    let exp = exp - frac.len() as i64;
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        return "0".to_owned();
    }
    let zeros = trimmed.len() - trimmed.trim_end_matches('0').len();
    let core = trimmed.trim_end_matches('0');
    format!("{}{core}e{}", if neg { "-" } else { "" }, exp + zeros as i64)
}

/// The type a boundary of `$bucket` belongs to (numbers are one type).
fn canonical_type(v: &Value) -> &'static str {
    match v {
        Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_) => "number",
        other => type_name_of(other),
    }
}

/// A `$bucket`.
#[derive(Clone, Debug)]
pub struct Bucket {
    group_by: Expr,
    boundaries: Vec<Value>,
    default: Option<Value>,
    outputs: Outputs,
}

/// Reads a `groupBy`: a `$` path or an expression object.
fn group_by(v: RawValue<'_>, stage: &str, ctx: &Ctx) -> Result<Expr, MqlError> {
    match v {
        RawValue::String(s) if s.starts_with('$') => Expr::parse_in(v, ctx),
        RawValue::Document(_) => Expr::parse_in(v, ctx),
        other => Err(error(
            BUCKET_GROUP_BY,
            format!(
                "The {stage} 'groupBy' field must be defined as a $-prefixed path or an expression, but found: {}.",
                display(&Value::from(other))
            ),
        )),
    }
}

/// The accumulators of `output`, or `count: {$sum: 1}` without one. `not_object` builds the stage's
/// own error for an `output` that is not a document, from the type name of what it found.
fn count_output(
    output: Option<RawValue<'_>>,
    ctx: &Ctx,
    not_object: impl FnOnce(&str) -> MqlError,
) -> Result<Outputs, MqlError> {
    match output {
        Some(RawValue::Document(d)) => read_outputs(d, "", ctx),
        Some(other) => Err(not_object(crate::expr::arith::raw_type_name(other))),
        None => {
            let spec = Document::new().with("$sum", 1);
            let bytes = spec.to_bytes().map_err(|e| error(TYPE_MISMATCH, e.to_string()))?;
            let raw = teseladb_bson::RawDoc::from_bytes(&bytes)
                .map_err(|e| error(TYPE_MISMATCH, e.to_string()))?;
            Ok(vec![(
                "count".to_owned(),
                Accumulator::parse("count", RawValue::Document(raw), ctx)?,
            )])
        }
    }
}

impl Bucket {
    /// Reads the specification of a `$bucket`.
    pub fn parse(v: RawValue<'_>, ctx: &Ctx) -> Result<Bucket, MqlError> {
        let RawValue::Document(d) = v else {
            return Err(error(
                super::errors::BUCKET_NOT_OBJECT,
                format!(
                    "Argument to $bucket stage must be an object, but found type: {}.",
                    crate::expr::arith::raw_type_name(v)
                ),
            ));
        };
        let (mut gb, mut bounds, mut default, mut output) = (None, None, None, None);
        for (k, x) in d.iter() {
            match k {
                "groupBy" => gb = Some(group_by(x, "$bucket", ctx)?),
                "boundaries" => bounds = Some(x),
                "default" => default = Some(Value::from(x)),
                "output" => output = Some(x),
                other => {
                    return Err(error(
                        UNKNOWN_FIELD,
                        format!("Unrecognized option to $bucket: {other}."),
                    ));
                }
            }
        }
        let required = || {
            error(BUCKET_REQUIRED, "$bucket requires 'groupBy' and 'boundaries' to be specified.")
        };
        let group_by = gb.ok_or_else(required)?;
        let boundaries: Vec<Value> = match bounds {
            Some(RawValue::Array(a)) => a.iter().map(Value::from).collect(),
            Some(other) => {
                return Err(error(
                    BUCKET_BOUNDARIES_ARRAY,
                    format!(
                        "The $bucket 'boundaries' field must be an array, but found type: {}.",
                        crate::expr::arith::raw_type_name(other)
                    ),
                ));
            }
            None => return Err(required()),
        };
        if boundaries.len() < 2 {
            return Err(error(
                BUCKET_FEW,
                format!(
                    "The $bucket 'boundaries' field must have at least 2 values, but found {} value(s).",
                    boundaries.len()
                ),
            ));
        }
        for (i, w) in boundaries.windows(2).enumerate() {
            let (a, b) = (canonical_type(&w[0]), canonical_type(&w[1]));
            if a != b {
                return Err(error(
                    BUCKET_TYPES,
                    format!(
                        "All values in the the 'boundaries' option to $bucket must have the same type. Found conflicting types {} and {}.",
                        type_name_of(&w[0]),
                        type_name_of(&w[1])
                    ),
                ));
            }
            if order(&Some(w[0].clone()), &Some(w[1].clone()), None)? != std::cmp::Ordering::Less {
                return Err(error(
                    BUCKET_UNSORTED,
                    format!(
                        "The 'boundaries' option to $bucket must be sorted, but elements {i} and {} are not in ascending order ({} is not less than {}).",
                        i + 1,
                        display(&w[0]),
                        display(&w[1])
                    ),
                ));
            }
        }
        if let Some(def) = &default
            && canonical_type(def) == canonical_type(&boundaries[0])
        {
            let low = order(&Some(def.clone()), &Some(boundaries[0].clone()), None)?;
            let high =
                order(&Some(def.clone()), &Some(boundaries[boundaries.len() - 1].clone()), None)?;
            if low != std::cmp::Ordering::Less && high == std::cmp::Ordering::Less {
                return Err(error(
                    BUCKET_DEFAULT,
                    "The $bucket 'default' field must be less than the lowest boundary or greater than or equal to the highest boundary.",
                ));
            }
        }
        let outputs = count_output(output, ctx, |found| {
            error(
                BUCKET_OUTPUT,
                format!("The $bucket 'output' field must be an object, but found type: {found}."),
            )
        })?;
        Ok(Bucket { group_by, boundaries, default, outputs })
    }

    /// The buckets of `docs`, in boundary order, the default last.
    pub fn run(
        &self,
        docs: Vec<Document>,
        env: &dyn Env,
        sbe: bool,
    ) -> Result<Vec<Document>, MqlError> {
        let collation = env.collation();
        let col = collation.get();
        let n = self.boundaries.len() - 1;
        let mut states: Vec<Option<Vec<State>>> = vec![None; n + 1];
        for doc in docs {
            let scope = group_scope(env, &doc, sbe);
            let v = self.group_by.eval(&doc, &scope)?.unwrap_or(Value::Null);
            let mut slot = None;
            if canonical_type(&v) == canonical_type(&self.boundaries[0]) {
                for i in 0..n {
                    let lo = order(&Some(self.boundaries[i].clone()), &Some(v.clone()), col)?;
                    let hi = order(&Some(v.clone()), &Some(self.boundaries[i + 1].clone()), col)?;
                    if lo != std::cmp::Ordering::Greater && hi == std::cmp::Ordering::Less {
                        slot = Some(i);
                        break;
                    }
                }
            }
            let i = match (slot, &self.default) {
                (Some(i), _) => i,
                (None, Some(_)) => n,
                (None, None) => {
                    return Err(error(
                        BUCKET_NO_MATCH,
                        "$switch could not find a matching branch for an input, and no default was specified.",
                    ));
                }
            };
            let st = states[i]
                .get_or_insert_with(|| self.outputs.iter().map(|(_, a)| a.start()).collect());
            for ((_, acc), s) in self.outputs.iter().zip(st.iter_mut()) {
                acc.add(s, &doc, &scope)?;
            }
        }
        let mut out = Vec::new();
        for (i, st) in states.into_iter().enumerate() {
            let Some(st) = st else { continue };
            let id = if i < n {
                self.boundaries[i].clone()
            } else {
                self.default.clone().unwrap_or(Value::Null)
            };
            let mut d = Document::new().with("_id", id);
            for ((name, acc), s) in self.outputs.iter().zip(st) {
                d.push(name.clone(), acc.finish(s, col, sbe)?);
            }
            out.push(d);
        }
        Ok(out)
    }
}

/// A `$bucketAuto`.
#[derive(Clone, Debug)]
pub struct BucketAuto {
    group_by: Expr,
    buckets: usize,
    outputs: Outputs,
    granularity: Option<Granularity>,
}

impl BucketAuto {
    /// Reads the specification of a `$bucketAuto`.
    pub fn parse(v: RawValue<'_>, ctx: &Ctx) -> Result<BucketAuto, MqlError> {
        let RawValue::Document(d) = v else {
            return Err(error(
                super::errors::BUCKET_AUTO_NOT_OBJECT,
                format!(
                    "The argument to $bucketAuto must be an object, but found type: {}",
                    crate::expr::arith::raw_type_name(v)
                ),
            ));
        };
        let (mut gb, mut buckets, mut output, mut granularity) = (None, None, None, None);
        for (k, x) in d.iter() {
            match k {
                "groupBy" => gb = Some(group_by(x, "$bucketAuto", ctx)?),
                "buckets" => buckets = Some(Value::from(x)),
                "output" => output = Some(x),
                "granularity" => {
                    let name = match x {
                        RawValue::String(s) => s.to_owned(),
                        other => display(&Value::from(other)),
                    };
                    granularity = Some(Granularity::named(&name)?);
                }
                other => {
                    return Err(error(
                        UNKNOWN_FIELD,
                        format!("Unrecognized option to $bucketAuto: {other}."),
                    ));
                }
            }
        }
        let (Some(group_by), Some(buckets)) = (gb, buckets) else {
            return Err(error(
                BUCKET_AUTO_REQUIRED,
                "$bucketAuto requires 'groupBy' and 'buckets' to be specified",
            ));
        };
        let n = bucket_count(&buckets)?;
        let outputs = count_output(output, ctx, |found| {
            error(
                BUCKET_AUTO_OUTPUT,
                format!(
                    "The $bucketAuto 'output' field must be an object, but found type: {found}"
                ),
            )
        })?;
        Ok(BucketAuto { group_by, buckets: n, outputs, granularity })
    }

    /// Buckets of about the same number of documents over the sorted values of `groupBy`; equal
    /// values stay in one bucket, and a bucket's `max` is the next bucket's `min` (the last
    /// value for the last bucket).
    pub fn run(&self, docs: Vec<Document>, env: &dyn Env) -> Result<Vec<Document>, MqlError> {
        let collation = env.collation();
        let col = collation.get();
        let mut keyed = Vec::with_capacity(docs.len());
        for doc in docs {
            let scope = group_scope(env, &doc, false);
            let v = self.group_by.eval(&doc, &scope)?.unwrap_or(Value::Null);
            // The values are checked in the order the documents come.
            if self.granularity.is_some() {
                Granularity::check(&v)?;
            }
            keyed.push((v, doc));
        }
        let mut failure = None;
        keyed.sort_by(|(a, _), (b, _)| {
            order(&Some(a.clone()), &Some(b.clone()), col).unwrap_or_else(|e| {
                failure.get_or_insert(e);
                std::cmp::Ordering::Equal
            })
        });
        if let Some(e) = failure {
            return Err(e);
        }
        let total = keyed.len();
        let size = ((total as f64) / (self.buckets as f64)).round().max(1.0) as usize;
        // Each bucket: its range of `keyed` and its upper boundary.
        let mut ranges: Vec<(usize, usize, Value)> = Vec::new();
        let mut start = 0;
        while start < total {
            let mut end = (start + size).min(total);
            if ranges.len() + 1 == self.buckets {
                end = total;
            }
            let max = match self.granularity {
                Some(g) => {
                    let boundary = g.round(&keyed[end - 1].0, true)?;
                    while end < total && less(&keyed[end].0, &boundary, col)? {
                        end += 1;
                    }
                    boundary
                }
                None => {
                    while end < total && equal(&keyed[end].0, &keyed[end - 1].0, col)? {
                        end += 1;
                    }
                    match keyed.get(end) {
                        Some((v, _)) => v.clone(),
                        None => keyed[end - 1].0.clone(),
                    }
                }
            };
            ranges.push((start, end, max));
            start = end;
        }
        let mut out = Vec::with_capacity(ranges.len());
        let mut previous: Option<Value> = None;
        for (s, e, max) in ranges {
            let min = match (previous.take(), self.granularity) {
                (Some(p), Some(_)) => p,
                (None, Some(g)) => g.round(&keyed[s].0, false)?,
                (_, None) => keyed[s].0.clone(),
            };
            previous = Some(max.clone());
            let mut states: Vec<State> = self.outputs.iter().map(|(_, a)| a.start()).collect();
            for (_, doc) in &keyed[s..e] {
                let scope = group_scope(env, doc, false);
                for ((_, acc), st) in self.outputs.iter().zip(states.iter_mut()) {
                    acc.add(st, doc, &scope)?;
                }
            }
            let mut d =
                Document::new().with("_id", Document::new().with("min", min).with("max", max));
            for ((name, acc), st) in self.outputs.iter().zip(states) {
                d.push(name.clone(), acc.finish(st, col, false)?);
            }
            out.push(d);
        }
        Ok(out)
    }
}

/// The `buckets` of `$bucketAuto`: a positive 32-bit integer.
fn bucket_count(v: &Value) -> Result<usize, MqlError> {
    if crate::expr::values::f64_of(v).is_none() {
        return Err(error(
            BUCKET_AUTO_NUMERIC,
            format!(
                "The $bucketAuto 'buckets' field must be a numeric value, but found type: {}",
                type_name_of(v)
            ),
        ));
    }
    let n = crate::expr::values::int64_of(v)
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(|| {
            error(
                BUCKET_AUTO_INT,
                format!(
                    "The $bucketAuto 'buckets' field must be representable as a 32-bit integer, but found {}",
                    display(v)
                ),
            )
        })?;
    if n <= 0 {
        return Err(error(
            BUCKET_AUTO_BUCKETS,
            format!("The $bucketAuto 'buckets' field must be greater than 0, but found: {n}"),
        ));
    }
    Ok(usize::try_from(n).unwrap_or(0))
}

/// `$sortByCount`: groups by the expression and counts, the largest count first.
pub fn sort_by_count(v: RawValue<'_>, ctx: &Ctx) -> Result<Expr, MqlError> {
    match v {
        RawValue::String(s) if s.starts_with('$') => Expr::parse_in(v, ctx),
        RawValue::Document(d) => match d.iter().next() {
            Some((k, _)) if k.starts_with('$') => Expr::parse_in(v, ctx),
            _ => Err(error(
                SORT_BY_COUNT_OBJECT,
                "the sortByCount field must be defined as a $-prefixed path or an expression inside an object",
            )),
        },
        RawValue::String(_) => Err(error(
            SORT_BY_COUNT_FIELD,
            "the sortByCount field must be defined as a $-prefixed path or an expression inside an object",
        )),
        _ => Err(error(
            super::errors::SORT_BY_COUNT_TYPE,
            "the sortByCount field must be specified as a string or as an object",
        )),
    }
}

/// The counts of `$sortByCount` over `docs`.
pub fn run_sort_by_count(
    e: &Expr,
    docs: Vec<Document>,
    env: &dyn Env,
    sbe: bool,
) -> Result<Vec<Document>, MqlError> {
    let collation = env.collation();
    let col = collation.get();
    let mut index: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut list: Vec<(Value, i64)> = Vec::new();
    for doc in docs {
        let scope = group_scope(env, &doc, sbe);
        let key = e.eval(&doc, &scope)?.unwrap_or(Value::Null);
        let c = canonical(&key, col);
        match index.get(&c) {
            Some(i) => list[*i].1 += 1,
            None => {
                index.insert(c, list.len());
                list.push((key, 1));
            }
        }
    }
    list.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    Ok(list
        .into_iter()
        .map(|(k, n)| {
            let count = i32::try_from(n).map_or(Value::Int64(n), Value::Int32);
            Document::new().with("_id", k).with("count", count)
        })
        .collect())
}
