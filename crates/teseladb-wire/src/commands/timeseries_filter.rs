//! The partial filter of a time series index as a filter over its buckets (015 FR-008), the way
//! a `refrouter` 9.0.4 writes it in the bucket index's specification (measured on 31-10-2026 with
//! `listIndexes` and `rawData: true`):
//!
//! - a condition on the meta value goes to `meta` as it is (a bare value as `$eq`);
//! - `$gt`/`$gte` on a measurement field compare the bucket's greatest value
//!   (`control.max.<f>: {$_internalExprGt: v}`), `$lt`/`$lte` its least, and an equality both;
//!   `$exists: true` asks for both; `$in` is an `$or` of equalities;
//! - on the time field each bound also bounds the other end by `bucketMaxSpanSeconds`;
//! - `$or` keeps its branches, translated; the conditions of the top level (and of its `$and`)
//!   come out as one document when only one of them is on a measurement, otherwise in `$and`.
//!
//! Anything else is RefDb's 5916301 with the condition as RefDb normalizes it.

use teseladb_mql::timeseries_write::BucketSpec;
use teseladb_bson::{Document, Value};

use crate::error::{CommandError, TIMESERIES_PREDICATE};

/// One condition of the bucket filter: its path and condition, or a whole expression.
type Cond = (String, Value);

/// Why a condition cannot go to the buckets, as RefDb words it.
#[derive(Clone, Copy)]
enum Why {
    Predicate,
    Null,
    NullInIn,
    Operand,
}

fn refuse(shown: Document, why: Why) -> CommandError {
    let what = match why {
        Why::Predicate => "can't handle this predicate",
        Why::Null => "can't handle comparison to null",
        Why::NullInIn => "can't handle {$eq: null} predicate (inside $in predicate)",
        Why::Operand => "operand can't be an object or array",
    };
    CommandError::new(
        TIMESERIES_PREDICATE,
        format!(
            "Error translating non-metadata time-series predicate to operate on buckets: {what}: {}",
            super::display(&Value::Document(shown))
        ),
    )
}

/// The types of a `$type` as RefDb shows them: `"number"` first when asked, then the codes
/// once each in ascending order (measured on a `refnode` 9.0.4).
fn type_set(v: &Value) -> Value {
    use teseladb_mql::types::{TypeSpec, by_alias, by_code};
    let items = match v {
        Value::Array(items) => items.clone(),
        one => vec![one.clone()],
    };
    // A `$type` that names no type is refused before (the filter is parsed first).
    let specs = items.iter().filter_map(|i| match i {
        Value::String(s) => by_alias(s),
        other => crate::types::number(other).and_then(|n| by_code(n as i32)),
    });
    let mut number = false;
    let mut codes = Vec::new();
    for spec in specs {
        match spec {
            TypeSpec::Number => number = true,
            // RefDb numbers the minimum key -1.
            TypeSpec::Element(e) => codes.push(i32::from(i8::from_ne_bytes([e]))),
        }
    }
    codes.sort_unstable();
    codes.dedup();
    let shown = codes.into_iter().map(Value::Int32);
    Value::Array(number.then(|| Value::from("number")).into_iter().chain(shown).collect())
}

/// The condition of an `$elemMatch` as RefDb shows it: operators on the element as they are;
/// conditions on its fields one predicate each, alone or in `$and` (measured on a `refnode` 9.0.4).
fn elem_match_shown(inner: &Document) -> Document {
    if inner.iter().next().is_some_and(|(k, _)| k.starts_with('$')) {
        return normalized(&Value::Document(inner.clone()));
    }
    let mut preds: Vec<Document> = Vec::new();
    for (k, x) in inner.iter() {
        for (op, v) in normalized(x).iter() {
            preds.push(Document::new().with(k, Document::new().with(op, v.clone())));
        }
    }
    match <[Document; 1]>::try_from(preds) {
        Ok([one]) => one,
        Err(preds) => Document::new()
            .with("$and", Value::Array(preds.into_iter().map(Value::Document).collect())),
    }
}

/// Condition `cond` on `path` as RefDb normalizes it in a message.
fn normalized(cond: &Value) -> Document {
    let Value::Document(ops) = cond else {
        return Document::new().with("$eq", cond.clone());
    };
    if ops.iter().next().is_none_or(|(k, _)| !k.starts_with('$')) {
        return Document::new().with("$eq", cond.clone());
    }
    let mut out = Document::new();
    for (op, v) in ops.iter() {
        let n = match (op, v) {
            ("$ne", v) => Document::new().with("$not", Document::new().with("$eq", v.clone())),
            ("$nin", v) => Document::new().with("$not", Document::new().with("$in", v.clone())),
            ("$exists", v) if !crate::types::truthy(v).unwrap_or(true) => {
                Document::new().with("$not", Document::new().with("$exists", true))
            }
            ("$type", v) => Document::new().with("$type", type_set(v)),
            ("$elemMatch", Value::Document(inner)) => {
                Document::new().with("$elemMatch", elem_match_shown(inner))
            }
            (op, v) => Document::new().with(op, v.clone()),
        };
        for (k, x) in n.iter() {
            out.push(k, x.clone());
        }
    }
    out
}

/// The conditions on `control.min.<f>`/`control.max.<f>` of one operator on measurement or time
/// field `f`.
fn bounds(spec: &BucketSpec, f: &str, op: &str, v: &Value) -> Option<Vec<Cond>> {
    let (min, max) = (format!("control.min.{f}"), format!("control.max.{f}"));
    let shifted = |x: &Value, sign: i64| shifted(spec, f, x, sign);
    Some(match op {
        "$gt" | "$gte" => {
            let internal = if op == "$gt" { "$_internalExprGt" } else { "$_internalExprGte" };
            let mut out = vec![cmp(&max, internal, v.clone())];
            out.extend(shifted(v, -1).map(|s| cmp(&min, internal, s)));
            out
        }
        "$lt" | "$lte" => {
            let internal = if op == "$lt" { "$_internalExprLt" } else { "$_internalExprLte" };
            let mut out = vec![cmp(&min, internal, v.clone())];
            out.extend(shifted(v, 1).map(|s| cmp(&max, internal, s)));
            out
        }
        "$eq" => eq_bounds(spec, f, v),
        "$exists" if crate::types::truthy(v) == Some(true) => vec![
            (min, Value::Document(Document::new().with("$exists", true))),
            (max, Value::Document(Document::new().with("$exists", true))),
        ],
        _ => return None,
    })
}

/// `path: {internal: x}`.
fn cmp(path: &str, internal: &str, x: Value) -> Cond {
    (path.to_owned(), Value::Document(Document::new().with(internal, x)))
}

/// A time bound `x` on time field `f` moved by the bucket span in direction `sign`; `None` on a
/// measurement or a value that is no date.
fn shifted(spec: &BucketSpec, f: &str, x: &Value, sign: i64) -> Option<Value> {
    match x {
        Value::DateTime(t) if f == spec.time_field => {
            Some(Value::DateTime(t.saturating_add(sign * spec.max_span_seconds * 1000)))
        }
        _ => None,
    }
}

/// The conditions on `control.min.<f>`/`control.max.<f>` of `f: {$eq: v}`.
fn eq_bounds(spec: &BucketSpec, f: &str, v: &Value) -> Vec<Cond> {
    let (min, max) = (format!("control.min.{f}"), format!("control.max.{f}"));
    let shifted = |x: &Value, sign: i64| shifted(spec, f, x, sign);
    let mut out = vec![cmp(&min, "$_internalExprLte", v.clone())];
    out.extend(shifted(v, -1).map(|s| cmp(&min, "$_internalExprGte", s)));
    out.push(cmp(&max, "$_internalExprGte", v.clone()));
    out.extend(shifted(v, 1).map(|s| cmp(&max, "$_internalExprLte", s)));
    out
}

/// The conditions of `path: cond` over the buckets, and whether they are on the meta value.
fn field(spec: &BucketSpec, path: &str, cond: &Value) -> Result<(Vec<Cond>, bool), CommandError> {
    if let Some(m) = super::timeseries::meta_path_of(spec, path) {
        let c = match cond {
            Value::Document(d) if d.iter().next().is_some_and(|(k, _)| k.starts_with('$')) => {
                cond.clone()
            }
            other => Value::Document(Document::new().with("$eq", other.clone())),
        };
        return Ok((vec![(m, c)], true));
    }
    // The predicate refused is shown alone, as RefDb splits a path's operators (measured on a
    // `refnode` 9.0.4).
    let shown_op = |op: &str, v: &Value| {
        Document::new()
            .with(path, normalized(&Value::Document(Document::new().with(op, v.clone()))))
    };
    let ops: Vec<(String, Value)> = match cond {
        Value::Document(d) if d.iter().next().is_some_and(|(k, _)| k.starts_with('$')) => {
            d.iter().map(|(k, v)| (k.to_owned(), v.clone())).collect()
        }
        other => vec![("$eq".to_owned(), other.clone())],
    };
    let mut out = Vec::new();
    for (op, v) in &ops {
        let shown = || shown_op(op, v);
        let compares = matches!(op.as_str(), "$eq" | "$gt" | "$gte" | "$lt" | "$lte");
        // A comparison to null is refused, shown alone (measured on a `refnode` 9.0.4).
        if compares && matches!(v, Value::Null) {
            let one = Document::new().with(path, Document::new().with(op.as_str(), Value::Null));
            return Err(refuse(one, Why::Null));
        }
        if compares && matches!(v, Value::Document(_) | Value::Array(_)) {
            return Err(refuse(shown(), Why::Operand));
        }
        // The filter is parsed first: an `$in` holds an array.
        if let ("$in", Value::Array(items)) = (op.as_str(), v) {
            if items.iter().any(|i| matches!(i, Value::Null)) {
                return Err(refuse(shown(), Why::NullInIn));
            }
            let branches = items
                .iter()
                .map(|item| Value::Document(assemble(eq_bounds(spec, path, item), false)));
            out.push(("$or".to_owned(), Value::Array(branches.collect())));
            continue;
        }
        out.extend(bounds(spec, path, op, v).ok_or_else(|| refuse(shown(), Why::Predicate))?);
    }
    Ok((out, false))
}

/// The conditions as one filter: alone, as one document when `flat`, or in `$and`.
fn assemble(conds: Vec<Cond>, flat: bool) -> Document {
    if conds.len() == 1 || flat {
        let mut d = Document::new();
        for (k, v) in conds {
            d.push(k, v);
        }
        return d;
    }
    let items = conds.into_iter().map(|(k, v)| Value::Document(Document::new().with(k, v)));
    Document::new().with("$and", Value::Array(items.collect()))
}

/// The conditions of the top level of `filter` (and of its `$and`s): those on the meta value and
/// those on measurements.
fn conditions(
    spec: &BucketSpec,
    filter: &Document,
    meta: &mut Vec<Cond>,
    measured: &mut Vec<Cond>,
) -> Result<(), CommandError> {
    for (k, v) in filter.iter() {
        match (k, v) {
            ("$and", Value::Array(items)) => {
                // The filter is parsed first: its `$and` holds documents only.
                for d in items.iter().filter_map(Value::as_document) {
                    conditions(spec, d, meta, measured)?;
                }
            }
            ("$or", Value::Array(items)) => {
                let mut branches = Vec::new();
                for d in items.iter().filter_map(Value::as_document) {
                    branches.push(Value::Document(bucket_filter(spec, d)?));
                }
                measured.push(("$or".to_owned(), Value::Array(branches)));
            }
            (k, _) if k.starts_with('$') => {
                let shown = match v {
                    Value::Array(items) => Value::Array(
                        items
                            .iter()
                            .map(|i| match i {
                                Value::Document(d) => Value::Document(
                                    d.iter()
                                        .map(|(p, c)| (p.to_owned(), normalized(c)))
                                        .fold(Document::new(), |acc, (p, c)| acc.with(p, c)),
                                ),
                                other => other.clone(),
                            })
                            .collect(),
                    ),
                    other => other.clone(),
                };
                return Err(refuse(Document::new().with(k, shown), Why::Predicate));
            }
            (path, cond) => {
                let (conds, on_meta) = field(spec, path, cond)?;
                if on_meta { meta.extend(conds) } else { measured.extend(conds) }
            }
        }
    }
    Ok(())
}

/// The bucket filter of partial filter `filter` of time series `spec`, or RefDb's refusal.
pub fn bucket_filter(spec: &BucketSpec, filter: &Document) -> Result<Document, CommandError> {
    let (mut meta, mut measured) = (Vec::new(), Vec::new());
    conditions(spec, filter, &mut meta, &mut measured)?;
    let flat = measured.len() <= 1;
    meta.extend(measured);
    Ok(assemble(meta, flat))
}
