//! The window functions of `$setWindowFields` that are no accumulator (measured on a `refrouter`
//! 9.0.4): `$expMovingAvg`, `$derivative`, `$integral`, `$covariancePop`, `$covarianceSamp`,
//! `$locf` and `$linearFill`.
//!
//! `$expMovingAvg` and the covariances compute in decimal, as RefDb does, and give a double
//! unless a decimal came in; `$derivative` and `$integral` compute in doubles.

use teseladb_bson::{Decimal128, Value};
use teseladb_mql::{DecOp, MqlError, dec_binary, to_decimal128};

use crate::expr::arith::type_name_of;
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

/// A point of `$derivative` and `$integral`: the sort value (a number, or a date in ms) and the
/// input.
pub(super) type Point = (f64, Option<f64>);

/// The rate of change between the first and last points per `unit` ms, `null` with fewer than
/// two.
pub(super) fn derivative(points: &[Point], unit: f64) -> Value {
    let (Some((x0, Some(y0))), Some((x1, Some(y1)))) = (points.first(), points.last()) else {
        return Value::Null;
    };
    if points.len() < 2 || x1 == x0 {
        return Value::Null;
    }
    Value::Double((y1 - y0) / ((x1 - x0) / unit))
}

/// The area under the points by trapezoids, per `unit` ms; one point gives 0 (an integer
/// without a unit, as RefDb starts its sum).
pub(super) fn integral(points: &[Point], unit: Option<f64>) -> Value {
    let pts: Vec<(f64, f64)> = points.iter().filter_map(|(x, y)| y.map(|y| (*x, y))).collect();
    if pts.len() < 2 {
        return if unit.is_some() { Value::Double(0.0) } else { Value::Int32(0) };
    }
    let mut area = 0.0;
    for w in pts.windows(2) {
        area += (w[1].0 - w[0].0) * (w[0].1 + w[1].1) / 2.0;
    }
    Value::Double(area / unit.unwrap_or(1.0))
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
