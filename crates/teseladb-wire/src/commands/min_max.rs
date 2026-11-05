//! `min` and `max` of `find` (006 FR-008): the keys of the hinted index from `min`, included, to
//! `max`, excluded, in that index's order. Measured on a `refrouter` 9.0.4 on 05-11-2026 (scenario
//! `find-min-max`): both need a `hint` (291), name the fields of the hinted index in its order
//! (51174), and `min` must come before `max` (51175); an empty one bounds nothing.

use std::cmp::Ordering;

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_meta::catalog::Namespace;

use super::hints::{Hinted, planner_error};
use super::plan_exec::PlanIndex;
use crate::error::{
    CommandError, MIN_MAX_INDEX_MISMATCH, MIN_NOT_BEFORE_MAX, NO_QUERY_EXECUTION_PLANS,
};

/// The name of the index of `_id`.
const ID_INDEX: &str = "_id_";

/// The `min` and `max` of a `find`, each empty when not given.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Bounds {
    min: Document,
    max: Document,
}

impl Bounds {
    /// The bounds of a `find` with `min` and `max` (either may be absent); `None` without both.
    pub(crate) fn of(min: Option<Document>, max: Option<Document>) -> Option<Bounds> {
        if min.is_none() && max.is_none() {
            return None;
        }
        Some(Bounds { min: min.unwrap_or_default(), max: max.unwrap_or_default() })
    }
}

/// The key pattern of the hinted index, by name or by pattern; the index of `_id` has none
/// among `indexes`.
fn hinted_pattern(hint: Option<&Value>, indexes: &[PlanIndex]) -> Option<Document> {
    match hint {
        Some(Value::String(name)) if name == ID_INDEX => Some(Document::new().with("_id", 1)),
        Some(Value::String(name)) => {
            indexes.iter().find(|i| i.entry.name == *name).map(|i| i.pattern.clone())
        }
        Some(Value::Document(k)) if !k.is_empty() && k.get("$natural").is_none() => Some(k.clone()),
        _ => None,
    }
}

/// Whether `bound` names the fields of `pattern` in its order; an empty bound names none.
fn names_pattern(bound: &Document, pattern: &Document) -> bool {
    bound.is_empty() || bound.iter().map(|(k, _)| k).eq(pattern.iter().map(|(k, _)| k))
}

/// The pattern the bounds go through: refuses them without a hint, when they do not name the
/// hinted index's fields, and when `min` does not come before `max`.
pub(crate) fn check(
    bounds: &Bounds,
    (ns, read): (&Namespace, &Hinted<'_>),
    hint: Option<&Value>,
    indexes: &[PlanIndex],
) -> Result<Document, CommandError> {
    let Some(pattern) = hinted_pattern(hint, indexes) else {
        let reason = "When using min()/max() a hint of which index to use must be provided";
        return Err(planner_error(ns, read, NO_QUERY_EXECUTION_PLANS, reason)?);
    };
    if !names_pattern(&bounds.min, &pattern) || !names_pattern(&bounds.max, &pattern) {
        let reason = "The index chosen is not compatible with min/max";
        return Err(planner_error(ns, read, MIN_MAX_INDEX_MISMATCH, reason)?);
    }
    let both = !bounds.min.is_empty() && !bounds.max.is_empty();
    if both && compare(&bounds.min, &bounds.max, &pattern)? != Ordering::Less {
        let reason = "The value provided for min() does not come before the value provided for \
                      max() in the hinted index";
        return Err(planner_error(ns, read, MIN_NOT_BEFORE_MAX, reason)?);
    }
    Ok(pattern)
}

/// Whether index key `key` is within `bounds` in the order of `pattern`.
pub(crate) fn within(
    bounds: &Bounds,
    pattern: &Document,
    key: &Document,
) -> Result<bool, CommandError> {
    let from_min = bounds.min.is_empty() || compare(key, &bounds.min, pattern)? != Ordering::Less;
    let before_max = bounds.max.is_empty() || compare(key, &bounds.max, pattern)? == Ordering::Less;
    Ok(from_min && before_max)
}

/// The order of keys `a` and `b`, field by field in the directions of `pattern`.
fn compare(a: &Document, b: &Document, pattern: &Document) -> Result<Ordering, CommandError> {
    let (a_bytes, b_bytes) = (a.to_bytes()?, b.to_bytes()?);
    let (a, b) = (RawDoc::from_bytes(&a_bytes)?, RawDoc::from_bytes(&b_bytes)?);
    let directions = pattern.iter().map(|(_, d)| descending(d));
    for (down, ((_, x), (_, y))) in directions.zip(a.iter().zip(b.iter())) {
        let order = teseladb_bson::compare(x, y, None);
        let order = if down { order.reverse() } else { order };
        if order != Ordering::Equal {
            return Ok(order);
        }
    }
    Ok(Ordering::Equal)
}

/// Whether a field of a key pattern goes from high to low.
fn descending(direction: &Value) -> bool {
    match direction {
        Value::Int32(n) => *n < 0,
        Value::Int64(n) => *n < 0,
        Value::Double(n) => *n < 0.0,
        _ => false,
    }
}
