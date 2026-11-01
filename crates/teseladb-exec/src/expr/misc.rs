//! Bitwise, trigonometric, size, timestamp, hashing, vector and context expressions (measured on
//! a `refrouter` 9.0.4): `$bitAnd`, `$bitOr`, `$bitXor`, `$bitNot`, `$sin` … `$tanh`, `$atan2`,
//! `$degreesToRadians`, `$radiansToDegrees`, `$sigmoid`, `$binarySize`, `$bsonSize`,
//! `$tsSecond`, `$tsIncrement`, `$subtype`, `$hash`, `$hexHash`, `$toHashedIndexKey`,
//! `$similarityDotProduct`, `$similarityCosine`, `$similarityEuclidean`, `$rand`,
//! `$currentDate`, `$createObjectId` and `$meta`.
//!
//! A double or integer argument gives a double, a decimal one a decimal (computed by
//! `teseladb_mql::decimal_trig`); `null` or a missing argument gives `null`.

use teseladb_bson::{Decimal128, Document, RawValue, Value};
use teseladb_mql::decimal_trig::{Trig, atan2 as decimal_atan2, trig};
use teseladb_mql::{ErrorCode, MqlError, stream_double};

use super::arith::type_name_of;
use super::values::{display, f64_of};
use super::{Ctx, Eval, Expr, Scope, arguments, codes, count, error};

macro_rules! code {
    ($n:literal) => {
        ErrorCode { code: $n, name: concat!("Location", $n) }
    };
}

const TRIG_DOMAIN: ErrorCode = code!(50989);
const ATAN2_FIRST: ErrorCode = code!(51044);
const ATAN2_SECOND: ErrorCode = code!(51045);
const BINARY_SIZE: ErrorCode = code!(51276);
const BSON_SIZE: ErrorCode = code!(31393);
const TS_SECOND: ErrorCode = code!(5687301);
const TS_INCREMENT: ErrorCode = code!(5687302);
const SUBTYPE: ErrorCode = code!(10389300);
const HASH_INPUT: ErrorCode = code!(10754000);
const HASH_ALGORITHM_STRING: ErrorCode = code!(10754001);
const HASH_ALGORITHM: ErrorCode = code!(10754002);
const NO_ARGUMENTS: ErrorCode = code!(10065);
const RAND_ARGUMENTS: ErrorCode = code!(3040501);
const CURRENT_DATE_ARGUMENTS: ErrorCode = code!(9428201);
const VECTOR_TYPE: ErrorCode = code!(10506600);
const META_STRING: ErrorCode = code!(17307);
const META_UNSUPPORTED: ErrorCode = code!(17308);
const META_STREAM: ErrorCode = code!(9692105);
const META_MISSING: ErrorCode = code!(40218);
const SIM_TYPE: ErrorCode = code!(10413200);
const SIM_SIZE: ErrorCode = code!(10413202);
const SIM_FIRST: ErrorCode = code!(10413203);
const SIM_SECOND: ErrorCode = code!(10413204);
const SIM_VECTORS: ErrorCode = code!(10413205);
const SIM_SCORE: ErrorCode = code!(10413206);
const SIM_UNKNOWN: ErrorCode = code!(10413207);
const SIM_ARGUMENT: ErrorCode = code!(10413208);
const SIM_TWO: ErrorCode = code!(10413209);

/// The milliseconds of a second.
const MS_PER_SECOND: i64 = 1000;

/// The functions of one number, with their names, domains as RefDb writes them and whether
/// the domain's bounds are ±1 (`[-1,1]`), from 1 (`[1,inf]`) or every finite number.
const UNARY: [(&str, Trig, &str); 15] = [
    ("$sin", Trig::Sin, "(-inf,inf)"),
    ("$cos", Trig::Cos, "(-inf,inf)"),
    ("$tan", Trig::Tan, "(-inf,inf)"),
    ("$asin", Trig::Asin, "[-1,1]"),
    ("$acos", Trig::Acos, "[-1,1]"),
    ("$atan", Trig::Atan, ""),
    ("$sinh", Trig::Sinh, ""),
    ("$cosh", Trig::Cosh, ""),
    ("$tanh", Trig::Tanh, ""),
    ("$asinh", Trig::Asinh, ""),
    ("$acosh", Trig::Acosh, "[1,inf]"),
    ("$atanh", Trig::Atanh, "[-1,1]"),
    ("$degreesToRadians", Trig::DegreesToRadians, ""),
    ("$radiansToDegrees", Trig::RadiansToDegrees, ""),
    ("$sigmoid", Trig::Sigmoid, ""),
];

/// A bitwise operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bits {
    And,
    Or,
    Xor,
}

/// The algorithms of `$hash`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Algorithm {
    Md5,
    Sha256,
    Xxh64,
}

/// A similarity of two vectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Similarity {
    Dot,
    Cosine,
    Euclidean,
}

/// One of these expressions.
#[derive(Clone, Debug, PartialEq)]
pub enum Misc {
    Bits(Bits, Vec<Expr>),
    BitNot(Expr),
    Unary(Trig, Expr),
    Atan2(Expr, Expr),
    BinarySize(Expr),
    BsonSize(Expr),
    TsSecond(Expr),
    TsIncrement(Expr),
    Subtype(Expr),
    Hash {
        hex: bool,
        input: Expr,
        algorithm: Expr,
    },
    HashedKey(Expr),
    Similarity {
        kind: Similarity,
        vectors: Vec<Expr>,
        score: bool,
    },
    Rand,
    CurrentDate,
    CreateObjectId,
    Meta(String),
    /// `$function`, run by the scope's JavaScript engine.
    Function(Box<super::js::Function>),
}

fn name_of(t: Trig) -> &'static str {
    UNARY.iter().find(|(_, x, _)| *x == t).map_or("", |(n, _, _)| n)
}

fn domain_of(t: Trig) -> &'static str {
    UNARY.iter().find(|(_, x, _)| *x == t).map_or("", |(_, _, d)| d)
}

fn one_arg(name: &str, arg: RawValue<'_>, ctx: &Ctx) -> Result<Expr, MqlError> {
    let args: Vec<Expr> =
        arguments(arg).into_iter().map(|x| Expr::parse_in(x, ctx)).collect::<Result<_, _>>()?;
    count(name, &args, 1..=1)?;
    Ok(args.into_iter().next().unwrap_or(Expr::Literal(Value::Null)))
}

/// The argument of `$rand` and `$currentDate`: `{}` or `[]`; a document or array with
/// something in it is refused with `busy`, anything else with 10065.
fn no_arguments(name: &str, arg: RawValue<'_>, busy: ErrorCode) -> Result<(), MqlError> {
    let empty = match arg {
        RawValue::Document(d) => d.iter().next().is_none(),
        RawValue::Array(a) => a.iter().next().is_none(),
        _ => {
            return Err(error(
                NO_ARGUMENTS,
                format!("invalid parameter: expected an object ({name})"),
            ));
        }
    };
    if empty {
        Ok(())
    } else {
        Err(error(busy, format!("{name} does not currently accept arguments")))
    }
}

/// Parses one of these operators; `None` for any other.
pub(super) fn parse(name: &str, arg: RawValue<'_>, ctx: &Ctx) -> Result<Option<Misc>, MqlError> {
    let list = || {
        arguments(arg).into_iter().map(|x| Expr::parse_in(x, ctx)).collect::<Result<Vec<_>, _>>()
    };
    Ok(Some(match name {
        "$bitAnd" => Misc::Bits(Bits::And, list()?),
        "$bitOr" => Misc::Bits(Bits::Or, list()?),
        "$bitXor" => Misc::Bits(Bits::Xor, list()?),
        "$bitNot" => Misc::BitNot(one_arg(name, arg, ctx)?),
        "$sigmoid" => parse_sigmoid(arg, ctx)?,
        "$atan2" => {
            let args = list()?;
            count(name, &args, 2..=2)?;
            let mut it = args.into_iter();
            let (y, x) = (it.next(), it.next());
            Misc::Atan2(
                y.unwrap_or(Expr::Literal(Value::Null)),
                x.unwrap_or(Expr::Literal(Value::Null)),
            )
        }
        "$binarySize" => Misc::BinarySize(one_arg(name, arg, ctx)?),
        "$bsonSize" => Misc::BsonSize(one_arg(name, arg, ctx)?),
        "$tsSecond" => Misc::TsSecond(one_arg(name, arg, ctx)?),
        "$tsIncrement" => Misc::TsIncrement(one_arg(name, arg, ctx)?),
        "$subtype" => Misc::Subtype(one_arg(name, arg, ctx)?),
        "$hash" | "$hexHash" => parse_hash(name, arg, ctx)?,
        "$toHashedIndexKey" => Misc::HashedKey(Expr::parse_in(arg, ctx)?),
        "$similarityDotProduct" => parse_similarity(name, Similarity::Dot, arg, ctx)?,
        "$similarityCosine" => parse_similarity(name, Similarity::Cosine, arg, ctx)?,
        "$similarityEuclidean" => parse_similarity(name, Similarity::Euclidean, arg, ctx)?,
        "$rand" => {
            no_arguments(name, arg, RAND_ARGUMENTS)?;
            Misc::Rand
        }
        "$currentDate" => {
            no_arguments(name, arg, CURRENT_DATE_ARGUMENTS)?;
            Misc::CurrentDate
        }
        "$createObjectId" => match arg {
            RawValue::Document(d) if d.iter().next().is_none() => Misc::CreateObjectId,
            _ => {
                return Err(error(
                    codes::FAILED_TO_PARSE,
                    "$createObjectId only accepts the empty object as argument. To convert a value to an ObjectId, use $toObjectId.",
                ));
            }
        },
        "$meta" => parse_meta(arg)?,
        "$function" => Misc::Function(Box::new(super::js::Function::parse(arg, ctx)?)),
        other => match UNARY.iter().find(|(n, _, _)| *n == other) {
            Some((_, t, _)) => Misc::Unary(*t, one_arg(name, arg, ctx)?),
            None => return Ok(None),
        },
    }))
}

fn parse_sigmoid(arg: RawValue<'_>, ctx: &Ctx) -> Result<Misc, MqlError> {
    // A literal that is no number is refused when parsed; anything else is evaluated.
    // A literal string or an array is refused when parsed; anything else is evaluated.
    let refused = match arg {
        RawValue::String(s) if !s.starts_with('$') => Some("string"),
        RawValue::Array(_) => Some("array"),
        _ => None,
    };
    if let Some(t) = refused {
        return Err(error(
            codes::TYPE_MISMATCH,
            format!("$sigmoid only supports numeric types, not {t}"),
        ));
    }
    Ok(Misc::Unary(Trig::Sigmoid, Expr::parse_in(arg, ctx)?))
}

fn parse_hash(name: &str, arg: RawValue<'_>, ctx: &Ctx) -> Result<Misc, MqlError> {
    let RawValue::Document(d) = arg else {
        return Err(error(
            codes::FAILED_TO_PARSE,
            format!(
                "{name} expects an object of named arguments but found: {}",
                super::arith::raw_type_name(arg)
            ),
        ));
    };
    let (mut input, mut algorithm) = (None, None);
    for (k, v) in d.iter() {
        match k {
            "input" => input = Some(Expr::parse_in(v, ctx)?),
            "algorithm" => algorithm = Some(Expr::parse_in(v, ctx)?),
            other => {
                return Err(error(
                    codes::FAILED_TO_PARSE,
                    format!("{name} found an unknown argument: {other}"),
                ));
            }
        }
    }
    let input = input.ok_or_else(|| {
        error(codes::FAILED_TO_PARSE, format!("Missing 'input' parameter to {name}"))
    })?;
    let algorithm = algorithm.ok_or_else(|| {
        error(codes::FAILED_TO_PARSE, format!("Missing 'algorithm' parameter to {name}"))
    })?;
    Ok(Misc::Hash { hex: name == "$hexHash", input, algorithm })
}

fn parse_similarity(
    name: &str,
    kind: Similarity,
    arg: RawValue<'_>,
    ctx: &Ctx,
) -> Result<Misc, MqlError> {
    let (vectors, score) = match arg {
        RawValue::Array(a) => {
            (a.iter().map(|x| Expr::parse_in(x, ctx)).collect::<Result<Vec<_>, _>>()?, false)
        }
        RawValue::Document(d) => {
            let (mut vectors, mut score) = (Vec::new(), false);
            for (k, v) in d.iter() {
                match (k, v) {
                    ("vectors", RawValue::Array(a)) => {
                        vectors =
                            a.iter().map(|x| Expr::parse_in(x, ctx)).collect::<Result<_, _>>()?;
                    }
                    ("vectors", other) => {
                        return Err(error(
                            SIM_VECTORS,
                            format!(
                                "vectors must be an array of expressions, but found {}",
                                super::arith::raw_type_name(other)
                            ),
                        ));
                    }
                    ("score", RawValue::Boolean(b)) => score = b,
                    ("score", _) => {
                        return Err(error(
                            SIM_SCORE,
                            "score must be a boolean, but found object".to_owned(),
                        ));
                    }
                    (other, _) => {
                        return Err(error(
                            SIM_UNKNOWN,
                            format!("{name} found an unknown argument: {other}"),
                        ));
                    }
                }
            }
            (vectors, score)
        }
        other => {
            return Err(error(
                SIM_ARGUMENT,
                format!(
                    "{name} only supports an object or array as an argument, but found{}",
                    super::arith::raw_type_name(other)
                ),
            ));
        }
    };
    if vectors.len() != 2 {
        return Err(error(SIM_TWO, format!("{name} requires exactly two vectors")));
    }
    Ok(Misc::Similarity { kind, vectors, score })
}

/// The `$meta` names that give nothing when the document has no such metadata, and those that
/// are an error then, with the words of the error.
const META_OPTIONAL: [&str; 10] = [
    "randVal",
    "searchHighlights",
    "indexKey",
    "searchScoreDetails",
    "searchSequenceToken",
    "timeseriesBucketMinTime",
    "timeseriesBucketMaxTime",
    "recordId",
    "sortKey",
    "changeStreamControlEvent",
];
const META_REQUIRED: [(&str, &str); 8] = [
    ("textScore", "text score"),
    ("searchScore", "$search score"),
    ("geoNearDistance", "$geoNear distance"),
    ("geoNearPoint", "$geoNear point"),
    ("vectorSearchScore", "$vectorSearch score"),
    ("score", "score"),
    ("scoreDetails", "scoreDetails"),
    ("searchRootDocumentId", "$search root document id"),
];

/// The error of metadata `name` that no stage before gives: `None` for one that is optional.
pub(crate) fn missing_meta_error(name: &str) -> Option<MqlError> {
    META_REQUIRED.iter().find(|(n, _)| *n == name).map(|(_, words)| {
        error(META_MISSING, format!("query requires {words} metadata, but it is not available"))
    })
}

fn parse_meta(arg: RawValue<'_>) -> Result<Misc, MqlError> {
    let RawValue::String(name) = arg else {
        return Err(error(META_STRING, "$meta only supports string arguments"));
    };
    if name == "stream" {
        return Err(error(META_STREAM, "Unsupported $meta field: stream"));
    }
    let known = META_OPTIONAL.contains(&name) || META_REQUIRED.iter().any(|(n, _)| *n == name);
    if !known {
        return Err(error(META_UNSUPPORTED, format!("Unsupported $meta field: {name}")));
    }
    Ok(Misc::Meta(name.to_owned()))
}

fn nullish(v: &Eval) -> bool {
    matches!(v, None | Some(Value::Null | Value::Undefined))
}

impl Misc {
    /// Evaluates the expression.
    pub fn eval(&self, doc: &Document, scope: &Scope) -> Result<Eval, MqlError> {
        let at = |e: &Expr| e.eval(doc, scope);
        match self {
            Misc::Bits(op, args) => {
                let vals: Vec<Eval> = args.iter().map(at).collect::<Result<_, _>>()?;
                bits(*op, &vals)
            }
            Misc::BitNot(e) => match at(e)? {
                ref v if nullish(v) => Ok(Some(Value::Null)),
                Some(Value::Int32(n)) => Ok(Some(Value::Int32(!n))),
                Some(Value::Int64(n)) => Ok(Some(Value::Int64(!n))),
                Some(other) => Err(error(
                    codes::TYPE_MISMATCH,
                    format!("$bitNot only supports int and long, not: {}.", type_name_of(&other)),
                )),
                None => Ok(Some(Value::Null)),
            },
            Misc::Unary(t, e) => unary(*t, &at(e)?),
            Misc::Atan2(y, x) => atan2(&at(y)?, &at(x)?),
            Misc::BinarySize(e) => match at(e)? {
                ref v if nullish(v) => Ok(Some(Value::Null)),
                Some(Value::String(s)) => Ok(Some(int(s.len()))),
                Some(Value::Binary { bytes, .. }) => Ok(Some(int(bytes.len()))),
                Some(other) => Err(error(
                    BINARY_SIZE,
                    format!(
                        "$binarySize requires a string or BinData argument, found: {}",
                        type_name_of(&other)
                    ),
                )),
                None => Ok(Some(Value::Null)),
            },
            Misc::BsonSize(e) => match at(e)? {
                ref v if nullish(v) => Ok(Some(Value::Null)),
                Some(Value::Document(d)) => Ok(Some(int(crate::raw::bytes_of(&d)?.len()))),
                Some(other) => Err(error(
                    BSON_SIZE,
                    format!("$bsonSize requires a document input, found: {}", type_name_of(&other)),
                )),
                None => Ok(Some(Value::Null)),
            },
            Misc::TsSecond(e) | Misc::TsIncrement(e) => {
                let second = matches!(self, Misc::TsSecond(_));
                match at(e)? {
                    ref v if nullish(v) => Ok(Some(Value::Null)),
                    Some(Value::Timestamp { time, increment }) => {
                        Ok(Some(Value::Int64(i64::from(if second { time } else { increment }))))
                    }
                    Some(other) => {
                        let (code, name) = if second {
                            (TS_SECOND, "$tsSecond")
                        } else {
                            (TS_INCREMENT, "$tsIncrement")
                        };
                        Err(error(
                            code,
                            format!(
                                " Argument to {name} must be a timestamp, but is {}",
                                type_name_of(&other)
                            ),
                        ))
                    }
                    None => Ok(Some(Value::Null)),
                }
            }
            Misc::Subtype(e) => match at(e)? {
                ref v if nullish(v) => Ok(Some(Value::Null)),
                Some(Value::Binary { subtype, .. }) => Ok(Some(Value::Int32(i32::from(subtype)))),
                Some(other) => Err(error(
                    SUBTYPE,
                    format!(
                        "Provided value: {}, does not have a subtype, but $subtype was used.",
                        display(&other)
                    ),
                )),
                None => Ok(Some(Value::Null)),
            },
            Misc::Hash { hex, input, algorithm } => hash(*hex, &at(input)?, &at(algorithm)?),
            Misc::HashedKey(e) => {
                let v = at(e)?.unwrap_or(Value::Null);
                let holder = Document::new().with("v", v);
                let key = crate::raw::with_raw_doc(&holder, |raw| {
                    Ok(raw.get("v").map_or(0, teseladb_index::keygen::hashed_key))
                })?;
                Ok(Some(Value::Int64(key)))
            }
            Misc::Similarity { kind, vectors, score } => {
                similarity(*kind, &at(&vectors[0])?, &at(&vectors[1])?, *score)
            }
            Misc::Rand => Ok(Some(Value::Double(scope.random_unit()))),
            Misc::Function(f) => f.eval(doc, scope),
            Misc::CurrentDate => super::path::eval_variable("NOW", None, doc, scope),
            Misc::CreateObjectId => {
                let now = match super::path::eval_variable("NOW", None, doc, scope)? {
                    Some(Value::DateTime(ms)) => ms,
                    _ => 0,
                };
                let secs = u32::try_from(now.div_euclid(MS_PER_SECOND)).unwrap_or(0);
                let mut id = [0u8; 12];
                id[..4].copy_from_slice(&secs.to_be_bytes());
                id[4..].copy_from_slice(&scope.random_u64().to_be_bytes());
                Ok(Some(Value::ObjectId(id)))
            }
            Misc::Meta(name) => match scope.meta.get(name) {
                Some(v) => Ok(Some(v.clone())),
                None => match META_REQUIRED.iter().find(|(n, _)| n == name) {
                    Some((_, words)) => Err(error(
                        META_MISSING,
                        format!("query requires {words} metadata, but it is not available"),
                    )),
                    None => Ok(None),
                },
            },
        }
    }
}

fn int(n: usize) -> Value {
    Value::Int32(i32::try_from(n).unwrap_or(i32::MAX))
}

fn bits(op: Bits, vals: &[Eval]) -> Result<Eval, MqlError> {
    let name = match op {
        Bits::And => "$bitAnd",
        Bits::Or => "$bitOr",
        Bits::Xor => "$bitXor",
    };
    let mut acc: i64 = if op == Bits::And { -1 } else { 0 };
    let mut long = false;
    // RefDb looks at the operands from the last one.
    for v in vals.iter().rev() {
        let n = match v {
            Some(Value::Int32(n)) => i64::from(*n),
            Some(Value::Int64(n)) => {
                long = true;
                *n
            }
            v if nullish(v) => return Ok(Some(Value::Null)),
            _ => {
                return Err(error(
                    codes::TYPE_MISMATCH,
                    format!("{name} only supports int and long operands."),
                ));
            }
        };
        acc = match op {
            Bits::And => acc & n,
            Bits::Or => acc | n,
            Bits::Xor => acc ^ n,
        };
    }
    Ok(Some(if long { Value::Int64(acc) } else { Value::Int32(acc as i32) }))
}

/// The value of a number as these messages write it.
fn number_text(v: &Value) -> String {
    match v {
        Value::Decimal128(d) => d.to_string(),
        other => stream_double(f64_of(other).unwrap_or(f64::NAN)),
    }
}

fn unary(t: Trig, v: &Eval) -> Result<Eval, MqlError> {
    let name = name_of(t);
    let Some(v) = v.as_ref().filter(|v| !matches!(v, Value::Null | Value::Undefined)) else {
        return Ok(Some(Value::Null));
    };
    let out_of_domain = || {
        error(
            TRIG_DOMAIN,
            format!("cannot apply {name} to {}, value must be in {}", number_text(v), domain_of(t)),
        )
    };
    if let Value::Decimal128(d) = v {
        return trig(t, *d).map(|r| Some(Value::Decimal128(r))).ok_or_else(out_of_domain);
    }
    let Some(x) = f64_of(v) else {
        let (code, verb) = if t == Trig::Sigmoid {
            (codes::TYPE_MISMATCH, "$multiply")
        } else {
            (codes::UNARY_NOT_NUMBER, name)
        };
        return Err(error(
            code,
            format!("{verb} only supports numeric types, not {}", type_name_of(v)),
        ));
    };
    let finite_only = || if x.is_infinite() { Err(out_of_domain()) } else { Ok(()) };
    // The functions of a finite double go through the decimal engine and back, so that the
    // double is the one nearest the exact value, as RefDb's results are (macOS's libm is off
    // by one unit for some, `asin(0.5)` among them).
    let exact = |f: fn(f64) -> f64| -> f64 {
        let plain = matches!(t, Trig::DegreesToRadians | Trig::RadiansToDegrees | Trig::Sigmoid);
        if !x.is_finite() || x == 0.0 || plain {
            return f(x);
        }
        let d = teseladb_mql::decimal_math::from_f64_exact(x);
        match trig(t, d) {
            Some(r) => teseladb_mql::decimal_to_f64(r),
            None => f(x),
        }
    };
    let unit = || if x.abs() > 1.0 { Err(out_of_domain()) } else { Ok(()) };
    let r = match t {
        Trig::Sin => finite_only().map(|()| exact(f64::sin))?,
        Trig::Cos => finite_only().map(|()| exact(f64::cos))?,
        Trig::Tan => finite_only().map(|()| exact(f64::tan))?,
        Trig::Asin => unit().map(|()| exact(f64::asin))?,
        Trig::Acos => unit().map(|()| exact(f64::acos))?,
        Trig::Atan => exact(f64::atan),
        Trig::Sinh => exact(f64::sinh),
        Trig::Cosh => exact(f64::cosh),
        Trig::Tanh => exact(f64::tanh),
        Trig::Asinh => exact(f64::asinh),
        Trig::Acosh if x < 1.0 => return Err(out_of_domain()),
        Trig::Acosh => exact(f64::acosh),
        Trig::Atanh => unit().map(|()| exact(f64::atanh))?,
        Trig::DegreesToRadians => x * std::f64::consts::PI / 180.0,
        Trig::RadiansToDegrees => x * 180.0 / std::f64::consts::PI,
        Trig::Sigmoid => 1.0 / (1.0 + (-x).exp()),
    };
    Ok(Some(Value::Double(r)))
}

fn atan2(y: &Eval, x: &Eval) -> Result<Eval, MqlError> {
    let present = |v: &Eval| v.clone().filter(|v| !matches!(v, Value::Null | Value::Undefined));
    let (Some(y), Some(x)) = (present(y), present(x)) else { return Ok(Some(Value::Null)) };
    let (y, x) = (&y, &x);
    for (v, code) in [(y, ATAN2_FIRST), (x, ATAN2_SECOND)] {
        if f64_of(v).is_none() {
            return Err(error(
                code,
                format!("$atan2 only supports numeric types, not {}", type_name_of(v)),
            ));
        }
    }
    if matches!(y, Value::Decimal128(_)) || matches!(x, Value::Decimal128(_)) {
        let dec = |v: &Value| teseladb_mql::to_decimal128(v).unwrap_or_else(Decimal128::nan);
        return Ok(decimal_atan2(dec(y), dec(x)).map(Value::Decimal128));
    }
    let (yf, xf) = (f64_of(y).unwrap_or(f64::NAN), f64_of(x).unwrap_or(f64::NAN));
    if !yf.is_finite() || !xf.is_finite() || yf == 0.0 || xf == 0.0 {
        return Ok(Some(Value::Double(yf.atan2(xf))));
    }
    let exact = |v: f64| teseladb_mql::decimal_math::from_f64_exact(v);
    let r = decimal_atan2(exact(yf), exact(xf)).map_or(yf.atan2(xf), teseladb_mql::decimal_to_f64);
    Ok(Some(Value::Double(r)))
}

fn hash(hex: bool, input: &Eval, algorithm: &Eval) -> Result<Eval, MqlError> {
    let algorithm = match algorithm {
        Some(Value::String(s)) => match s.as_str() {
            "md5" => Algorithm::Md5,
            "sha256" => Algorithm::Sha256,
            "xxh64" => Algorithm::Xxh64,
            other => {
                return Err(error(
                    HASH_ALGORITHM,
                    format!(
                        "Currently, the only supported algorithms for $hash are 'md5', 'sha256' and 'xxh64', found: {other}"
                    ),
                ));
            }
        },
        other => {
            return Err(error(
                HASH_ALGORITHM_STRING,
                format!(
                    "$hash requires that 'algorithm' be a string, found: {} with value {}",
                    super::array::eval_type(other),
                    super::values::display_eval(other)
                ),
            ));
        }
    };
    let bytes: Vec<u8> = match input {
        v if nullish(v) => return Ok(Some(Value::Null)),
        Some(Value::String(s)) => s.as_bytes().to_vec(),
        Some(Value::Binary { bytes, .. }) => bytes.clone(),
        Some(other) => {
            return Err(error(
                HASH_INPUT,
                format!(
                    "$hash requires that 'input' be a valid UTF-8 string or binData, found: {} with value {}",
                    type_name_of(other),
                    display(other)
                ),
            ));
        }
        None => return Ok(Some(Value::Null)),
    };
    let digest: Vec<u8> = match algorithm {
        Algorithm::Md5 => teseladb_hash::md5(&bytes).to_vec(),
        Algorithm::Sha256 => teseladb_hash::sha256(&bytes).to_vec(),
        Algorithm::Xxh64 => teseladb_compress::zstd::xxh64(&bytes).to_be_bytes().to_vec(),
    };
    Ok(Some(if hex {
        Value::String(super::convert_bin::hex(&digest))
    } else {
        Value::Binary { subtype: 0, bytes: digest }
    }))
}

/// The binary subtype of a vector.
const VECTOR_SUBTYPE: u8 = 9;
const INT8: u8 = 0x03;
const FLOAT32: u8 = 0x27;
const PACKED_BIT: u8 = 0x10;

/// The numbers of a vector argument, or RefDb's refusal.
fn vector(v: &Value, which: &str, name: &str) -> Result<Vec<f64>, MqlError> {
    match v {
        Value::Array(items) => items
            .iter()
            .map(|x| match x {
                Value::Boolean(b) => Ok(f64::from(u8::from(*b))),
                other => f64_of(other).ok_or_else(|| {
                    let code = if which == "first" { SIM_FIRST } else { SIM_SECOND };
                    error(code, format!("All elements in the {which} argument to {name} must be numeric or boolean"))
                }),
            })
            .collect(),
        Value::Binary { subtype: VECTOR_SUBTYPE, bytes } if bytes.len() >= 2 => {
            let (dtype, padding, data) = (bytes[0], bytes[1], &bytes[2..]);
            Ok(match dtype {
                INT8 => data.iter().map(|b| f64::from(*b as i8)).collect(),
                FLOAT32 => {
                    data.as_chunks::<4>().0.iter().map(|c| f64::from(f32::from_le_bytes(*c))).collect()
                }
                PACKED_BIT => {
                    let all: Vec<f64> = data
                        .iter()
                        .flat_map(|b| (0..8).rev().map(move |i| f64::from((b >> i) & 1)))
                        .collect();
                    let keep = all.len().saturating_sub(usize::from(padding));
                    all.into_iter().take(keep).collect()
                }
                _ => {
                    return Err(error(
                        VECTOR_TYPE,
                        "Invalid dType for provided BinData vector. Valid options are 0x10 for PACKED_BIT, 0x03 for INT8, or 0x27 for FLOAT32.",
                    ));
                }
            })
        }
        other => Err(error(
            SIM_TYPE,
            format!(
                "Arguments to {name} must be arrays or binData vectors, but found type: {}",
                type_name_of(other)
            ),
        )),
    }
}

fn similarity(kind: Similarity, a: &Eval, b: &Eval, score: bool) -> Result<Eval, MqlError> {
    let name = match kind {
        Similarity::Dot => "$similarityDotProduct",
        Similarity::Cosine => "$similarityCosine",
        Similarity::Euclidean => "$similarityEuclidean",
    };
    let (Some(a), Some(b)) = (
        a.as_ref().filter(|v| !matches!(v, Value::Null)),
        b.as_ref().filter(|v| !matches!(v, Value::Null)),
    ) else {
        return Ok(Some(Value::Null));
    };
    let (x, y) = (vector(a, "first", name)?, vector(b, "second", name)?);
    if x.len() != y.len() {
        return Err(error(
            SIM_SIZE,
            format!(
                "Arguments to {name} must be the same size, but the first is of size {} and the second is of size {}",
                x.len(),
                y.len()
            ),
        ));
    }
    let dot = x.iter().zip(&y).fold(0.0, |acc, (p, q)| acc + p * q);
    let r = match kind {
        Similarity::Dot => {
            if score {
                (1.0 + dot) / 2.0
            } else {
                dot
            }
        }
        Similarity::Cosine => {
            let norm = x.iter().map(|p| p * p).sum::<f64>().sqrt()
                * y.iter().map(|q| q * q).sum::<f64>().sqrt();
            let cos = if norm == 0.0 { 0.0 } else { dot / norm };
            if score { (1.0 + cos) / 2.0 } else { cos }
        }
        Similarity::Euclidean => {
            let d = x.iter().zip(&y).map(|(p, q)| (p - q) * (p - q)).sum::<f64>().sqrt();
            if score { 1.0 / (1.0 + d) } else { d }
        }
    };
    Ok(Some(Value::Double(r)))
}
