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

use teseladb_bson::{Document, Value};
use teseladb_mql::timeseries_write::BucketSpec;

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

/// The numeric code of a `$type` name, as RefDb shows `$type` in its normalized form.
fn type_code(v: &Value) -> Value {
    const NAMES: [(&str, i32); 13] = [
        ("double", 1),
        ("string", 2),
        ("object", 3),
        ("array", 4),
        ("binData", 5),
        ("objectId", 7),
        ("bool", 8),
        ("date", 9),
        ("null", 10),
        ("regex", 11),
        ("int", 16),
        ("timestamp", 17),
        ("long", 18),
    ];
    match v {
        Value::String(s) => {
            NAMES.iter().find(|(n, _)| n == s).map_or_else(|| v.clone(), |(_, c)| Value::Int32(*c))
        }
        other => other.clone(),
    }
}

/// Condition `cond` on `path` as RefDb normalizes it in a message.
fn normalized(cond: &Value) -> Value {
    let Value::Document(ops) = cond else {
        return Value::Document(Document::new().with("$eq", cond.clone()));
    };
    if ops.iter().next().is_none_or(|(k, _)| !k.starts_with('$')) {
        return Value::Document(Document::new().with("$eq", cond.clone()));
    }
    let mut out = Document::new();
    for (op, v) in ops.iter() {
        let n = match (op, v) {
            ("$ne", v) => Document::new().with("$not", Document::new().with("$eq", v.clone())),
            ("$nin", v) => Document::new().with("$not", Document::new().with("$in", v.clone())),
            ("$exists", v) if !crate::types::truthy(v).unwrap_or(true) => {
                Document::new().with("$not", Document::new().with("$exists", true))
            }
            ("$type", v) => {
                let codes = match v {
                    Value::Array(items) => items.iter().map(type_code).collect(),
                    one => vec![type_code(one)],
                };
                Document::new().with("$type", Value::Array(codes))
            }
            ("$elemMatch", Value::Document(inner)) => {
                let mut e = Document::new();
                for (k, x) in inner.iter() {
                    match normalized(x) {
                        Value::Document(d) if !k.starts_with('$') => e.push(k, d),
                        other => e.push(k, other),
                    }
                }
                Document::new().with("$elemMatch", e)
            }
            (op, v) => Document::new().with(op, v.clone()),
        };
        for (k, x) in n.iter() {
            out.push(k, x.clone());
        }
    }
    Value::Document(out)
}

/// The conditions on `control.min.<f>`/`control.max.<f>` of one operator on measurement or time
/// field `f`.
fn bounds(spec: &BucketSpec, f: &str, op: &str, v: &Value) -> Option<Vec<Cond>> {
    let (min, max) = (format!("control.min.{f}"), format!("control.max.{f}"));
    let cmp = |path: &str, internal: &str, x: Value| {
        (path.to_owned(), Value::Document(Document::new().with(internal, x)))
    };
    let time = f == spec.time_field;
    let shifted = |x: &Value, sign: i64| match x {
        Value::DateTime(t) if time => {
            Some(Value::DateTime(t.saturating_add(sign * spec.max_span_seconds * 1000)))
        }
        _ => None,
    };
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
        "$eq" => {
            let mut out = vec![cmp(&min, "$_internalExprLte", v.clone())];
            out.extend(shifted(v, -1).map(|s| cmp(&min, "$_internalExprGte", s)));
            out.push(cmp(&max, "$_internalExprGte", v.clone()));
            out.extend(shifted(v, 1).map(|s| cmp(&max, "$_internalExprLte", s)));
            out
        }
        "$exists" if crate::types::truthy(v) == Some(true) => vec![
            (min, Value::Document(Document::new().with("$exists", true))),
            (max, Value::Document(Document::new().with("$exists", true))),
        ],
        _ => return None,
    })
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
    let shown = || Document::new().with(path, normalized(cond));
    let ops: Vec<(String, Value)> = match cond {
        Value::Document(d) if d.iter().next().is_some_and(|(k, _)| k.starts_with('$')) => {
            d.iter().map(|(k, v)| (k.to_owned(), v.clone())).collect()
        }
        other => vec![("$eq".to_owned(), other.clone())],
    };
    let mut out = Vec::new();
    for (op, v) in &ops {
        let compares = matches!(op.as_str(), "$eq" | "$gt" | "$gte" | "$lt" | "$lte");
        // A comparison to null is refused, shown alone (measured on a `refnode` 9.0.4).
        if compares && matches!(v, Value::Null) {
            let one = Document::new().with(path, Document::new().with(op.as_str(), Value::Null));
            return Err(refuse(one, Why::Null));
        }
        if compares && matches!(v, Value::Document(_) | Value::Array(_)) {
            return Err(refuse(shown(), Why::Operand));
        }
        if op == "$in" {
            let Value::Array(items) = v else { return Err(refuse(shown(), Why::Predicate)) };
            let mut branches = Vec::new();
            if items.iter().any(|i| matches!(i, Value::Null)) {
                return Err(refuse(shown(), Why::NullInIn));
            }
            for item in items {
                let conds = bounds(spec, path, "$eq", item)
                    .ok_or_else(|| refuse(shown(), Why::Predicate))?;
                branches.push(Value::Document(assemble(conds, false)));
            }
            out.push(("$or".to_owned(), Value::Array(branches)));
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
                for item in items {
                    if let Value::Document(d) = item {
                        conditions(spec, d, meta, measured)?;
                    }
                }
            }
            ("$or", Value::Array(items)) => {
                let mut branches = Vec::new();
                for item in items {
                    let Value::Document(d) = item else { continue };
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
