//! The window functions of `$setWindowFields` that are no accumulator (measured on a `refrouter`
//! 9.0.4): `$expMovingAvg`, `$derivative`, `$integral`, `$covariancePop`, `$covarianceSamp`,
//! `$locf`, `$linearFill` and `$minMaxScaler`.
//!
//! `$expMovingAvg` and the covariances compute in decimal, as RefDb does, and give a double
//! unless a decimal came in; `$derivative` and `$integral` compute in doubles.

use teseladb_bson::{Decimal128, Value};
use teseladb_mql::{DecOp, ErrorCode, MqlError, dec_binary, to_decimal128};

use crate::expr::arith::{add, divide, multiply, subtract, type_name_of};
use crate::expr::codes::BAD_DIVISION;
use crate::expr::stats::Total;
use crate::expr::values::is_number;
use crate::pipeline::errors::{TYPE_MISMATCH, error};

/// A number as a decimal (a double with 15 digits, as RefDb converts it).
pub(super) fn decimal_of(v: &Value) -> Option<Decimal128> {
    match v {
        Value::Decimal128(d) => Some(*d),
        Value::Double(x) if x.is_finite() => {
            // Rounded to 15 significant digits, trailing zeros dropped (`2.5`, not `2.50…0`).
            let text = format!("{x:.14e}");
            let (mantissa, exp) = text.split_once('e')?;
            let mantissa = match mantissa.split_once('.') {
                Some((int, frac)) => {
                    let frac = frac.trim_end_matches('0');
                    if frac.is_empty() { int.to_owned() } else { format!("{int}.{frac}") }
                }
                None => mantissa.to_owned(),
            };
            format!("{mantissa}E{exp}").parse().ok()
        }
        Value::Int32(_) | Value::Int64(_) | Value::Double(_) => to_decimal128(v),
        _ => None,
    }
}

fn dec(op: DecOp, a: Decimal128, b: Decimal128) -> Decimal128 {
    dec_binary(op, &Value::Decimal128(a), &Value::Decimal128(b)).unwrap_or_else(Decimal128::nan)
}

/// A decimal result: as it is when a decimal came in, else the nearest double.
fn result(d: Decimal128, decimal: bool) -> Value {
    if decimal { Value::Decimal128(d) } else { Value::Double(teseladb_mql::decimal_to_f64(d)) }
}

/// The weight of `$expMovingAvg`: `alpha`, or `2 / (N + 1)`.
#[derive(Clone, Copy, Debug)]
pub(super) enum Weight {
    N(i64),
    Alpha(f64),
}

/// The moving averages of `inputs` in order: each number moves the average; anything else
/// gives the average so far (`null` before the first number).
pub(super) fn exp_moving_avg(inputs: &[Option<Value>], w: Weight) -> Vec<Value> {
    let one = Decimal128::from_parts(false, 1, 0).unwrap_or_else(Decimal128::nan);
    let alpha = match w {
        Weight::N(n) => dec(
            DecOp::Div,
            Decimal128::from_parts(false, 2, 0).unwrap_or_else(Decimal128::nan),
            Decimal128::from_parts(false, u128::from((n + 1).unsigned_abs()), 0)
                .unwrap_or_else(Decimal128::nan),
        ),
        Weight::Alpha(a) => decimal_of(&Value::Double(a)).unwrap_or_else(Decimal128::nan),
    };
    let rest = dec(DecOp::Sub, one, alpha);
    let mut ema: Option<Decimal128> = None;
    let mut decimal = false;
    let mut out = Vec::with_capacity(inputs.len());
    for v in inputs {
        if let Some(x) = v.as_ref().and_then(decimal_of) {
            decimal |= matches!(v, Some(Value::Decimal128(_)));
            ema = Some(match ema {
                None => x,
                Some(e) => dec(DecOp::Add, dec(DecOp::Mul, x, alpha), dec(DecOp::Mul, e, rest)),
            });
        }
        out.push(ema.map_or(Value::Null, |e| result(e, decimal)));
    }
    out
}

/// An arithmetic operation of the covariances.
#[derive(Clone, Copy)]
enum NumOp {
    Add,
    Sub,
    Mul,
    Div,
}

/// `a op b` with RefDb's numeric types: a decimal makes a decimal (a double converted with 15
/// digits), a double or a division a double, integers otherwise.
fn num(a: &Value, b: &Value, op: NumOp) -> Value {
    if matches!(a, Value::Decimal128(_)) || matches!(b, Value::Decimal128(_)) {
        let (Some(x), Some(y)) = (decimal_of(a), decimal_of(b)) else { return Value::Null };
        let d = match op {
            NumOp::Add => DecOp::Add,
            NumOp::Sub => DecOp::Sub,
            NumOp::Mul => DecOp::Mul,
            NumOp::Div => DecOp::Div,
        };
        return Value::Decimal128(dec(d, x, y));
    }
    let ints = |v: &Value| match v {
        Value::Int32(n) => Some(i64::from(*n)),
        Value::Int64(n) => Some(*n),
        _ => None,
    };
    if let (Some(x), Some(y)) = (ints(a), ints(b)) {
        let r = match op {
            NumOp::Add => x.checked_add(y),
            NumOp::Sub => x.checked_sub(y),
            NumOp::Mul => x.checked_mul(y),
            NumOp::Div => None,
        };
        if let Some(r) = r {
            return i32::try_from(r).map_or(Value::Int64(r), Value::Int32);
        }
    }
    let (x, y) = (
        crate::expr::values::f64_of(a).unwrap_or(0.0),
        crate::expr::values::f64_of(b).unwrap_or(0.0),
    );
    Value::Double(match op {
        NumOp::Add => x + y,
        NumOp::Sub => x - y,
        NumOp::Mul => x * y,
        NumOp::Div => x / y,
    })
}

/// The covariance of the numeric pairs of `pairs` (`sample`: divided by n − 1), `null` with too
/// few: `c += (x − mean of x before) × (y − mean of y)`, the means as running sums over the
/// count, in RefDb's numeric types.
pub(super) fn covariance(pairs: &[(Option<Value>, Option<Value>)], sample: bool) -> Value {
    let (mut sum_x, mut sum_y, mut c) = (Value::Int32(0), Value::Int32(0), Value::Int32(0));
    let mut count: i64 = 0;
    for (x, y) in pairs {
        let (Some(xv), Some(yv)) = (
            x.as_ref().filter(|v| decimal_of(v).is_some()),
            y.as_ref().filter(|v| decimal_of(v).is_some()),
        ) else {
            continue;
        };
        let mean_x = if count == 0 {
            Value::Int32(0)
        } else {
            num(&sum_x, &Value::Int64(count), NumOp::Div)
        };
        count += 1;
        sum_x = num(&sum_x, xv, NumOp::Add);
        sum_y = num(&sum_y, yv, NumOp::Add);
        let mean_y = num(&sum_y, &Value::Int64(count), NumOp::Div);
        let term = num(&num(xv, &mean_x, NumOp::Sub), &num(yv, &mean_y, NumOp::Sub), NumOp::Mul);
        c = num(&c, &term, NumOp::Add);
    }
    let divisor = if sample { count - 1 } else { count };
    if divisor <= 0 {
        return Value::Null;
    }
    num(&c, &Value::Int64(divisor), NumOp::Div)
}

/// A point of `$derivative` and `$integral`: the sort field's value and the input's.
pub(super) type Point = (Option<Value>, Option<Value>);

/// A location code.
macro_rules! loc {
    ($n:literal) => {
        ErrorCode { code: $n, name: concat!("Location", $n) }
    };
}

const DERIVATIVE_NOT_DATE: ErrorCode = loc!(5624900);
const DERIVATIVE_UNIT: ErrorCode = loc!(5624901);
const DERIVATIVE_NOT_NUMERIC: ErrorCode = loc!(5624902);
const DERIVATIVE_NULL: ErrorCode = loc!(5624903);
const INTEGRAL_VECTOR: ErrorCode = loc!(5423900);
const INTEGRAL_NOT_DATE: ErrorCode = loc!(5423901);
const INTEGRAL_NOT_NUMERIC: ErrorCode = loc!(5423902);

fn is_date(v: &Option<Value>) -> bool {
    matches!(v, Some(Value::DateTime(_)))
}

fn is_numeric(v: &Option<Value>) -> bool {
    v.as_ref().is_some_and(is_number)
}

/// `$derivative` between the first point `left` and the last `right` of a window, per `unit`
/// ms (both sort values dates) or per unit of the numeric sort values: the inputs' difference
/// over the sort values' with `$subtract` and `$divide`, `null` when the sort values are equal.
pub(super) fn derivative(
    left: &Point,
    right: &Point,
    unit: Option<i64>,
) -> Result<Value, MqlError> {
    if unit.is_some() {
        if !(is_date(&left.0) && is_date(&right.0)) {
            return Err(error(
                DERIVATIVE_NOT_DATE,
                "$derivative with 'unit' expects the sortBy field to be a Date",
            ));
        }
    } else {
        if is_date(&left.0) || is_date(&right.0) {
            return Err(error(
                DERIVATIVE_UNIT,
                "$derivative where the sortBy is a Date requires an 'unit'",
            ));
        }
        if !(is_numeric(&left.0) && is_numeric(&right.0)) {
            return Err(error(
                DERIVATIVE_NOT_NUMERIC,
                "$derivative (with no 'unit') expects the sortBy field to be numeric",
            ));
        }
    }
    let run = subtract(&right.0, &left.0)?;
    let rise = subtract(&right.1, &left.1)?;
    if matches!(rise, None | Some(Value::Null | Value::Undefined)) {
        return Err(error(DERIVATIVE_NULL, "$derivative input must not be null or missing"));
    }
    let result = match divide(&rise, &run) {
        // A window of one document has no run.
        Err(e) if e.code == BAD_DIVISION => return Ok(Value::Null),
        other => other?,
    };
    let result = match unit {
        Some(ms) => multiply(&[result, Some(Value::Int64(ms))])?,
        None => result,
    };
    Ok(result.unwrap_or(Value::Null))
}

/// `$integral` of a window's points by trapezoids, summed as `$sum` adds (an integer 0 for one
/// point), divided by `unit` ms when the sort values are dates.
pub(super) fn integral(points: &[Point], unit: Option<i64>) -> Result<Value, MqlError> {
    for (x, y) in points {
        if !is_numeric(y) || !(is_numeric(x) || is_date(x)) {
            return Err(error(
                INTEGRAL_VECTOR,
                "The input value of $integral window function must be a vector of 2 value, the first value must be numeric or date type and the second must be numeric.",
            ));
        }
        if unit.is_some() && !is_date(x) {
            return Err(error(
                INTEGRAL_NOT_DATE,
                "$integral with 'unit' expects the sortBy field to be a Date",
            ));
        }
        if unit.is_none() && is_date(x) {
            return Err(error(
                INTEGRAL_NOT_NUMERIC,
                "$integral (with no 'unit') expects the sortBy field to be numeric",
            ));
        }
    }
    let mut total = Total::default();
    for w in points.windows(2) {
        let height = add(&[w[0].1.clone(), w[1].1.clone()])?;
        let width = subtract(&w[1].0, &w[0].0)?;
        let area = divide(&multiply(&[height, width])?, &Some(Value::Double(2.0)))?;
        total.add(&area.unwrap_or(Value::Null));
    }
    let sum = Some(total.sum());
    Ok(match unit {
        Some(ms) => divide(&sum, &Some(Value::Int64(ms)))?,
        None => sum,
    }
    .unwrap_or(Value::Null))
}

/// `$locf`: each value, or the last value before it that is neither `null` nor missing.
pub(super) fn locf(values: &[Option<Value>]) -> Vec<Value> {
    let mut last = Value::Null;
    values
        .iter()
        .map(|v| {
            if let Some(x) = v.as_ref().filter(|x| !matches!(x, Value::Null)) {
                last = x.clone();
            }
            last.clone()
        })
        .collect()
}

/// `$linearFill`: each `null` or missing value between two numbers on the line between them
/// (by the sort positions `xs`); the others as they are.
pub(super) fn linear_fill(values: &[Option<Value>], xs: &[f64]) -> Result<Vec<Value>, MqlError> {
    let mut known: Vec<Option<f64>> = Vec::with_capacity(values.len());
    for v in values {
        known.push(match v {
            None | Some(Value::Null) => None,
            Some(x) => match crate::expr::values::f64_of(x) {
                Some(f) => Some(f),
                None => {
                    return Err(error(
                        TYPE_MISMATCH,
                        format!(
                            "Value to be filled must be numeric or nullish, but found {}",
                            type_name_of(x)
                        ),
                    ));
                }
            },
        });
    }
    let mut out = Vec::with_capacity(values.len());
    for (i, v) in values.iter().enumerate() {
        if known[i].is_some() {
            out.push(v.clone().unwrap_or(Value::Null));
            continue;
        }
        let before = (0..i).rev().find(|j| known[*j].is_some());
        let after = (i + 1..values.len()).find(|j| known[*j].is_some());
        out.push(match (before, after) {
            (Some(a), Some(b)) => {
                let (ya, yb) = (known[a].unwrap_or(0.0), known[b].unwrap_or(0.0));
                Value::Double(ya + (yb - ya) * (xs[i] - xs[a]) / (xs[b] - xs[a]))
            }
            _ => Value::Null,
        });
    }
    Ok(out)
}

/// `x` scaled from the range of `window` (its least and greatest values in BSON order, `NaN`
/// first) to `[min, max]` with the engine's arithmetic: `min` itself when the window holds one
/// value (measured on a `refrouter` 9.0.4).
pub(super) fn min_max_scaled(
    x: &Value,
    window: &[Value],
    (min, max): (&Value, &Value),
) -> Result<Value, MqlError> {
    let mut ends: Option<(&Value, &Value)> = None;
    for v in window {
        ends = Some(match ends {
            None => (v, v),
            Some((lo, hi)) => {
                (if before(v, lo)? { v } else { lo }, if before(hi, v)? { v } else { hi })
            }
        });
    }
    let Some((lo, hi)) = ends.filter(|(lo, hi)| !matches!(before(lo, hi), Ok(false))) else {
        return Ok(min.clone());
    };
    let some = |v: &Value| Some(v.clone());
    let share = divide(&subtract(&some(x), &some(lo))?, &subtract(&some(hi), &some(lo))?)?;
    let scaled = multiply(&[share, subtract(&some(max), &some(min))?])?;
    Ok(add(&[scaled, some(min)])?.unwrap_or(Value::Null))
}

/// Whether `a` comes before `b` in BSON order.
fn before(a: &Value, b: &Value) -> Result<bool, MqlError> {
    Ok(crate::expr::order(&Some(a.clone()), &Some(b.clone()), None)?.is_lt())
}
