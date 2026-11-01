//! Arithmetic expressions with RefDb's numeric types (measured on a `refrouter` 9.0.4): an `int`
//! result that overflows becomes a `long`, a `long` that overflows becomes a `double`, a
//! `double` operand gives a `double` and a `decimal` one a `decimal`; `null` or a missing operand
//! gives `null`. `$add` and `$subtract` also take dates.

use teseladb_bson::{Decimal, Decimal128, Document, Value};
use teseladb_mql::{DecOp, MqlError, dec_binary, decimal_math, decimal_to_f64, to_decimal128};

use super::{Eval, Expr, Scope, codes, count, error};

/// Which arithmetic operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arith {
    Add,
    Subtract,
    Multiply,
    Divide,
    Mod,
    Abs,
    Ceil,
    Floor,
    Sqrt,
    Exp,
    Ln,
    Log10,
    Log,
    Pow,
    Round,
    Trunc,
}

/// Every operator with its name.
const NAMES: [(&str, Arith); 16] = [
    ("$add", Arith::Add),
    ("$subtract", Arith::Subtract),
    ("$multiply", Arith::Multiply),
    ("$divide", Arith::Divide),
    ("$mod", Arith::Mod),
    ("$abs", Arith::Abs),
    ("$ceil", Arith::Ceil),
    ("$floor", Arith::Floor),
    ("$sqrt", Arith::Sqrt),
    ("$exp", Arith::Exp),
    ("$ln", Arith::Ln),
    ("$log10", Arith::Log10),
    ("$log", Arith::Log),
    ("$pow", Arith::Pow),
    ("$round", Arith::Round),
    ("$trunc", Arith::Trunc),
];

/// A number of one of RefDb's numeric types.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Num {
    Int(i32),
    Long(i64),
    Double(f64),
    Decimal(Decimal128),
}

impl Num {
    fn of(v: &Value) -> Option<Num> {
        match v {
            Value::Int32(n) => Some(Num::Int(*n)),
            Value::Int64(n) => Some(Num::Long(*n)),
            Value::Double(n) => Some(Num::Double(*n)),
            Value::Decimal128(d) => Some(Num::Decimal(*d)),
            _ => None,
        }
    }

    fn value(self) -> Value {
        match self {
            Num::Int(n) => Value::Int32(n),
            Num::Long(n) => Value::Int64(n),
            Num::Double(n) => Value::Double(n),
            Num::Decimal(d) => Value::Decimal128(d),
        }
    }

    fn f64(self) -> f64 {
        match self {
            Num::Int(n) => f64::from(n),
            Num::Long(n) => n as f64,
            Num::Double(n) => n,
            Num::Decimal(d) => decimal_to_f64(d),
        }
    }

    fn is_decimal(self) -> bool {
        matches!(self, Num::Decimal(_))
    }

    fn is_double(self) -> bool {
        matches!(self, Num::Double(_))
    }
}

/// `a op b` as decimals.
fn dec(op: DecOp, a: Num, b: Num) -> Decimal128 {
    dec_binary(op, &a.value(), &b.value()).unwrap_or_else(Decimal128::nan)
}

/// `a ⊕ b` with the promotion of `+`, `−` and `×` (`f` on `i64`, `g` on `f64`, `op` on decimals).
fn combine(
    a: Num,
    b: Num,
    f: fn(i64, i64) -> Option<i64>,
    g: fn(f64, f64) -> f64,
    op: DecOp,
) -> Num {
    match (a, b) {
        (x, y) if x.is_decimal() || y.is_decimal() => Num::Decimal(dec(op, x, y)),
        (x, y) if x.is_double() || y.is_double() => Num::Double(g(x.f64(), y.f64())),
        (Num::Int(x), Num::Int(y)) => match f(i64::from(x), i64::from(y)) {
            Some(r) => i32::try_from(r).map_or(Num::Long(r), Num::Int),
            None => Num::Double(g(f64::from(x), f64::from(y))),
        },
        (x, y) => {
            let (i, j) = (as_i64(x), as_i64(y));
            match f(i, j) {
                Some(r) => Num::Long(r),
                None => Num::Double(g(i as f64, j as f64)),
            }
        }
    }
}

fn as_i64(n: Num) -> i64 {
    match n {
        Num::Int(x) => i64::from(x),
        Num::Long(x) => x,
        Num::Double(x) => x as i64,
        Num::Decimal(d) => decimal_to_f64(d) as i64,
    }
}

/// RefDb's name of a raw value's type in a message.
pub(crate) fn raw_type_name(v: teseladb_bson::RawValue<'_>) -> &'static str {
    type_name(&Value::from(v))
}

/// RefDb's name of a value's type in a message.
pub(crate) fn type_name_of(v: &Value) -> &'static str {
    type_name(v)
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Double(_) => "double",
        Value::String(_) => "string",
        Value::Document(_) => "object",
        Value::Array(_) => "array",
        Value::Binary { .. } => "binData",
        Value::Undefined => "undefined",
        Value::ObjectId(_) => "objectId",
        Value::Boolean(_) => "bool",
        Value::DateTime(_) => "date",
        Value::Null => "null",
        Value::Regex { .. } => "regex",
        Value::Int32(_) => "int",
        Value::Int64(_) => "long",
        Value::Decimal128(_) => "decimal",
        Value::Timestamp { .. } => "timestamp",
        Value::MinKey => "minKey",
        Value::MaxKey => "maxKey",
        Value::Symbol(_) => "symbol",
        Value::DbPointer { .. } => "dbPointer",
        Value::JavaScriptWithScope { .. } => "javascriptWithScope",
        Value::JavaScript(_) => "javascript",
    }
}

/// A number as these messages write it: a decimal as it is, anything else as C streams write the
/// double (`-2.14748e+09`).
fn shown(n: Num) -> String {
    match n {
        Num::Decimal(d) => d.to_string(),
        other => teseladb_mql::stream_double(other.f64()),
    }
}

fn nullish(v: &Eval) -> bool {
    matches!(v, None | Some(Value::Null | Value::Undefined))
}

impl Arith {
    /// The operator named `name`.
    pub fn of(name: &str) -> Option<Arith> {
        NAMES.iter().find(|(n, _)| *n == name).map(|(_, a)| *a)
    }

    /// Checks the number of arguments.
    pub fn check(self, name: &str, args: &[Expr]) -> Result<(), MqlError> {
        match self {
            Arith::Add | Arith::Multiply => Ok(()),
            Arith::Subtract | Arith::Divide | Arith::Mod | Arith::Log | Arith::Pow => {
                count(name, args, 2..=2)
            }
            Arith::Round | Arith::Trunc => count(name, args, 1..=2),
            _ => count(name, args, 1..=1),
        }
    }

    /// Evaluates the operator on `args`.
    pub fn eval(self, args: &[Expr], doc: &Document, scope: &Scope) -> Result<Eval, MqlError> {
        let vals: Vec<Eval> = args.iter().map(|a| a.eval(doc, scope)).collect::<Result<_, _>>()?;
        match self {
            Arith::Add => add(&vals),
            Arith::Multiply => multiply(&vals),
            Arith::Subtract => subtract(&vals[0], &vals[1]),
            Arith::Divide => divide(&vals[0], &vals[1]),
            Arith::Mod => modulo(&vals[0], &vals[1]),
            Arith::Pow => pow(&vals[0], &vals[1]),
            Arith::Log => log(&vals[0], &vals[1]),
            Arith::Round | Arith::Trunc => round(self, &vals),
            unary => unary_op(unary, &vals[0]),
        }
    }
}

/// `$add` over `vals`.
pub(crate) fn add(vals: &[Eval]) -> Result<Eval, MqlError> {
    let mut sum = Num::Int(0);
    let mut date: Option<i64> = None;
    let mut null = false;
    for v in vals {
        match v {
            None | Some(Value::Null | Value::Undefined) => null = true,
            Some(Value::DateTime(t)) => {
                if date.is_some() {
                    return Err(error(
                        codes::ADD_TWO_DATES,
                        "only one date allowed in an $add expression",
                    ));
                }
                date = Some(*t);
            }
            Some(x) => match Num::of(x) {
                Some(n) => sum = combine(sum, n, i64::checked_add, |a, b| a + b, DecOp::Add),
                None => {
                    return Err(error(
                        codes::TYPE_MISMATCH,
                        format!("$add only supports numeric or date types, not {}", type_name(x)),
                    ));
                }
            },
        }
    }
    if null {
        return Ok(Some(Value::Null));
    }
    Ok(Some(match date {
        Some(t) => Value::DateTime(t.wrapping_add(millis_of(sum))),
        None => sum.value(),
    }))
}

/// A number of milliseconds added to a date: a double rounded half away from zero, a decimal
/// half to even (`2.5` is 2), as RefDb converts them.
fn millis_of(n: Num) -> i64 {
    match n {
        Num::Decimal(_) => round_half_even(n.f64()) as i64,
        other => other.f64().round() as i64,
    }
}

/// `x` rounded to the nearest integer, ties to even (`$round`).
fn round_half_even(x: f64) -> f64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 && r % 2.0 != 0.0 { r - x.signum() } else { r }
}

/// `$multiply` over `vals`.
pub(crate) fn multiply(vals: &[Eval]) -> Result<Eval, MqlError> {
    let mut product = Num::Int(1);
    let mut null = false;
    for v in vals {
        match v {
            None | Some(Value::Null | Value::Undefined) => null = true,
            Some(x) => match Num::of(x) {
                Some(n) => {
                    product = combine(product, n, i64::checked_mul, |a, b| a * b, DecOp::Mul)
                }
                None => {
                    return Err(error(
                        codes::TYPE_MISMATCH,
                        format!("$multiply only supports numeric types, not {}", type_name(x)),
                    ));
                }
            },
        }
    }
    Ok(Some(if null { Value::Null } else { product.value() }))
}

/// `$subtract` of `b` from `a`.
pub(crate) fn subtract(a: &Eval, b: &Eval) -> Result<Eval, MqlError> {
    let (Some(x), Some(y)) = (a, b) else { return Ok(Some(Value::Null)) };
    if nullish(a) || nullish(b) {
        return Ok(Some(Value::Null));
    }
    match (x, y) {
        (Value::DateTime(s), Value::DateTime(t)) => Ok(Some(Value::Int64(s.wrapping_sub(*t)))),
        (Value::DateTime(s), n) if Num::of(n).is_some() => {
            let ms = Num::of(n).map_or(0, millis_of);
            Ok(Some(Value::DateTime(s.wrapping_sub(ms))))
        }
        _ => match (Num::of(x), Num::of(y)) {
            (Some(m), Some(n)) => {
                Ok(Some(combine(m, n, i64::checked_sub, |a, b| a - b, DecOp::Sub).value()))
            }
            _ => Err(error(
                codes::TYPE_MISMATCH,
                format!(
                    "can't $subtract {} from {}",
                    type_name(y),
                    if matches!(x, Value::DateTime(_)) { "Date" } else { type_name(x) }
                ),
            )),
        },
    }
}

fn numbers(
    name: &str,
    code: teseladb_mql::ErrorCode,
    a: &Value,
    b: &Value,
) -> Result<(Num, Num), MqlError> {
    match (Num::of(a), Num::of(b)) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => Err(error(
            code,
            format!(
                "{name} only supports numeric types, not {} and {}",
                type_name(a),
                type_name(b)
            ),
        )),
    }
}

fn is_zero(n: Num) -> bool {
    match n {
        Num::Decimal(d) => decimal_to_f64(d) == 0.0,
        other => other.f64() == 0.0,
    }
}

/// `$divide` of `a` by `b`.
pub(crate) fn divide(a: &Eval, b: &Eval) -> Result<Eval, MqlError> {
    let (Some(x), Some(y)) = (a, b) else { return Ok(Some(Value::Null)) };
    if nullish(a) || nullish(b) {
        return Ok(Some(Value::Null));
    }
    let (m, n) = numbers("$divide", codes::TYPE_MISMATCH, x, y)?;
    if is_zero(n) {
        return Err(error(codes::BAD_DIVISION, "can't $divide by zero"));
    }
    Ok(Some(if m.is_decimal() || n.is_decimal() {
        Value::Decimal128(dec(DecOp::Div, m, n))
    } else {
        Value::Double(m.f64() / n.f64())
    }))
}

fn modulo(a: &Eval, b: &Eval) -> Result<Eval, MqlError> {
    let (Some(x), Some(y)) = (a, b) else { return Ok(Some(Value::Null)) };
    if nullish(a) || nullish(b) {
        return Ok(Some(Value::Null));
    }
    let (m, n) = numbers("$mod", codes::MOD_TYPES, x, y)?;
    if is_zero(n) {
        return Err(error(codes::MOD_BY_ZERO, "can't $mod by zero"));
    }
    Ok(Some(match (m, n) {
        (p, q) if p.is_decimal() || q.is_decimal() => Value::Decimal128(dec(DecOp::Rem, p, q)),
        (p, q) if p.is_double() || q.is_double() => Value::Double(p.f64() % q.f64()),
        (Num::Int(p), Num::Int(q)) => Value::Int32(p.wrapping_rem(q)),
        (p, q) => Value::Int64(as_i64(p).wrapping_rem(as_i64(q))),
    }))
}

fn pow(a: &Eval, b: &Eval) -> Result<Eval, MqlError> {
    let (Some(x), Some(y)) = (a, b) else { return Ok(Some(Value::Null)) };
    if nullish(a) || nullish(b) {
        return Ok(Some(Value::Null));
    }
    let (Some(base), Some(exp)) = (Num::of(x), Num::of(y)) else {
        let (code, which, v) = match Num::of(x) {
            None => (codes::POW_BASE, "base", x),
            Some(_) => (codes::POW_EXPONENT, "exponent", y),
        };
        return Err(error(code, format!("$pow's {which} must be numeric, not {}", type_name(v))));
    };
    if is_zero(base) && exp.f64() < 0.0 {
        return Err(error(
            codes::POW_ZERO_NEGATIVE,
            "$pow cannot take a base of 0 and a negative exponent",
        ));
    }
    if base.is_decimal() || exp.is_decimal() {
        return Ok(Some(Value::Decimal128(decimal_math::pow(as_decimal(base), as_decimal(exp)))));
    }
    if base.is_double() || exp.is_double() {
        return Ok(Some(Value::Double(base.f64().powf(exp.f64()))));
    }
    let (b, e) = (as_i64(base), as_i64(exp));
    if e < 0 {
        // A base of 1 or −1 keeps its integer type; any other base gives a double.
        let unit = match b {
            1 => Some(1),
            -1 => Some(if e % 2 == 0 { 1 } else { -1 }),
            _ => None,
        };
        return Ok(Some(match (unit, base) {
            (Some(u), Num::Int(_)) => Value::Int32(u as i32),
            (Some(u), _) => Value::Int64(u),
            (None, _) => Value::Double((b as f64).powf(e as f64)),
        }));
    }
    let exact = u32::try_from(e).ok().and_then(|e| b.checked_pow(e));
    Ok(Some(match (exact, base, exp) {
        (Some(r), Num::Int(_), Num::Int(_)) => {
            i32::try_from(r).map_or(Value::Int64(r), Value::Int32)
        }
        (Some(r), _, _) => Value::Int64(r),
        (None, _, _) => Value::Double((b as f64).powf(e as f64)),
    }))
}

fn log(a: &Eval, b: &Eval) -> Result<Eval, MqlError> {
    let (Some(x), Some(y)) = (a, b) else { return Ok(Some(Value::Null)) };
    if nullish(a) || nullish(b) {
        return Ok(Some(Value::Null));
    }
    let (Some(n), Some(base)) = (Num::of(x), Num::of(y)) else {
        let (code, which, v) = match Num::of(x) {
            None => (codes::LOG_ARGUMENT_NUMBER, "argument", x),
            Some(_) => (codes::LOG_BASE_NUMBER, "base", y),
        };
        return Err(error(code, format!("$log's {which} must be numeric, not {}", type_name(v))));
    };
    if n.f64() <= 0.0 {
        return Err(error(
            codes::LOG_NOT_POSITIVE,
            format!("$log's argument must be a positive number, but is {}", shown(n)),
        ));
    }
    if base.f64() <= 0.0 || base.f64() == 1.0 {
        return Err(error(
            codes::LOG_BASE,
            format!("$log's base must be a positive number not equal to 1, but is {}", shown(base)),
        ));
    }
    if n.is_decimal() || base.is_decimal() {
        let r = decimal_math::log(as_decimal(n), as_decimal(base));
        return Ok(Some(Value::Decimal128(r.unwrap_or_else(Decimal128::nan))));
    }
    Ok(Some(Value::Double(n.f64().ln() / base.f64().ln())))
}

fn unary_op(op: Arith, a: &Eval) -> Result<Eval, MqlError> {
    let Some(x) = a else { return Ok(Some(Value::Null)) };
    if nullish(a) {
        return Ok(Some(Value::Null));
    }
    let name = NAMES.iter().find(|(_, o)| *o == op).map_or("", |(n, _)| *n);
    let Some(n) = Num::of(x) else {
        return Err(error(
            codes::UNARY_NOT_NUMBER,
            format!("{name} only supports numeric types, not {}", type_name(x)),
        ));
    };
    let positive = |code, what: &str| -> Result<(), MqlError> {
        if n.f64() <= 0.0 {
            return Err(error(
                code,
                format!("{name}'s argument must be a positive number, but is {}{what}", shown(n)),
            ));
        }
        Ok(())
    };
    Ok(Some(match op {
        Arith::Abs => match n {
            Num::Int(i) => i.checked_abs().map_or(Value::Int64(-i64::from(i)), Value::Int32),
            Num::Long(l) => match l.checked_abs() {
                Some(a) => Value::Int64(a),
                None => return Err(error(codes::ABS_LONG_MIN, "can't take $abs of long long min")),
            },
            Num::Double(d) => Value::Double(d.abs()),
            Num::Decimal(d) => Value::Decimal128(dec_round(d, 0, Rounding::Abs)),
        },
        Arith::Ceil | Arith::Floor => match n {
            Num::Double(d) => Value::Double(if op == Arith::Ceil { d.ceil() } else { d.floor() }),
            Num::Decimal(d) => {
                let r = if op == Arith::Ceil { Rounding::Ceil } else { Rounding::Floor };
                Value::Decimal128(dec_round(d, 0, r))
            }
            other => other.value(),
        },
        Arith::Sqrt => {
            if n.f64() < 0.0 {
                return Err(error(
                    codes::SQRT_NEGATIVE,
                    "$sqrt's argument must be greater than or equal to 0",
                ));
            }
            match n {
                Num::Decimal(d) => {
                    Value::Decimal128(decimal_math::sqrt(d).unwrap_or_else(Decimal128::nan))
                }
                other => Value::Double(other.f64().sqrt()),
            }
        }
        Arith::Exp => match n {
            Num::Decimal(d) => Value::Decimal128(decimal_math::exp(d)),
            other => Value::Double(other.f64().exp()),
        },
        Arith::Ln => {
            positive(codes::LN_NOT_POSITIVE, "")?;
            match n {
                Num::Decimal(d) => {
                    Value::Decimal128(decimal_math::ln(d).unwrap_or_else(Decimal128::nan))
                }
                other => Value::Double(other.f64().ln()),
            }
        }
        _ => {
            positive(codes::LOG10_NOT_POSITIVE, "")?;
            match n {
                Num::Decimal(d) => {
                    Value::Decimal128(decimal_math::log10(d).unwrap_or_else(Decimal128::nan))
                }
                other => Value::Double(other.f64().log10()),
            }
        }
    }))
}

/// `$round` and `$trunc` to `places` decimal places (negative: to tens, hundreds…), keeping the
/// number's type.
fn round(op: Arith, vals: &[Eval]) -> Result<Eval, MqlError> {
    let Some(x) = &vals[0] else { return Ok(Some(Value::Null)) };
    if nullish(&vals[0]) || vals.get(1).is_some_and(nullish) {
        return Ok(Some(Value::Null));
    }
    let name = if op == Arith::Round { "$round" } else { "$trunc" };
    let Some(n) = Num::of(x) else {
        return Err(error(
            codes::ROUND_TYPES,
            format!("{name} only supports numeric types, not {}", type_name(x)),
        ));
    };
    let places = match vals.get(1).cloned().flatten() {
        None => 0,
        Some(p) => match Num::of(&p) {
            Some(q) if q.f64().fract() != 0.0 => {
                return Err(error(
                    codes::ROUND_PLACES_INTEGRAL,
                    format!("precision argument to  {name} must be a integral value"),
                ));
            }
            Some(q) if (-20.0..=100.0).contains(&q.f64()) => q.f64() as i32,
            Some(q) => {
                return Err(error(
                    codes::ROUND_PLACES,
                    format!(
                        "cannot apply {name} with precision value {} value must be in [-20, 100]",
                        q.f64() as i64
                    ),
                ));
            }
            None => {
                return Err(error(
                    codes::CANNOT_CONVERT_TO_LONG,
                    format!("can't convert from BSON type {} to long", type_name(&p)),
                ));
            }
        },
    };
    let scale = 10f64.powi(places);
    let apply = |d: f64| {
        let s = d * scale;
        let r = if op == Arith::Round { round_half_even(s) } else { s.trunc() };
        r / scale
    };
    Ok(Some(match n {
        Num::Double(d) => Value::Double(apply(d)),
        Num::Decimal(d) => {
            let r = if op == Arith::Round { Rounding::HalfEven } else { Rounding::Trunc };
            Value::Decimal128(dec_round(d, places, r))
        }
        Num::Int(i) if places < 0 => i32::try_from(apply(f64::from(i)) as i64)
            .map_or(Value::Int64(apply(f64::from(i)) as i64), Value::Int32),
        Num::Long(l) if places < 0 => Value::Int64(apply(l as f64) as i64),
        other => other.value(),
    }))
}

/// A number as a decimal (a double with 15 significant digits, as RefDb converts it).
fn as_decimal(n: Num) -> Decimal128 {
    to_decimal128(&n.value()).unwrap_or_else(Decimal128::nan)
}

/// How [`dec_round`] treats the dropped digits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rounding {
    HalfEven,
    Trunc,
    Ceil,
    Floor,
    /// No rounding: the absolute value.
    Abs,
}

/// Decimal `d` rounded to `places` decimal places, exactly.
fn dec_round(d: Decimal128, places: i32, mode: Rounding) -> Decimal128 {
    let Decimal::Finite { negative, coefficient, exponent } = d.classify() else { return d };
    if mode == Rounding::Abs {
        return Decimal128::from_parts(false, coefficient, exponent).unwrap_or(d);
    }
    let drop = -places - exponent;
    if drop <= 0 {
        return d;
    }
    let (kept, rest, unit) = match 10u128.checked_pow(drop as u32) {
        Some(unit) => (coefficient / unit, coefficient % unit, unit),
        None => (0, coefficient, u128::MAX),
    };
    let up = match mode {
        // A power of ten is even: its half is exact.
        Rounding::HalfEven => {
            let half = unit / 2;
            rest > half || (rest == half && kept % 2 == 1)
        }
        Rounding::Trunc | Rounding::Abs => false,
        Rounding::Ceil => rest > 0 && !negative,
        Rounding::Floor => rest > 0 && negative,
    };
    let kept = kept + u128::from(up);
    Decimal128::from_parts(negative, kept, -places).unwrap_or(d)
}
