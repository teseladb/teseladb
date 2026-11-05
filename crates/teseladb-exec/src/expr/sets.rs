//! Set expressions (measured on a `refrouter` 9.0.4): arrays as sets of distinct values in RefDb's
//! order, where `1`, `NumberLong(1)` and `1.0` are one value. `$setUnion` and
//! `$setIntersection` give their result in that order, `$setDifference` keeps the order of its
//! first array; the three give `null` for a `null` or missing operand, and `$setEquals` and
//! `$setIsSubset` refuse it.

use std::cmp::Ordering;

use teseladb_mql::{ErrorCode, MqlError};
use teseladb_bson::{Document, RawValue, Value, compare};

use super::array::eval_type;
use super::values::{sorted, with_raw};
use super::{Eval, Expr, Scope, count, error, truthy};

/// Which operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOp {
    Union,
    Intersection,
    Difference,
    Equals,
    IsSubset,
    AllTrue,
    AnyTrue,
}

const NAMES: [(&str, SetOp); 7] = [
    ("$setUnion", SetOp::Union),
    ("$setIntersection", SetOp::Intersection),
    ("$setDifference", SetOp::Difference),
    ("$setEquals", SetOp::Equals),
    ("$setIsSubset", SetOp::IsSubset),
    ("$allElementsTrue", SetOp::AllTrue),
    ("$anyElementTrue", SetOp::AnyTrue),
];

macro_rules! code {
    ($n:literal) => {
        ErrorCode { code: $n, name: concat!("Location", $n) }
    };
}

const ALL_TRUE_ARRAY: ErrorCode = code!(17040);
const ANY_TRUE_ARRAY: ErrorCode = code!(17041);
const SUBSET_SECOND: ErrorCode = code!(17042);
const UNION_ARRAYS: ErrorCode = code!(17043);
const EQUALS_ARRAYS: ErrorCode = code!(17044);
const EQUALS_TWO: ErrorCode = code!(17045);
const SUBSET_FIRST: ErrorCode = code!(17046);
const INTERSECTION_ARRAYS: ErrorCode = code!(17047);
const DIFFERENCE_FIRST: ErrorCode = code!(17048);
const DIFFERENCE_SECOND: ErrorCode = code!(17049);

fn nullish(v: &Eval) -> bool {
    matches!(v, None | Some(Value::Null | Value::Undefined))
}

/// Whether `x` is in `sorted`, a list in RefDb's order (strings in collation `col`).
fn member(
    sorted: &[RawValue<'_>],
    x: RawValue<'_>,
    col: Option<&dyn teseladb_bson::Collator>,
) -> bool {
    sorted.binary_search_by(|y| compare(*y, x, col)).is_ok()
}

/// The distinct values of `items`, in RefDb's order (the first of each equal run).
fn distinct(
    items: Vec<Value>,
    col: Option<&dyn teseladb_bson::Collator>,
) -> Result<Vec<Value>, MqlError> {
    let all = sorted(items, col)?;
    let keep = with_raw(&[&all], |r| {
        (0..all.len())
            .map(|i| i == 0 || compare(r[0][i - 1], r[0][i], col) != Ordering::Equal)
            .collect::<Vec<bool>>()
    })?;
    Ok(all.into_iter().zip(keep).filter_map(|(v, k)| k.then_some(v)).collect())
}

/// The arrays of `vals`, `None` when one is `null` or missing, or the error of the first that
/// is not an array.
fn arrays(
    vals: Vec<Eval>,
    wrong: impl Fn(&Eval) -> MqlError,
) -> Result<Option<Vec<Vec<Value>>>, MqlError> {
    let mut out = Vec::new();
    for v in vals {
        match v {
            ref n if nullish(n) => return Ok(None),
            Some(Value::Array(items)) => out.push(items),
            other => return Err(wrong(&other)),
        }
    }
    Ok(Some(out))
}

impl SetOp {
    /// The operator named `name`.
    pub fn of(name: &str) -> Option<SetOp> {
        NAMES.iter().find(|(n, _)| *n == name).map(|(_, a)| *a)
    }

    /// Checks the number of arguments.
    pub fn check(self, name: &str, args: &[Expr]) -> Result<(), MqlError> {
        match self {
            SetOp::Union | SetOp::Intersection => Ok(()),
            SetOp::Equals if args.len() < 2 => Err(error(
                EQUALS_TWO,
                format!("$setEquals needs at least two arguments had: {}", args.len()),
            )),
            SetOp::Equals => Ok(()),
            SetOp::Difference | SetOp::IsSubset => count(name, args, 2..=2),
            SetOp::AllTrue | SetOp::AnyTrue => count(name, args, 1..=1),
        }
    }

    /// Evaluates the operator.
    pub fn eval(self, args: &[Expr], doc: &Document, scope: &Scope) -> Result<Eval, MqlError> {
        let vals: Vec<Eval> = args.iter().map(|a| a.eval(doc, scope)).collect::<Result<_, _>>()?;
        let col = scope.collator();
        match self {
            SetOp::Union => {
                let wrong = |v: &Eval| {
                    error(
                        UNION_ARRAYS,
                        format!(
                            "All operands of $setUnion must be arrays. One argument is of type: {}",
                            eval_type(v)
                        ),
                    )
                };
                match arrays(vals, wrong)? {
                    None => Ok(Some(Value::Null)),
                    Some(lists) => Ok(Some(Value::Array(distinct(lists.concat(), col)?))),
                }
            }
            SetOp::Intersection => {
                let wrong = |v: &Eval| {
                    error(
                        INTERSECTION_ARRAYS,
                        format!(
                            "All operands of $setIntersection must be arrays. One argument is of type: {}",
                            eval_type(v)
                        ),
                    )
                };
                match arrays(vals, wrong)? {
                    None => Ok(Some(Value::Null)),
                    Some(lists) => intersection(lists, col),
                }
            }
            SetOp::Difference => difference(&vals[0], &vals[1], col),
            SetOp::Equals => equals(vals, col),
            SetOp::IsSubset => is_subset(&vals[0], &vals[1], col),
            SetOp::AllTrue | SetOp::AnyTrue => {
                let all = self == SetOp::AllTrue;
                let Some(Value::Array(items)) = &vals[0] else {
                    let (code, name) = if all {
                        (ALL_TRUE_ARRAY, "$allElementsTrue")
                    } else {
                        (ANY_TRUE_ARRAY, "$anyElementTrue")
                    };
                    return Err(error(
                        code,
                        format!(
                            "{name}'s argument must be an array, but is {}",
                            eval_type(&vals[0])
                        ),
                    ));
                };
                let mut truths = items.iter().map(|i| truthy(&Some(i.clone())));
                let r = if all { truths.all(|t| t) } else { truths.any(|t| t) };
                Ok(Some(Value::Boolean(r)))
            }
        }
    }
}

/// The values in every list, as RefDb keeps them: each later list's distinct values meet
/// the values kept so far by walking the smaller of the two (the kept ones on a tie), so of
/// equal values (in collation `col`) the smaller set's are shown.
fn intersection(
    lists: Vec<Vec<Value>>,
    col: Option<&dyn teseladb_bson::Collator>,
) -> Result<Eval, MqlError> {
    let Some((first, rest)) = lists.split_first() else {
        return Ok(Some(Value::Array(Vec::new())));
    };
    let mut kept = distinct(first.clone(), col)?;
    for list in rest {
        if kept.is_empty() {
            break;
        }
        let next = distinct(list.clone(), col)?;
        let (walked, other) = if kept.len() > next.len() { (next, kept) } else { (kept, next) };
        let keep = with_raw(&[&walked, &other], |r| {
            r[0].iter().map(|x| member(&r[1], *x, col)).collect::<Vec<bool>>()
        })?;
        kept = walked.into_iter().zip(keep).filter_map(|(v, k)| k.then_some(v)).collect();
    }
    Ok(Some(Value::Array(kept)))
}

fn difference(
    a: &Eval,
    b: &Eval,
    col: Option<&dyn teseladb_bson::Collator>,
) -> Result<Eval, MqlError> {
    if nullish(a) || nullish(b) {
        return Ok(Some(Value::Null));
    }
    let wrong = |which: &str, code, v: &Eval| {
        error(
            code,
            format!(
                "both operands of $setDifference must be arrays. {which} argument is of type: {}",
                eval_type(v)
            ),
        )
    };
    let Some(Value::Array(first)) = a else { return Err(wrong("First", DIFFERENCE_FIRST, a)) };
    let Some(Value::Array(second)) = b else { return Err(wrong("Second", DIFFERENCE_SECOND, b)) };
    let second = sorted(second.clone(), col)?;
    let keep = with_raw(&[first, &second], |r| {
        let mut seen: Vec<RawValue<'_>> = Vec::new();
        r[0].iter()
            .map(|x| {
                let new = !member(&r[1], *x, col)
                    && !seen.iter().any(|s| compare(*s, *x, col) == Ordering::Equal);
                if new {
                    seen.push(*x);
                }
                new
            })
            .collect::<Vec<bool>>()
    })?;
    let out = first.iter().zip(keep).filter(|(_, k)| *k).map(|(v, _)| v.clone());
    Ok(Some(Value::Array(out.collect())))
}

/// The distinct values of each array, sorted, to compare as sets.
fn as_sets(
    lists: &[&Vec<Value>],
    col: Option<&dyn teseladb_bson::Collator>,
) -> Result<Vec<Vec<Value>>, MqlError> {
    lists.iter().map(|l| distinct((*l).clone(), col)).collect()
}

fn equals(vals: Vec<Eval>, col: Option<&dyn teseladb_bson::Collator>) -> Result<Eval, MqlError> {
    let mut lists = Vec::new();
    for (i, v) in vals.iter().enumerate() {
        let Some(Value::Array(items)) = v else {
            return Err(error(
                EQUALS_ARRAYS,
                format!(
                    "All operands of $setEquals must be arrays. {}-th argument is of type: {}",
                    i + 1,
                    eval_type(v)
                ),
            ));
        };
        lists.push(items);
    }
    let sets = as_sets(&lists, col)?;
    let slices: Vec<&[Value]> = sets.iter().map(Vec::as_slice).collect();
    let same = with_raw(&slices, |r| {
        r[1..].iter().all(|o| {
            o.len() == r[0].len()
                && o.iter().zip(&r[0]).all(|(x, y)| compare(*x, *y, col) == Ordering::Equal)
        })
    })?;
    Ok(Some(Value::Boolean(same)))
}

fn is_subset(a: &Eval, b: &Eval, col: Option<&dyn teseladb_bson::Collator>) -> Result<Eval, MqlError> {
    let wrong = |which: &str, code, v: &Eval| {
        error(
            code,
            format!(
                "both operands of $setIsSubset must be arrays. {which} argument is of type: {}",
                eval_type(v)
            ),
        )
    };
    let Some(Value::Array(first)) = a else { return Err(wrong("First", SUBSET_FIRST, a)) };
    let Some(Value::Array(second)) = b else { return Err(wrong("Second", SUBSET_SECOND, b)) };
    let second = sorted(second.clone(), col)?;
    let all = with_raw(&[first, &second], |r| r[0].iter().all(|x| member(&r[1], *x, col)))?;
    Ok(Some(Value::Boolean(all)))
}
