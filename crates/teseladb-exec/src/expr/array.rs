//! Array expressions with positional arguments (measured on a `refrouter` 9.0.4): `$arrayElemAt`,
//! `$first`, `$last`, `$concatArrays`, `$in`, `$indexOfArray`, `$isArray`, `$size`,
//! `$reverseArray`, `$slice`, `$range`, `$objectToArray` and `$arrayToObject`. Most of them give
//! `null` for a `null` or missing array; `$size`, `$in` and `$isArray` do not.

use std::cmp::Ordering;

use teseladb_mql::{ErrorCode, MqlError};
use teseladb_bson::{Document, Value};

use super::arith::type_name_of;
use super::values::{display, int32_of, is_number};
use super::{Eval, Expr, Scope, count, error, order};
use teseladb_mql::stream_double;

/// Which operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrayOp {
    ElemAt,
    First,
    Last,
    Concat,
    In,
    IndexOf,
    IsArray,
    Size,
    Reverse,
    Slice,
    Range,
    ObjectToArray,
    ArrayToObject,
}

const NAMES: [(&str, ArrayOp); 13] = [
    ("$arrayElemAt", ArrayOp::ElemAt),
    ("$first", ArrayOp::First),
    ("$last", ArrayOp::Last),
    ("$concatArrays", ArrayOp::Concat),
    ("$in", ArrayOp::In),
    ("$indexOfArray", ArrayOp::IndexOf),
    ("$isArray", ArrayOp::IsArray),
    ("$size", ArrayOp::Size),
    ("$reverseArray", ArrayOp::Reverse),
    ("$slice", ArrayOp::Slice),
    ("$range", ArrayOp::Range),
    ("$objectToArray", ArrayOp::ObjectToArray),
    ("$arrayToObject", ArrayOp::ArrayToObject),
];

/// A location code of this family.
macro_rules! code {
    ($n:literal) => {
        ErrorCode { code: $n, name: concat!("Location", $n) }
    };
}

const ELEM_AT_ARRAY: ErrorCode = code!(28689);
const ELEM_AT_NUMBER: ErrorCode = code!(28690);
const ELEM_AT_INT: ErrorCode = code!(28691);
const CONCAT_TYPES: ErrorCode = code!(28664);
const IN_ARRAY: ErrorCode = code!(40081);
const INDEX_OF_ARRAY: ErrorCode = code!(40090);
const INDEX_OF_INTEGRAL: ErrorCode = code!(9711600);
const INDEX_OF_NEGATIVE: ErrorCode = code!(9711601);
const SIZE_ARRAY: ErrorCode = code!(17124);
const REVERSE_ARRAY: ErrorCode = code!(34435);
const SLICE_ARRAY: ErrorCode = code!(28724);
const SLICE_SECOND_NUMBER: ErrorCode = code!(28725);
const SLICE_SECOND_INT: ErrorCode = code!(28726);
const SLICE_THIRD_NUMBER: ErrorCode = code!(28727);
const SLICE_THIRD_INT: ErrorCode = code!(28728);
const SLICE_THIRD_POSITIVE: ErrorCode = code!(28729);
const RANGE_START_NUMBER: ErrorCode = code!(34443);
const RANGE_START_INT: ErrorCode = code!(34444);
const RANGE_END_NUMBER: ErrorCode = code!(34445);
const RANGE_END_INT: ErrorCode = code!(34446);
const RANGE_STEP_NUMBER: ErrorCode = code!(34447);
const RANGE_STEP_INT: ErrorCode = code!(34448);
const RANGE_STEP_ZERO: ErrorCode = code!(34449);
const OBJECT_TO_ARRAY: ErrorCode = code!(40390);
const TO_OBJECT_ARRAY: ErrorCode = code!(40386);
const TO_OBJECT_KEYS: ErrorCode = code!(40392);
const TO_OBJECT_K_V: ErrorCode = code!(40393);
const TO_OBJECT_K_TYPE: ErrorCode = code!(40394);
const TO_OBJECT_PAIR_KEY: ErrorCode = code!(40395);
const TO_OBJECT_CONSISTENT: ErrorCode = code!(40396);
const TO_OBJECT_CONSISTENT_OBJECT: ErrorCode = code!(40391);
const TO_OBJECT_PAIR_SIZE: ErrorCode = code!(40397);
const TO_OBJECT_FORMAT: ErrorCode = code!(40398);
const TO_OBJECT_NULL_BYTE: ErrorCode = code!(4940400);

/// The type of a result as RefDb names it in a message, `missing` for nothing.
pub(super) fn eval_type(v: &Eval) -> &'static str {
    v.as_ref().map_or("missing", type_name_of)
}

fn nullish(v: &Eval) -> bool {
    matches!(v, None | Some(Value::Null | Value::Undefined))
}

impl ArrayOp {
    /// The operator named `name`.
    pub fn of(name: &str) -> Option<ArrayOp> {
        NAMES.iter().find(|(n, _)| *n == name).map(|(_, a)| *a)
    }

    /// Checks the number of arguments.
    pub fn check(self, name: &str, args: &[Expr]) -> Result<(), MqlError> {
        match self {
            ArrayOp::Concat => Ok(()),
            ArrayOp::ElemAt | ArrayOp::In => count(name, args, 2..=2),
            ArrayOp::IndexOf => count(name, args, 2..=4),
            ArrayOp::Slice | ArrayOp::Range => count(name, args, 2..=3),
            _ => count(name, args, 1..=1),
        }
    }

    /// Evaluates the operator.
    pub fn eval(self, args: &[Expr], doc: &Document, scope: &Scope) -> Result<Eval, MqlError> {
        let vals: Vec<Eval> = args.iter().map(|a| a.eval(doc, scope)).collect::<Result<_, _>>()?;
        match self {
            ArrayOp::ElemAt => elem_at(&vals[0], &vals[1]),
            ArrayOp::First | ArrayOp::Last => first_last(self == ArrayOp::First, &vals[0]),
            ArrayOp::Concat => concat(vals),
            ArrayOp::In => contains(&vals[0], &vals[1], scope.collator()),
            ArrayOp::IndexOf => index_of(&vals, scope.collator()),
            ArrayOp::IsArray => Ok(Some(Value::Boolean(matches!(vals[0], Some(Value::Array(_)))))),
            ArrayOp::Size => match &vals[0] {
                Some(Value::Array(a)) => Ok(Some(size(a.len()))),
                other => Err(error(
                    SIZE_ARRAY,
                    format!(
                        "The argument to $size must be an array, but was of type: {}",
                        eval_type(other)
                    ),
                )),
            },
            ArrayOp::Reverse => match &vals[0] {
                v if nullish(v) => Ok(Some(Value::Null)),
                Some(Value::Array(a)) => Ok(Some(Value::Array(a.iter().rev().cloned().collect()))),
                other => Err(error(
                    REVERSE_ARRAY,
                    format!(
                        "The argument to $reverseArray must be an array, but was of type: {}",
                        eval_type(other)
                    ),
                )),
            },
            ArrayOp::Slice => slice(&vals),
            ArrayOp::Range => range(&vals),
            ArrayOp::ObjectToArray => object_to_array(&vals[0]),
            ArrayOp::ArrayToObject => array_to_object(&vals[0]),
        }
    }
}

/// A length or position as RefDb returns it: an `int`.
fn size(n: usize) -> Value {
    Value::Int32(i32::try_from(n).unwrap_or(i32::MAX))
}

fn elem_at(array: &Eval, index: &Eval) -> Result<Eval, MqlError> {
    if nullish(array) || nullish(index) {
        return Ok(Some(Value::Null));
    }
    let Some(Value::Array(items)) = array else {
        return Err(error(
            ELEM_AT_ARRAY,
            format!("$arrayElemAt's first argument must be an array, but is {}", eval_type(array)),
        ));
    };
    let Some(i) = index.as_ref().filter(|v| is_number(v)) else {
        return Err(error(
            ELEM_AT_NUMBER,
            format!(
                "$arrayElemAt's second argument must be a numeric value, but is {}",
                eval_type(index)
            ),
        ));
    };
    let Some(n) = int32_of(i) else {
        let x = super::values::f64_of(i).unwrap_or(f64::NAN);
        return Err(error(
            ELEM_AT_INT,
            format!(
                "$arrayElemAt's second argument must be representable as a 32-bit integer: {}",
                stream_double(x)
            ),
        ));
    };
    Ok(at(items, i64::from(n)))
}

/// Element `n` of `items`, counting from the end when negative.
fn at(items: &[Value], n: i64) -> Eval {
    let len = items.len() as i64;
    let i = if n < 0 { len + n } else { n };
    usize::try_from(i).ok().and_then(|i| items.get(i)).cloned()
}

fn first_last(first: bool, v: &Eval) -> Result<Eval, MqlError> {
    if nullish(v) {
        return Ok(Some(Value::Null));
    }
    let Some(Value::Array(items)) = v else {
        let name = if first { "$first" } else { "$last" };
        return Err(error(
            ELEM_AT_ARRAY,
            format!("{name}'s argument must be an array, but is {}", eval_type(v)),
        ));
    };
    Ok(if first { items.first() } else { items.last() }.cloned())
}

fn concat(vals: Vec<Eval>) -> Result<Eval, MqlError> {
    let mut out = Vec::new();
    for v in vals {
        match v {
            ref n if nullish(n) => return Ok(Some(Value::Null)),
            Some(Value::Array(items)) => out.extend(items),
            other => {
                return Err(error(
                    CONCAT_TYPES,
                    format!("$concatArrays only supports arrays, not {}", eval_type(&other)),
                ));
            }
        }
    }
    Ok(Some(Value::Array(out)))
}

fn contains(
    x: &Eval,
    array: &Eval,
    col: Option<&dyn teseladb_bson::Collator>,
) -> Result<Eval, MqlError> {
    let Some(Value::Array(items)) = array else {
        return Err(error(
            IN_ARRAY,
            format!("$in requires an array as a second argument, found: {}", eval_type(array)),
        ));
    };
    for item in items {
        if order(x, &Some(item.clone()), col)? == Ordering::Equal {
            return Ok(Some(Value::Boolean(true)));
        }
    }
    Ok(Some(Value::Boolean(false)))
}

/// A starting or ending index of `$indexOfArray`.
fn index_bound(v: &Eval, which: &str) -> Result<usize, MqlError> {
    let n = v.as_ref().and_then(super::values::int64_of).ok_or_else(|| {
        error(
            INDEX_OF_INTEGRAL,
            format!(
                "$indexOfArrayrequires an integral {which} index, found a value of type: {}, with value: {}",
                eval_type(v),
                super::values::display_eval(v)
            ),
        )
    })?;
    usize::try_from(n).map_err(|_| {
        error(
            INDEX_OF_NEGATIVE,
            format!("$indexOfArray requires a nonnegative {which} index, found: {n}"),
        )
    })
}

fn index_of(vals: &[Eval], col: Option<&dyn teseladb_bson::Collator>) -> Result<Eval, MqlError> {
    if nullish(&vals[0]) {
        return Ok(Some(Value::Null));
    }
    let Some(Value::Array(items)) = &vals[0] else {
        return Err(error(
            INDEX_OF_ARRAY,
            format!(
                "$indexOfArray requires an array as a first argument, found: {}",
                eval_type(&vals[0])
            ),
        ));
    };
    let start = match vals.get(2) {
        Some(v) => index_bound(v, "starting")?,
        None => 0,
    };
    let end = match vals.get(3) {
        Some(v) => index_bound(v, "ending")?.min(items.len()),
        None => items.len(),
    };
    for (i, item) in items.iter().enumerate().take(end).skip(start) {
        if order(&vals[1], &Some(item.clone()), col)? == Ordering::Equal {
            return Ok(Some(size(i)));
        }
    }
    Ok(Some(Value::Int32(-1)))
}

fn slice(vals: &[Eval]) -> Result<Eval, MqlError> {
    if vals.iter().any(nullish) {
        return Ok(Some(Value::Null));
    }
    let Some(Value::Array(items)) = &vals[0] else {
        return Err(error(
            SLICE_ARRAY,
            format!(
                "First argument to $slice must be an array, but is of type: {}",
                eval_type(&vals[0])
            ),
        ));
    };
    let int =
        |v: &Eval, which: &str, num: ErrorCode, int_code: ErrorCode| -> Result<i64, MqlError> {
            let Some(x) = v.as_ref().filter(|x| is_number(x)) else {
                let what =
                    if which == "Second" { "must be a numeric value" } else { "must be numeric" };
                return Err(error(
                    num,
                    format!("{which} argument to $slice {what}, but is of type: {}", eval_type(v)),
                ));
            };
            int32_of(x).map(i64::from).ok_or_else(|| {
                error(
                    int_code,
                    format!(
                        "{which} argument to $slice can't be represented as a 32-bit integer: {}",
                        display(x)
                    ),
                )
            })
        };
    let len = items.len() as i64;
    let second = int(&vals[1], "Second", SLICE_SECOND_NUMBER, SLICE_SECOND_INT)?;
    let (start, n) = match vals.get(2) {
        None if second >= 0 => (0, second),
        None => ((len + second).max(0), len),
        Some(third) => {
            let n = int(third, "Third", SLICE_THIRD_NUMBER, SLICE_THIRD_INT)?;
            if n <= 0 {
                return Err(error(
                    SLICE_THIRD_POSITIVE,
                    format!("Third argument to $slice must be positive: {n}"),
                ));
            }
            let start = if second >= 0 { second.min(len) } else { (len + second).max(0) };
            (start, n)
        }
    };
    let (start, n) = (usize::try_from(start).unwrap_or(0), usize::try_from(n).unwrap_or(0));
    Ok(Some(Value::Array(items.iter().skip(start).take(n).cloned().collect())))
}

fn range(vals: &[Eval]) -> Result<Eval, MqlError> {
    let int = |v: &Eval,
               which: &str,
               num: ErrorCode,
               int_code: ErrorCode|
     -> Result<i64, MqlError> {
        let Some(x) = v.as_ref().filter(|x| is_number(x)) else {
            return Err(error(
                num,
                format!(
                    "$range requires a numeric {which} value, found value of type: {}",
                    eval_type(v)
                ),
            ));
        };
        int32_of(x).map(i64::from).ok_or_else(|| {
            error(
                int_code,
                format!(
                    "$range requires a{} {which} value that can be represented as a 32-bit integer, found value: {}",
                    if which == "ending" { "n" } else { "" },
                    display(x)
                ),
            )
        })
    };
    let start = int(&vals[0], "starting", RANGE_START_NUMBER, RANGE_START_INT)?;
    let end = int(&vals[1], "ending", RANGE_END_NUMBER, RANGE_END_INT)?;
    let step = match vals.get(2) {
        Some(v) => int(v, "step", RANGE_STEP_NUMBER, RANGE_STEP_INT)?,
        None => 1,
    };
    if step == 0 {
        return Err(error(RANGE_STEP_ZERO, "$range requires a non-zero step value"));
    }
    let mut out = Vec::new();
    let mut i = start;
    while (step > 0 && i < end) || (step < 0 && i > end) {
        out.push(Value::Int32(i32::try_from(i).unwrap_or(i32::MAX)));
        i += step;
    }
    Ok(Some(Value::Array(out)))
}

fn object_to_array(v: &Eval) -> Result<Eval, MqlError> {
    if nullish(v) {
        return Ok(Some(Value::Null));
    }
    let Some(Value::Document(d)) = v else {
        return Err(error(
            OBJECT_TO_ARRAY,
            format!("$objectToArray requires a document input, found: {}", eval_type(v)),
        ));
    };
    let pairs =
        d.iter().map(|(k, x)| Value::Document(Document::new().with("k", k).with("v", x.clone())));
    Ok(Some(Value::Array(pairs.collect())))
}

/// One key and value of `$arrayToObject`'s input, with the format it was written in (`true` for
/// a pair `[k, v]`).
fn pair(item: &Value) -> Result<(bool, String, Value), MqlError> {
    match item {
        Value::Array(kv) => {
            if kv.len() != 2 {
                return Err(error(
                    TO_OBJECT_PAIR_SIZE,
                    format!(
                        "$arrayToObject requires an array of size 2 arrays,found array of size: {}",
                        kv.len()
                    ),
                ));
            }
            let Value::String(k) = &kv[0] else {
                return Err(error(
                    TO_OBJECT_PAIR_KEY,
                    format!(
                        "$arrayToObject requires an array of key-value pairs, where the key must be of type string. Found key type: {}",
                        type_name_of(&kv[0])
                    ),
                ));
            };
            Ok((true, k.clone(), kv[1].clone()))
        }
        Value::Document(d) => {
            let n = d.iter().count();
            if n != 2 {
                return Err(error(
                    TO_OBJECT_KEYS,
                    format!(
                        "$arrayToObject requires an object keys of 'k' and 'v'. Found incorrect number of keys:{n}"
                    ),
                ));
            }
            let (Some(k), Some(v)) = (d.get("k"), d.get("v")) else {
                return Err(error(
                    TO_OBJECT_K_V,
                    format!(
                        "$arrayToObject requires an object with keys 'k' and 'v'. Missing either or both keys from: {}",
                        display(item)
                    ),
                ));
            };
            let Value::String(k) = k else {
                return Err(error(
                    TO_OBJECT_K_TYPE,
                    format!(
                        "$arrayToObject requires an object with keys 'k' and 'v', where the value of 'k' must be of type string. Found type: {}",
                        type_name_of(k)
                    ),
                ));
            };
            Ok((false, k.clone(), v.clone()))
        }
        other => Err(error(
            TO_OBJECT_FORMAT,
            format!("Unrecognised input type format for $arrayToObject: {}", type_name_of(other)),
        )),
    }
}

fn array_to_object(v: &Eval) -> Result<Eval, MqlError> {
    if nullish(v) {
        return Ok(Some(Value::Null));
    }
    let Some(Value::Array(items)) = v else {
        return Err(error(
            TO_OBJECT_ARRAY,
            format!("$arrayToObject requires an array input, found: {}", eval_type(v)),
        ));
    };
    let mut out: Vec<(String, Value)> = Vec::new();
    let mut format = None;
    for item in items {
        let (is_pair, k, x) = pair(item)?;
        match format {
            None => format = Some(is_pair),
            Some(f) if f != is_pair => {
                let (was, now, code) = if f {
                    ("Array", "object", TO_OBJECT_CONSISTENT)
                } else {
                    ("Object", "array", TO_OBJECT_CONSISTENT_OBJECT)
                };
                return Err(error(
                    code,
                    format!(
                        "$arrayToObject requires a consistent input format. Elements mustall be arrays or all be objects. {was} was detected, now found: {now}"
                    ),
                ));
            }
            Some(_) => {}
        }
        if k.contains('\0') {
            return Err(error(
                TO_OBJECT_NULL_BYTE,
                "Key field cannot contain an embedded null byte",
            ));
        }
        match out.iter_mut().find(|(name, _)| *name == k) {
            Some(slot) => slot.1 = x,
            None => out.push((k, x)),
        }
    }
    let mut d = Document::new();
    for (k, x) in out {
        d.push(k.as_str(), x);
    }
    Ok(Some(Value::Document(d)))
}
