//! Array operators: `$push` with `$each`, `$position`, `$sort` and `$slice`, `$addToSet` with
//! `$each`, `$pop`, `$pull` and `$pullAll`.

use std::cmp::Ordering;

use teseladb_bson::{Document, RawDoc, RawValue, Value};

use super::positional::Slot;
use super::tree::MVal;
use super::{Action, Entry, UpdateContext};
use crate::error::{Result, bad_value, failed_to_parse, type_mismatch};
use crate::matcher::{Eval, Filter};
use crate::ops;
use crate::parse::{self, Expr, FilterUse, Pred};
use crate::types;
use crate::value::{OwnedDoc, OwnedValue, cmp, exact_i64, is_number};

/// The condition of `$pull`.
#[derive(Debug)]
pub(crate) enum PullCond {
    /// Elements equal to the value.
    Value(OwnedValue),
    /// Elements the operators match (`{$gte: 6}`).
    Pred(Pred),
    /// Documents the filter matches (`{score: 8}`).
    Filter(Filter),
}

/// `$push` and its modifiers.
#[derive(Debug)]
pub(crate) struct PushSpec {
    each: Vec<Value>,
    position: Option<i64>,
    sort: Option<SortSpec>,
    slice: Option<i64>,
}

impl PushSpec {
    /// Whether it only adds its elements at the end (no `$position`, `$sort` or `$slice`).
    pub(crate) fn appends(&self) -> bool {
        self.position.is_none() && self.sort.is_none() && self.slice.is_none()
    }
}

#[derive(Debug)]
enum SortSpec {
    Whole(bool),
    Fields(Vec<(String, bool)>),
}

pub(super) fn parse(op: &str, path: &str, v: RawValue<'_>) -> Result<Action> {
    Ok(match op {
        ops::ADD_TO_SET => Action::AddToSet(add_to_set(v)?),
        ops::POP => match crate::int64::exact_int64(v, path).map_err(failed_to_parse)? {
            1 => Action::Pop { first: false },
            -1 => Action::Pop { first: true },
            n => return Err(failed_to_parse(format!("$pop expects 1 or -1, found: {n}"))),
        },
        ops::PULL => Action::Pull(pull(path, v)?),
        ops::PULL_ALL => {
            let RawValue::Array(arr) = v else {
                return Err(bad_value(format!(
                    "$pullAll requires an array argument but was given a {}",
                    types::name_of(v.element_type())
                )));
            };
            Action::PullAll(arr.iter().map(OwnedValue::from_raw).collect::<Result<_>>()?)
        }
        _ => Action::Push(push(v)?),
    })
}

fn add_to_set(v: RawValue<'_>) -> Result<Vec<Value>> {
    let RawValue::Document(d) = v else { return Ok(vec![Value::from(v)]) };
    let Some(each) = crate::matcher::field(d, ops::EACH) else { return Ok(vec![Value::from(v)]) };
    if d.iter().count() > 1 {
        return Err(bad_value(format!(
            "Found unexpected fields after $each in $addToSet: {}",
            crate::fmt::shell(RawValue::Document(d))
        )));
    }
    let RawValue::Array(arr) = each else {
        return Err(type_mismatch(format!(
            "The argument to $each in $addToSet must be an array but it was of type {}",
            types::name_of(each.element_type())
        )));
    };
    Ok(arr.iter().map(Value::from).collect())
}

/// The query operators of a field.
const FIELD_OPERATORS: [&str; 27] = [
    ops::EQ,
    ops::NE,
    ops::GT,
    ops::GTE,
    ops::LT,
    ops::LTE,
    ops::IN,
    ops::NIN,
    ops::EXISTS,
    ops::TYPE,
    ops::MOD,
    ops::REGEX,
    ops::OPTIONS,
    ops::ALL,
    ops::ELEM_MATCH,
    ops::SIZE,
    ops::NOT,
    ops::BITS_ALL_CLEAR,
    ops::BITS_ALL_SET,
    ops::BITS_ANY_CLEAR,
    ops::BITS_ANY_SET,
    ops::GEO_WITHIN,
    ops::GEO_INTERSECTS,
    ops::NEAR,
    ops::NEAR_SPHERE,
    ops::MAX_DISTANCE,
    ops::MIN_DISTANCE,
];

fn pull(path: &str, v: RawValue<'_>) -> Result<PullCond> {
    let RawValue::Document(d) = v else { return Ok(PullCond::Value(OwnedValue::from_raw(v)?)) };
    // A condition on the elements when it starts with an operator of a field; anything else is
    // a filter of the documents, whose unknown operators are top-level ones (measured).
    let operators = d.iter().next().is_some_and(|(k, _)| FIELD_OPERATORS.contains(&k));
    if operators {
        let wrapper = OwnedDoc::new(&Document::new().with(path, Value::from(v)))?;
        return match parse::parse(wrapper.get(), FilterUse::Nested)? {
            Expr::Field { pred, .. } => Ok(PullCond::Pred(pred)),
            _ => Ok(PullCond::Value(OwnedValue::from_raw(v)?)),
        };
    }
    Ok(PullCond::Filter(Filter::parse(d, FilterUse::Nested)?))
}

fn push(v: RawValue<'_>) -> Result<PushSpec> {
    let plain = PushSpec { each: vec![Value::from(v)], position: None, sort: None, slice: None };
    let RawValue::Document(d) = v else { return Ok(plain) };
    if crate::matcher::field(d, ops::EACH).is_none() {
        return Ok(plain);
    }
    let mut spec = PushSpec { each: Vec::new(), position: None, sort: None, slice: None };
    for (k, arg) in d.iter() {
        match k {
            ops::EACH => {
                let RawValue::Array(arr) = arg else {
                    return Err(bad_value(format!(
                        "The argument to $each in $push must be an array but it was of type: {}",
                        types::name_of(arg.element_type())
                    )));
                };
                spec.each = arr.iter().map(Value::from).collect();
            }
            ops::SLICE => {
                spec.slice = Some(integer(arg).ok_or_else(|| {
                    bad_value(format!(
                        "The value for $slice must be an integer value but was given type: {}",
                        types::name_of(arg.element_type())
                    ))
                })?);
            }
            ops::POSITION => {
                spec.position = Some(integer(arg).ok_or_else(|| {
                    bad_value(format!(
                        "The value for $position must be an integer value, not of type: {}",
                        types::name_of(arg.element_type())
                    ))
                })?);
            }
            ops::SORT => spec.sort = Some(sort(arg)?),
            _ => return Err(bad_value(format!("Unrecognized clause in $push: {k}"))),
        }
    }
    Ok(spec)
}

fn integer(v: RawValue<'_>) -> Option<i64> {
    is_number(&v).then(|| exact_i64(&v)).flatten()
}

fn direction(v: RawValue<'_>) -> Option<bool> {
    match integer(v) {
        Some(1) => Some(true),
        Some(-1) => Some(false),
        _ => None,
    }
}

fn sort(v: RawValue<'_>) -> Result<SortSpec> {
    let invalid = || {
        bad_value(
            "The $sort is invalid: use 1/-1 to sort the whole element, or {field:1/-1} to sort embedded fields",
        )
    };
    match v {
        RawValue::Document(d) => {
            if d.is_empty() {
                return Err(bad_value(
                    "The sort pattern is empty when it should be a set of fields.",
                ));
            }
            let mut fields = Vec::new();
            for (k, dir) in d.iter() {
                if k.split('.').any(str::is_empty) {
                    return Err(bad_value(format!(
                        "The $sort field is a dotted field but has an empty part: {k}"
                    )));
                }
                let dir = direction(dir)
                    .ok_or_else(|| bad_value("The $sort element value must be either 1 or -1"))?;
                fields.push((k.to_owned(), dir));
            }
            Ok(SortSpec::Fields(fields))
        }
        v if crate::value::is_number(&v) => direction(v)
            .map(SortSpec::Whole)
            .ok_or_else(|| bad_value("The $sort element value must be either 1 or -1")),
        _ => Err(invalid()),
    }
}

/// Applies an array operator at `slot`.
pub(super) fn apply(
    e: &Entry,
    mut slot: Slot<'_>,
    ctx: &UpdateContext<'_>,
    original: RawDoc<'_>,
) -> Result<()> {
    let coll = ctx.matcher.collation;
    let name = slot.key_text();
    let current = slot.get().map(|v| v.expanded().into_owned());
    let items = match (&current, &e.action) {
        (Some(MVal::Arr(items)), _) => items.clone(),
        (None, Action::Push(_) | Action::AddToSet(_)) => Vec::new(),
        (None, _) => return Ok(()),
        (Some(other), action) => {
            let ty = other.type_name();
            return Err(match action {
                Action::AddToSet(_) => bad_value(format!(
                    "Cannot apply $addToSet to non-array field. Field named '{name}' has non-array type {ty}"
                )),
                Action::Pop { .. } => type_mismatch(format!(
                    "Path '{name}' contains an element of non-array type '{ty}'"
                )),
                Action::Push(_) => bad_value(format!(
                    "The field '{name}' must be an array but is of type {ty} in document {}",
                    id_text(original)
                )),
                _ => bad_value("Cannot apply $pull to a non-array value"),
            });
        }
    };
    let result = match &e.action {
        Action::AddToSet(values) => {
            let mut out = items;
            for v in values {
                let ov = OwnedValue::new(v.clone())?;
                let mut present = false;
                for x in &out {
                    present |= cmp(x.owned()?.get(), ov.get(), coll) == Ordering::Equal;
                }
                if !present {
                    out.push(MVal::from_value(v));
                }
            }
            out
        }
        Action::Pop { first } => {
            let mut out = items;
            if !out.is_empty() {
                if *first {
                    out.remove(0)
                } else {
                    out.remove(out.len() - 1)
                };
            }
            out
        }
        Action::Pull(cond) => {
            let mut out = Vec::new();
            for x in items {
                if !pulls(cond, &x, ctx)? {
                    out.push(x);
                }
            }
            out
        }
        Action::PullAll(values) => {
            let mut out = Vec::new();
            for x in items {
                let o = x.owned()?;
                if !values.iter().any(|v| cmp(o.get(), v.get(), coll) == Ordering::Equal) {
                    out.push(x);
                }
            }
            out
        }
        Action::Push(spec) => pushed(spec, items, ctx)?,
        _ => return Ok(()),
    };
    slot.set(MVal::Arr(result))
}

fn id_text(original: RawDoc<'_>) -> String {
    let d = match original.get(ops::ID) {
        Some(id) => Document::new().with(ops::ID, Value::from(id)),
        None => Document::new(),
    };
    OwnedDoc::new(&d).map(|o| crate::fmt::shell_doc(o.get())).unwrap_or_default()
}

fn pulls(cond: &PullCond, x: &MVal, ctx: &UpdateContext<'_>) -> Result<bool> {
    let o = x.owned()?;
    Ok(match cond {
        PullCond::Value(v) => cmp(o.get(), v.get(), ctx.matcher.collation) == Ordering::Equal,
        PullCond::Pred(p) => Eval::new(&ctx.matcher, false).leaf(p, Some(o.get()), None, false)?,
        PullCond::Filter(f) => match o.get() {
            RawValue::Document(d) => f.matches(d, &ctx.matcher)?,
            _ => false,
        },
    })
}

fn pushed(spec: &PushSpec, mut items: Vec<MVal>, ctx: &UpdateContext<'_>) -> Result<Vec<MVal>> {
    let len = i64::try_from(items.len()).unwrap_or(i64::MAX);
    let at = match spec.position {
        None => len,
        Some(p) if p >= 0 => p.min(len),
        Some(p) => (len + p).max(0),
    };
    let at = usize::try_from(at).unwrap_or(items.len());
    let tail = items.split_off(at);
    items.extend(spec.each.iter().map(MVal::from_value));
    items.extend(tail);
    if let Some(sort) = &spec.sort {
        let mut keyed = Vec::new();
        for x in items {
            keyed.push((sort_key(sort, &x)?, x));
        }
        let coll = ctx.matcher.collation;
        keyed.sort_by(|(a, _), (b, _)| {
            for (i, (ka, kb)) in a.iter().zip(b).enumerate() {
                let asc = match sort {
                    SortSpec::Whole(asc) => *asc,
                    SortSpec::Fields(f) => f[i].1,
                };
                let o = cmp(ka.get(), kb.get(), coll);
                if o != Ordering::Equal {
                    return if asc { o } else { o.reverse() };
                }
            }
            Ordering::Equal
        });
        items = keyed.into_iter().map(|(_, x)| x).collect();
    }
    if let Some(n) = spec.slice {
        let len = i64::try_from(items.len()).unwrap_or(i64::MAX);
        if n >= 0 {
            items.truncate(usize::try_from(n.min(len)).unwrap_or(0));
        } else {
            let drop = usize::try_from((len + n).max(0)).unwrap_or(0);
            items.drain(..drop);
        }
    }
    Ok(items)
}

/// The values an element sorts by: itself, or its fields (missing ones as null).
fn sort_key(sort: &SortSpec, x: &MVal) -> Result<Vec<OwnedValue>> {
    let o = x.owned()?;
    match sort {
        SortSpec::Whole(_) => Ok(vec![o]),
        SortSpec::Fields(fields) => fields
            .iter()
            .map(|(path, _)| match o.get() {
                RawValue::Document(d) => {
                    OwnedValue::from_raw(d.get(path).unwrap_or(RawValue::Null))
                }
                _ => OwnedValue::new(Value::Null),
            })
            .collect(),
    }
}
