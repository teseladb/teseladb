//! Type expressions (measured on a `refrouter` 9.0.4): `$type`, `$isNumber`, `$convert` and its
//! shorthands `$toBool`, `$toInt`, `$toLong`, `$toDouble`, `$toDecimal`, `$toString`,
//! `$toObjectId`, `$toDate`, `$toUUID`, `$toArray` and `$toObject`.
//!
//! A conversion that fails is RefDb's 241 (`ConversionFailure`), which `onError` replaces; the
//! other errors (a wrong `format`, `base` or `subtype`) are not replaced. A `null` or missing
//! input gives `onNull`, or `null`.

use teseladb_bson::{Decimal, Decimal128, Document, RawValue, Value};
use teseladb_mql::{ErrorCode, MqlError, decimal_to_f64, stream_double, to_decimal128};

use super::arith::type_name_of;
use super::convert_bin::{self as bin, Format, UUID_SUBTYPE};
use super::string::{date_text, double_text};
use super::values::{f64_of, int64_of};
use super::{Ctx, Eval, Expr, Scope, arguments, codes, count, error};

macro_rules! code {
    ($n:literal) => {
        ErrorCode { code: $n, name: concat!("Location", $n) }
    };
}

/// A conversion that failed.
const CONVERSION_FAILURE: ErrorCode = ErrorCode { code: 241, name: "ConversionFailure" };
const SINGLE_ARGUMENT: ErrorCode = code!(50723);
const FORMAT_REQUIRED: ErrorCode = code!(4341115);
const FORMAT_INVALID: ErrorCode = code!(4341125);
const SUBTYPE_INVALID: ErrorCode = code!(4341107);
const SUBTYPE_NUMBER: ErrorCode = code!(4341108);
const BASE_INTEGER: ErrorCode = code!(3501300);
const BASE_INVALID: ErrorCode = code!(3501301);
const BYTE_ORDER: ErrorCode = code!(9130002);
const BYTE_ORDER_STRING: ErrorCode = code!(9130001);
const MD5_LENGTH: ErrorCode = code!(13016802);
const COLUMN_SUBTYPE: ErrorCode = code!(12910300);
const DATE_RANGE: ErrorCode = code!(18537);

/// The binary subtype of an MD5 digest, which needs 16 bytes.
const MD5_SUBTYPE: u8 = 5;
/// The binary subtype of a column, which `$convert` refuses.
const COLUMN: u8 = 7;
/// The milliseconds of a second.
const MS_PER_SECOND: i64 = 1000;

/// BSON types by their number and the name `$convert` and `$type` know them by.
const TYPES: [(i32, &str); 22] = [
    (1, "double"),
    (2, "string"),
    (3, "object"),
    (4, "array"),
    (5, "binData"),
    (6, "undefined"),
    (7, "objectId"),
    (8, "bool"),
    (9, "date"),
    (10, "null"),
    (11, "regex"),
    (12, "dbPointer"),
    (13, "javascript"),
    (14, "symbol"),
    (15, "javascriptWithScope"),
    (16, "int"),
    (17, "timestamp"),
    (18, "long"),
    (19, "decimal"),
    (-1, "minKey"),
    (127, "maxKey"),
    (0, "missing"),
];

const DOUBLE: i32 = 1;
const STRING: i32 = 2;
const OBJECT: i32 = 3;
const ARRAY: i32 = 4;
const BIN_DATA: i32 = 5;
const OBJECT_ID: i32 = 7;
const BOOL: i32 = 8;
const DATE: i32 = 9;
const INT: i32 = 16;
const LONG: i32 = 18;
const DECIMAL: i32 = 19;

fn type_name(code: i32) -> &'static str {
    TYPES.iter().find(|(c, _)| *c == code).map_or("missing", |(_, n)| n)
}

/// A conversion of `$convert`.
#[derive(Clone, Debug, PartialEq)]
pub struct Convert {
    input: Expr,
    to: To,
    on_error: Option<Expr>,
    on_null: Option<Expr>,
    format: Option<Expr>,
    byte_order: Option<Expr>,
    base: Option<Expr>,
}

/// Where `$convert` goes: a fixed type (with a binary subtype) or an expression.
#[derive(Clone, Debug, PartialEq)]
enum To {
    Fixed(i32, Option<u8>),
    Computed(Expr),
}

/// A type expression.
#[derive(Clone, Debug, PartialEq)]
pub enum TypeExpr {
    /// `$type`.
    Type(Expr),
    /// `$isNumber`.
    IsNumber(Expr),
    /// `$convert` or a shorthand.
    Convert(Box<Convert>),
}

/// The shorthands and the type each converts to (`$toUUID` is binary data of subtype 4, read
/// from text as a UUID).
const SHORTHANDS: [(&str, i32); 11] = [
    ("$toBool", BOOL),
    ("$toInt", INT),
    ("$toLong", LONG),
    ("$toDouble", DOUBLE),
    ("$toDecimal", DECIMAL),
    ("$toString", STRING),
    ("$toObjectId", OBJECT_ID),
    ("$toDate", DATE),
    ("$toUUID", BIN_DATA),
    ("$toArray", ARRAY),
    ("$toObject", OBJECT),
];

/// Parses a type expression; `None` for any other operator.
pub(super) fn parse(
    name: &str,
    arg: RawValue<'_>,
    ctx: &Ctx,
) -> Result<Option<TypeExpr>, MqlError> {
    let one = |args: Vec<Expr>| -> Result<Expr, MqlError> {
        count(name, &args, 1..=1)?;
        Ok(args.into_iter().next().unwrap_or(Expr::Literal(Value::Null)))
    };
    let args = || {
        arguments(arg).into_iter().map(|x| Expr::parse_in(x, ctx)).collect::<Result<Vec<_>, _>>()
    };
    if name == "$type" {
        return Ok(Some(TypeExpr::Type(one(args()?)?)));
    }
    if name == "$isNumber" {
        return Ok(Some(TypeExpr::IsNumber(one(args()?)?)));
    }
    if name == "$convert" {
        return Ok(Some(TypeExpr::Convert(Box::new(parse_convert(arg, ctx)?))));
    }
    let Some((_, to)) = SHORTHANDS.iter().find(|(n, _)| *n == name) else { return Ok(None) };
    let list = args()?;
    if list.len() != 1 {
        return Err(error(
            SINGLE_ARGUMENT,
            format!("{name} requires a single argument, got {}", list.len()),
        ));
    }
    let uuid = name == "$toUUID";
    let subtype = uuid.then_some(UUID_SUBTYPE);
    let input = list.into_iter().next().unwrap_or(Expr::Literal(Value::Null));
    let format = match name {
        "$toUUID" => Some(Expr::Literal(Value::String("uuid".to_owned()))),
        "$toString" => Some(Expr::Literal(Value::String("auto".to_owned()))),
        _ => None,
    };
    Ok(Some(TypeExpr::Convert(Box::new(Convert {
        input,
        to: To::Fixed(*to, subtype),
        on_error: None,
        on_null: None,
        format,
        byte_order: None,
        base: None,
    }))))
}

fn parse_convert(arg: RawValue<'_>, ctx: &Ctx) -> Result<Convert, MqlError> {
    let RawValue::Document(d) = arg else {
        return Err(error(
            codes::FAILED_TO_PARSE,
            format!(
                "$convert expects an object of named arguments but found: {}",
                super::arith::raw_type_name(arg)
            ),
        ));
    };
    let mut c = Convert {
        input: Expr::Literal(Value::Null),
        to: To::Computed(Expr::Literal(Value::Null)),
        on_error: None,
        on_null: None,
        format: None,
        byte_order: None,
        base: None,
    };
    let (mut input, mut to) = (None, None);
    for (k, v) in d.iter() {
        let e = Some(Expr::parse_in(v, ctx)?);
        match k {
            "input" => input = e,
            "to" => to = e,
            "onError" => c.on_error = e,
            "onNull" => c.on_null = e,
            "format" => c.format = e,
            "byteOrder" => c.byte_order = e,
            "base" => c.base = e,
            other => {
                return Err(error(
                    codes::FAILED_TO_PARSE,
                    format!("$convert found an unknown argument: {other}"),
                ));
            }
        }
    }
    c.input = input
        .ok_or_else(|| error(codes::FAILED_TO_PARSE, "Missing 'input' parameter to $convert"))?;
    let to =
        to.ok_or_else(|| error(codes::FAILED_TO_PARSE, "Missing 'to' parameter to $convert"))?;
    c.to = To::Computed(to);
    Ok(c)
}

/// The type code of a `to` value, `None` for `null` or missing.
fn type_code(v: &Eval) -> Result<Option<i32>, MqlError> {
    match v {
        None | Some(Value::Null | Value::Undefined) => Ok(None),
        Some(Value::String(s)) => TYPES
            .iter()
            .find(|(_, n)| n == s)
            .map(|(c, _)| Some(*c))
            .ok_or_else(|| error(codes::BAD_VALUE, format!("Unknown type name: {s}"))),
        Some(x) if f64_of(x).is_some() => {
            let n = int64_of(x).ok_or_else(|| {
                error(
                    codes::FAILED_TO_PARSE,
                    "In $convert, numeric 'to' argument is not an integer",
                )
            })?;
            TYPES.iter().find(|(c, _)| i64::from(*c) == n).map(|(c, _)| Some(*c)).ok_or_else(|| {
                error(
                    codes::FAILED_TO_PARSE,
                    format!("In $convert, numeric value for 'to' does not correspond to a BSON type: {n}"),
                )
            })
        }
        Some(x) => Err(error(
            codes::FAILED_TO_PARSE,
            format!(
                "$convert's 'to' argument must be a string or number, but is {}",
                type_name_of(x)
            ),
        )),
    }
}

/// The binary subtype `subtype` of a `to` document.
fn subtype_of(v: Option<&Value>) -> Result<u8, MqlError> {
    let Some(v) = v else { return Ok(0) };
    if f64_of(v).is_none() {
        return Err(error(
            SUBTYPE_NUMBER,
            format!(
                "For BinData, $convert's 'subtype' argument must be a number, but is {}",
                type_name_of(v)
            ),
        ));
    }
    let n = int64_of(v).unwrap_or(-1);
    match u8::try_from(n) {
        Ok(s) if s <= 9 || s >= 128 => Ok(s),
        _ => Err(error(
            SUBTYPE_INVALID,
            format!(
                "In $convert, numeric value for 'subtype' does not correspond to a BinData type: {n}"
            ),
        )),
    }
}

/// A conversion failure (241), with RefDb's text: `what` and the detail after it.
fn failure(what: &str, detail: Option<&str>) -> MqlError {
    let tail = detail.map(|d| format!(": {d}")).unwrap_or_default();
    error(CONVERSION_FAILURE, format!("{what} in $convert with no onError value{tail}"))
}

fn unsupported(from: &Value, to: i32) -> MqlError {
    failure(
        &format!("Unsupported conversion from {} to {}", type_name_of(from), type_name(to)),
        None,
    )
}

fn overflow(detail: &str) -> MqlError {
    failure("Conversion would overflow target type", Some(detail))
}

/// The options of one conversion.
struct Options {
    format: Option<Format>,
    big: bool,
    base: Option<u32>,
    subtype: u8,
}

impl TypeExpr {
    /// Evaluates the expression.
    pub fn eval(&self, doc: &Document, scope: &Scope) -> Result<Eval, MqlError> {
        match self {
            TypeExpr::Type(e) => {
                let v = e.eval(doc, scope)?;
                Ok(Some(Value::String(super::array::eval_type(&v).to_owned())))
            }
            TypeExpr::IsNumber(e) => {
                let v = e.eval(doc, scope)?;
                Ok(Some(Value::Boolean(v.as_ref().is_some_and(super::values::is_number))))
            }
            TypeExpr::Convert(c) => c.eval(doc, scope),
        }
    }
}

impl Convert {
    fn eval(&self, doc: &Document, scope: &Scope) -> Result<Eval, MqlError> {
        let (to, subtype) = match &self.to {
            To::Fixed(t, s) => (*t, s.unwrap_or(0)),
            To::Computed(e) => {
                let v = e.eval(doc, scope)?;
                let (code, subtype) = match &v {
                    Some(Value::Document(d)) => {
                        let t = d.get("type").cloned();
                        let code = match type_code(&t)? {
                            Some(c) => c,
                            None => {
                                return Err(error(
                                    codes::FAILED_TO_PARSE,
                                    format!(
                                        "$convert's 'to' argument must be a string or number, but is {}",
                                        super::array::eval_type(&t)
                                    ),
                                ));
                            }
                        };
                        let sub = if code == BIN_DATA { subtype_of(d.get("subtype"))? } else { 0 };
                        (Some(code), sub)
                    }
                    other => (type_code(other)?, 0),
                };
                match code {
                    Some(c) => (c, subtype),
                    None => return Ok(Some(Value::Null)),
                }
            }
        };
        let input = self.input.eval(doc, scope)?;
        if matches!(input, None | Some(Value::Null | Value::Undefined)) {
            return match &self.on_null {
                Some(e) => e.eval(doc, scope),
                None => Ok(Some(Value::Null)),
            };
        }
        let opts = self.options(doc, scope, subtype)?;
        let input = input.unwrap_or(Value::Null);
        match convert(&input, to, &opts) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.code.code == CONVERSION_FAILURE.code => match &self.on_error {
                Some(on) => on.eval(doc, scope),
                None => Err(e),
            },
            Err(e) => Err(e),
        }
    }

    fn options(&self, doc: &Document, scope: &Scope, subtype: u8) -> Result<Options, MqlError> {
        let eval = |e: &Option<Expr>| -> Result<Eval, MqlError> {
            match e {
                Some(x) => x.eval(doc, scope),
                None => Ok(None),
            }
        };
        let format = match eval(&self.format)? {
            Some(Value::String(s)) => Some(Format::of(&s).ok_or_else(|| {
                error(FORMAT_INVALID, format!("Invalid 'format' argument for $convert: {s}"))
            })?),
            _ => None,
        };
        let big = match eval(&self.byte_order)? {
            Some(Value::String(s)) if s == "big" => true,
            Some(Value::String(s)) if s == "little" => false,
            None | Some(Value::Null) => false,
            Some(Value::String(other)) => {
                return Err(error(
                    BYTE_ORDER,
                    format!("Invalid 'byteOrder' argument for $convert: {other}"),
                ));
            }
            Some(other) => {
                return Err(error(
                    BYTE_ORDER_STRING,
                    format!(
                        "$convert requires that 'byteOrder' be a string, found: {} with value {}",
                        type_name_of(&other),
                        super::values::display(&other)
                    ),
                ));
            }
        };
        let base = match eval(&self.base)? {
            None | Some(Value::Null | Value::Undefined) => None,
            Some(v) => {
                let n = int64_of(&v).filter(|_| f64_of(&v).is_some()).ok_or_else(|| {
                    error(BASE_INTEGER, "In $convert, 'base' argument is not an integer")
                })?;
                match n {
                    2 | 8 | 10 | 16 => Some(n as u32),
                    _ => {
                        return Err(error(
                            BASE_INVALID,
                            "In $convert, 'base' argument is not a valid base",
                        ));
                    }
                }
            }
        };
        Ok(Options { format, big, base, subtype })
    }
}

/// `v` converted to type `to`.
fn convert(v: &Value, to: i32, o: &Options) -> Result<Value, MqlError> {
    match to {
        BOOL => Ok(Value::Boolean(
            super::truthy(&Some(v.clone())) || matches!(v, Value::Double(x) if x.is_nan()),
        )),
        INT => to_int(v, o, INT)
            .and_then(|n| i32::try_from(n.0).map(Value::Int32).map_err(|_| overflow(&n.1))),
        LONG => to_int(v, o, LONG).map(|n| Value::Int64(n.0)),
        DOUBLE => to_double(v, o).map(Value::Double),
        DECIMAL => to_decimal(v, o).map(Value::Decimal128),
        STRING => to_string(v, o).map(Value::String),
        OBJECT_ID => to_object_id(v),
        DATE => to_date(v),
        BIN_DATA => to_bin_data(v, o),
        ARRAY | OBJECT => match v {
            Value::String(s) => {
                let parsed = super::json::parse(s).ok_or_else(|| {
                    error(CONVERSION_FAILURE, "Input doesn't represent valid JSON")
                })?;
                let fits = matches!(
                    (&parsed, to),
                    (Value::Array(_), ARRAY) | (Value::Document(_), OBJECT)
                );
                if !fits {
                    return Err(error(
                        CONVERSION_FAILURE,
                        format!("Input doesn't match expected type '{}'", type_name(to)),
                    ));
                }
                Ok(parsed)
            }
            other => Err(unsupported(other, to)),
        },
        other => Err(unsupported(v, other)),
    }
}

/// A number as an integer toward zero, with the text an overflow to `int` shows.
fn to_int(v: &Value, o: &Options, target: i32) -> Result<(i64, String), MqlError> {
    let nan = || failure("Attempt to convert NaN value to integer type", None);
    let inf = || failure("Attempt to convert infinity value to integer type", None);
    match v {
        Value::Int32(n) => Ok((i64::from(*n), String::new())),
        Value::Int64(n) => Ok((*n, String::new())),
        Value::Boolean(b) => Ok((i64::from(*b), String::new())),
        Value::Double(x) => {
            if x.is_nan() {
                return Err(nan());
            }
            if x.is_infinite() {
                return Err(inf());
            }
            // A double is checked against the target's range before it is truncated.
            let shown = stream_double(*x);
            let fits = if target == INT {
                (f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(x)
            } else {
                (-9.223_372_036_854_776e18..9.223_372_036_854_776e18).contains(x)
            };
            if !fits {
                return Err(overflow(&shown));
            }
            Ok((x.trunc() as i64, shown))
        }
        Value::Decimal128(d) => match d.classify() {
            Decimal::NaN => Err(nan()),
            Decimal::Infinity { .. } => Err(inf()),
            Decimal::Finite { .. } => {
                let shown = d.to_string();
                let t = truncate_decimal(&shown);
                t.parse::<i64>().map(|n| (n, shown.clone())).map_err(|_| overflow(&shown))
            }
        },
        Value::DateTime(ms) if target != INT => Ok((*ms, String::new())),
        Value::String(s) => {
            let n = parse_integer(s, o.base.unwrap_or(10))?;
            if o.base.is_none() && target == INT && i32::try_from(n).is_err() {
                return Err(failure(&format!("Failed to parse number '{s}'"), Some("Overflow")));
            }
            Ok((n, String::new()))
        }
        Value::Binary { bytes, .. }
            if matches!(bytes.len(), 1 | 2 | 4) || bytes.len() == 8 && target != INT =>
        {
            let mut le = bin::number_bytes(bytes.clone(), o.big);
            let negative = le.last().is_some_and(|b| b & 0x80 != 0);
            le.resize(8, if negative { 0xFF } else { 0 });
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&le);
            Ok((i64::from_le_bytes(buf), String::new()))
        }
        Value::Binary { subtype, bytes } => Err(error(
            CONVERSION_FAILURE,
            format!(
                "Failed to convert '{}' to number in $convert because of invalid length: {}",
                bin::shown(*subtype, bytes),
                bytes.len()
            ),
        )),
        other => Err(unsupported(other, target)),
    }
}

/// The integral part of decimal text (`-12.7` → `-12`, `1E+3` → `1000`).
fn truncate_decimal(text: &str) -> String {
    let Ok(d) = text.parse::<Decimal128>() else { return String::new() };
    match d.classify() {
        Decimal::Finite { negative, coefficient, exponent } => {
            let digits = coefficient.to_string();
            let int = if exponent >= 0 {
                if exponent > 40 {
                    return text.to_owned();
                }
                format!("{digits}{}", "0".repeat(exponent as usize))
            } else {
                let cut = digits.len() as i64 + i64::from(exponent);
                if cut <= 0 { "0".to_owned() } else { digits[..cut as usize].to_owned() }
            };
            if negative && int != "0" { format!("-{int}") } else { int }
        }
        _ => String::new(),
    }
}

/// Integer text in `base`, as a `long`.
fn parse_integer(s: &str, base: u32) -> Result<i64, MqlError> {
    let parse_fail = |why: &str| failure(&format!("Failed to parse number '{s}'"), Some(why));
    let (neg, body) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    if body.starts_with("0x") || body.starts_with("0X") {
        return Err(failure("Illegal hexadecimal input", Some(s)));
    }
    if body.is_empty() {
        return Err(parse_fail("No digits"));
    }
    if !body.chars().all(|c| c.is_digit(base)) {
        return Err(parse_fail("Did not consume whole string."));
    }
    let magnitude =
        i128::try_from(u128::from_str_radix(body, base).unwrap_or(u128::MAX)).unwrap_or(i128::MAX);
    let value = if neg { -magnitude } else { magnitude };
    i64::try_from(value).map_err(|_| parse_fail("Overflow"))
}

fn to_double(v: &Value, o: &Options) -> Result<f64, MqlError> {
    match v {
        Value::Double(x) => Ok(*x),
        Value::Int32(n) => Ok(f64::from(*n)),
        Value::Int64(n) => Ok(*n as f64),
        Value::Boolean(b) => Ok(f64::from(u8::from(*b))),
        Value::DateTime(ms) => Ok(*ms as f64),
        Value::Decimal128(d) => {
            let x = decimal_to_f64(*d);
            let zero = matches!(d.classify(), Decimal::Finite { coefficient: 0, .. });
            if x.is_infinite() && !matches!(d.classify(), Decimal::Infinity { .. })
                || x == 0.0 && !zero
            {
                return Err(overflow(&d.to_string()));
            }
            Ok(x)
        }
        Value::String(s) if o.base.is_some() => {
            parse_integer(s, o.base.unwrap_or(10)).map(|n| n as f64)
        }
        Value::String(s) => parse_double(s),
        Value::Binary { bytes, .. } if bytes.len() == 4 => {
            let le = bin::number_bytes(bytes.clone(), o.big);
            Ok(f64::from(f32::from_le_bytes([le[0], le[1], le[2], le[3]])))
        }
        Value::Binary { bytes, .. } if bytes.len() == 8 => {
            let le = bin::number_bytes(bytes.clone(), o.big);
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&le);
            Ok(f64::from_le_bytes(buf))
        }
        Value::Binary { subtype, bytes } => Err(error(
            CONVERSION_FAILURE,
            format!(
                "Failed to convert '{}' to number in $convert because of invalid length: {}",
                bin::shown(*subtype, bytes),
                bytes.len()
            ),
        )),
        other => Err(unsupported(other, DOUBLE)),
    }
}

fn parse_double(s: &str) -> Result<f64, MqlError> {
    let fail = |why: &str| failure(&format!("Failed to parse number '{s}'"), Some(why));
    if s.is_empty() {
        return Err(fail("Empty string"));
    }
    if s.starts_with(char::is_whitespace) {
        return Err(fail("Leading whitespace"));
    }
    let body = s.trim_start_matches(['+', '-']);
    if body.starts_with("0x") || body.starts_with("0X") {
        return Err(failure("Illegal hexadecimal input", Some(s)));
    }
    let lower = body.to_ascii_lowercase();
    let neg = s.starts_with('-');
    if lower == "inf" || lower == "infinity" {
        return Ok(if neg { f64::NEG_INFINITY } else { f64::INFINITY });
    }
    if lower == "nan" {
        return Ok(f64::NAN);
    }
    let valid =
        s.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E'));
    let x: f64 = match s.parse() {
        Ok(x) if valid => x,
        // No number at the start at all is "Invalid number"; one followed by more text is not.
        _ if !body.starts_with(|c: char| c.is_ascii_digit() || c == '.') => {
            return Err(fail("Invalid number"));
        }
        _ => return Err(fail("Did not consume whole string")),
    };
    let underflow = x == 0.0 && body.bytes().any(|b| matches!(b, b'1'..=b'9')) && {
        let mantissa = body.split(['e', 'E']).next().unwrap_or("");
        mantissa.bytes().any(|b| matches!(b, b'1'..=b'9'))
    };
    if x.is_infinite() || underflow {
        return Err(fail("Out of range"));
    }
    Ok(x)
}

fn to_decimal(v: &Value, o: &Options) -> Result<Decimal128, MqlError> {
    let long = |n: i64| {
        Decimal128::from_parts(n < 0, u128::from(n.unsigned_abs()), 0)
            .unwrap_or_else(Decimal128::nan)
    };
    match v {
        Value::Decimal128(d) => Ok(*d),
        Value::Double(_) => Ok(to_decimal128(v).unwrap_or_else(Decimal128::nan)),
        Value::Int32(n) => Ok(long(i64::from(*n))),
        Value::Int64(n) => Ok(long(*n)),
        Value::Boolean(b) => Ok(long(i64::from(*b))),
        Value::DateTime(ms) => Ok(long(*ms)),
        Value::String(s) if o.base.is_some() => parse_integer(s, o.base.unwrap_or(10)).map(long),
        Value::String(s) => parse_decimal(s),
        other => Err(unsupported(other, DECIMAL)),
    }
}

fn parse_decimal(s: &str) -> Result<Decimal128, MqlError> {
    let fail = |why: &str| failure(&format!("Failed to parse number '{s}'"), Some(why));
    let body = s.trim_start_matches(['+', '-']);
    if body.starts_with("0x") || body.starts_with("0X") {
        return Err(failure("Illegal hexadecimal input", Some(s)));
    }
    let lower = body.to_ascii_lowercase();
    let neg = s.starts_with('-');
    if lower == "inf" || lower == "infinity" {
        return Ok(Decimal128::infinity(neg));
    }
    if lower == "nan" {
        return Ok(Decimal128::nan());
    }
    let shape = !body.is_empty()
        && body
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
        && body.bytes().next().is_some_and(|b| b.is_ascii_digit() || b == b'.');
    if !shape {
        return Err(fail("Failed to parse string to decimal"));
    }
    let (mantissa, exp_text) = match body.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (body, None),
    };
    let exp: i64 = match exp_text {
        Some(e) => e.parse().map_err(|_| fail("Failed to parse string to decimal"))?,
        None => 0,
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if int.is_empty() && frac.is_empty()
        || !(int.bytes().chain(frac.bytes())).all(|b| b.is_ascii_digit())
    {
        return Err(fail("Failed to parse string to decimal"));
    }
    match decimal_of_digits(neg, &format!("{int}{frac}"), exp - frac.len() as i64) {
        Ok(d) => Ok(d),
        Err(DecimalRange::Over) => Err(fail("Conversion from string to decimal would overflow")),
        Err(DecimalRange::Under) => Err(fail("Conversion from string to decimal would underflow")),
    }
}

/// Why digits do not fit a decimal.
enum DecimalRange {
    Over,
    Under,
}

/// The decimal `±digits × 10^exp`, cut to 34 digits toward zero as RefDb's `$convert` reads
/// decimal text (`…12349` gives `…1234`).
fn decimal_of_digits(neg: bool, digits: &str, exp: i64) -> Result<Decimal128, DecimalRange> {
    const MAX_DIGITS: usize = 34;
    const MIN_EXP: i64 = -6176;
    const MAX_EXP: i64 = 6111;
    let trimmed = digits.trim_start_matches('0');
    let mut digits: Vec<u8> = trimmed.bytes().map(|b| b - b'0').collect();
    let mut exp = exp;
    if digits.is_empty() {
        let e = exp.clamp(MIN_EXP, MAX_EXP) as i32;
        return Decimal128::from_parts(neg, 0, e).ok_or(DecimalRange::Over);
    }
    if digits.len() > MAX_DIGITS {
        let dropped = digits.split_off(MAX_DIGITS);
        exp += dropped.len() as i64;
    }
    // A large exponent takes trailing zeros into the coefficient while there is room.
    while exp > MAX_EXP && digits.len() < MAX_DIGITS {
        digits.push(0);
        exp -= 1;
    }
    if exp > MAX_EXP {
        return Err(DecimalRange::Over);
    }
    if exp < MIN_EXP {
        return Err(DecimalRange::Under);
    }
    let coefficient = digits.iter().fold(0u128, |acc, d| acc * 10 + u128::from(*d));
    Decimal128::from_parts(neg, coefficient, exp as i32).ok_or(DecimalRange::Over)
}

/// A date as `$toString` writes it, or RefDb's 18537 outside years 0 to 9999.
pub(super) fn date_string(ms: i64) -> Result<String, MqlError> {
    let text = date_text(ms);
    let year: i64 = text.get(..text.len() - 20).and_then(|y| y.parse().ok()).unwrap_or(0);
    if !(0..=9999).contains(&year) {
        return Err(error(
            DATE_RANGE,
            format!(
                "Could not convert date to string: date component was outside the supported range of 0-9999: {year}"
            ),
        ));
    }
    Ok(text)
}

/// The text of a scalar value as `$toString` gives it (`None` for arrays, documents and `null`).
pub(super) fn scalar_text(v: &Value) -> Option<String> {
    Some(match v {
        Value::String(s) | Value::Symbol(s) | Value::JavaScript(s) => s.clone(),
        Value::Double(x) => double_text(*x),
        Value::Int32(n) => n.to_string(),
        Value::Int64(n) => n.to_string(),
        Value::Decimal128(d) => d.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::DateTime(ms) => date_string(*ms).ok()?,
        Value::ObjectId(id) => id.iter().map(|b| format!("{b:02x}")).collect(),
        Value::Regex { pattern, options } => format!("/{pattern}/{options}"),
        Value::Timestamp { time, increment } => format!("Timestamp({time}, {increment})"),
        Value::MinKey => "MinKey".to_owned(),
        Value::MaxKey => "MaxKey".to_owned(),
        Value::Binary { subtype, bytes } if *subtype == UUID_SUBTYPE => {
            bin::uuid_text(bytes).unwrap_or_else(|| bin::base64(bytes, false))
        }
        Value::Binary { bytes, .. } => bin::base64(bytes, false),
        _ => return None,
    })
}

fn radix(n: i128, base: u32) -> String {
    let mut digits = Vec::new();
    let mut m = n.unsigned_abs();
    loop {
        digits.push(
            char::from_digit((m % u128::from(base)) as u32, base)
                .unwrap_or('0')
                .to_ascii_uppercase(),
        );
        m /= u128::from(base);
        if m == 0 {
            break;
        }
    }
    let body: String = digits.into_iter().rev().collect();
    if n < 0 { format!("-{body}") } else { body }
}

fn to_string(v: &Value, o: &Options) -> Result<String, MqlError> {
    if let (Some(base), Some(x)) = (o.base, f64_of(v)) {
        let integral = match v {
            Value::Int32(n) => Some(i128::from(*n)),
            Value::Int64(n) => Some(i128::from(*n)),
            _ if x.fract() == 0.0 && x.is_finite() => Some(x as i128),
            _ => None,
        };
        return integral
            .map(|n| radix(n, base))
            .ok_or_else(|| failure("Base conversion is not supported for non-integers", Some("")));
    }
    match v {
        Value::DateTime(ms) => date_string(*ms),
        Value::Array(_) | Value::Document(_) => Ok(super::json::write(v)),
        Value::Binary { subtype, bytes } => binary_text(*subtype, bytes, o),
        other => scalar_text(other).ok_or_else(|| unsupported(other, STRING)),
    }
}

fn binary_text(subtype: u8, bytes: &[u8], o: &Options) -> Result<String, MqlError> {
    let fail = |why: &str| {
        failure(&format!("Failed to convert '{}' to string", bin::shown(subtype, bytes)), Some(why))
    };
    match o.format {
        None => Err(error(
            FORMAT_REQUIRED,
            "Format must be speficied when converting from 'binData' to 'string'",
        )),
        Some(Format::Auto) if subtype == UUID_SUBTYPE => {
            Ok(bin::uuid_text(bytes).unwrap_or_else(|| bin::base64(bytes, false)))
        }
        Some(Format::Base64 | Format::Auto) => Ok(bin::base64(bytes, false)),
        Some(Format::Base64Url) => Ok(bin::base64(bytes, true)),
        Some(Format::Hex) => Ok(bin::hex(bytes)),
        Some(Format::Utf8) => String::from_utf8(bytes.to_vec())
            .map_err(|_| fail("BinData does not represent a valid UTF-8 string")),
        Some(Format::Uuid) => {
            bin::uuid_text(bytes).ok_or_else(|| fail("BinData does not represent a valid UUID"))
        }
    }
}

fn to_object_id(v: &Value) -> Result<Value, MqlError> {
    match v {
        Value::ObjectId(_) => Ok(v.clone()),
        Value::String(s) => {
            let fail =
                |why: String| failure(&format!("Failed to parse objectId '{s}'"), Some(&why));
            if s.len() != 24 {
                return Err(fail(format!(
                    "Invalid string length for parsing to OID, expected 24 but found {}",
                    s.len()
                )));
            }
            if let Some(c) = s.chars().find(|c| !c.is_ascii_hexdigit()) {
                return Err(fail(format!("Invalid character found in hex string: {c}")));
            }
            let bytes = bin::from_hex(s).map_err(fail)?;
            let mut id = [0u8; 12];
            id.copy_from_slice(&bytes);
            Ok(Value::ObjectId(id))
        }
        other => Err(unsupported(other, OBJECT_ID)),
    }
}

fn to_date(v: &Value) -> Result<Value, MqlError> {
    match v {
        Value::DateTime(_) => Ok(v.clone()),
        Value::Double(_) | Value::Int64(_) | Value::Decimal128(_) => {
            let o = Options { format: None, big: false, base: None, subtype: 0 };
            to_int(v, &o, DATE).map(|n| Value::DateTime(n.0))
        }
        Value::ObjectId(id) => {
            let secs = u32::from_be_bytes([id[0], id[1], id[2], id[3]]);
            Ok(Value::DateTime(i64::from(secs) * MS_PER_SECOND))
        }
        Value::Timestamp { time, .. } => Ok(Value::DateTime(i64::from(*time) * MS_PER_SECOND)),
        Value::String(s) => super::date_parse::parse_for_convert(s),
        other => Err(unsupported(other, DATE)),
    }
}

fn to_bin_data(v: &Value, o: &Options) -> Result<Value, MqlError> {
    let sub = o.subtype;
    let check_sub = |len: usize| -> Result<(), MqlError> {
        if sub == COLUMN {
            return Err(error(
                COLUMN_SUBTYPE,
                "$convert to BinData subtype Column (7) is not allowed",
            ));
        }
        if sub == MD5_SUBTYPE && len != 16 {
            return Err(error(
                MD5_LENGTH,
                "$convert to BinData subtype 5 requires exactly 16 bytes",
            ));
        }
        Ok(())
    };
    let bytes = match v {
        Value::Binary { subtype, bytes } => {
            if *subtype != sub {
                return Err(error(
                    CONVERSION_FAILURE,
                    "Conversions between different BinData subtypes are not supported",
                ));
            }
            return Ok(Value::Binary { subtype: sub, bytes: bytes.clone() });
        }
        Value::Int32(n) => bin::number_bytes(n.to_le_bytes().to_vec(), o.big),
        Value::Int64(n) => bin::number_bytes(n.to_le_bytes().to_vec(), o.big),
        Value::Double(x) => bin::number_bytes(x.to_le_bytes().to_vec(), o.big),
        Value::String(s) => string_bytes(s, o)?,
        other => return Err(unsupported(other, BIN_DATA)),
    };
    check_sub(bytes.len())?;
    Ok(Value::Binary { subtype: sub, bytes })
}

fn string_bytes(s: &str, o: &Options) -> Result<Vec<u8>, MqlError> {
    let fail = |why: &str| failure(&format!("Failed to parse BinData '{s}'"), Some(why));
    let format = o.format.ok_or_else(|| {
        error(
            FORMAT_REQUIRED,
            "Format must be speficied when converting from 'string' to 'binData'",
        )
    })?;
    let uuid_sub = o.subtype == UUID_SUBTYPE;
    if (format == Format::Uuid) != uuid_sub {
        return Err(fail("Only the 'uuid' format is allowed with the UUID subtype"));
    }
    match format {
        Format::Auto => Err(fail("Invalid format 'auto'")),
        Format::Base64 => bin::from_base64(s, false).ok_or_else(|| fail("invalid base64")),
        Format::Base64Url => {
            bin::from_base64(s, true).ok_or_else(|| fail("Invalid base64 character"))
        }
        Format::Hex => bin::from_hex(s).map_err(|why| fail(&why)),
        Format::Utf8 => Ok(s.as_bytes().to_vec()),
        Format::Uuid => bin::from_uuid(s).ok_or_else(|| fail(&format!("Invalid UUID string: {s}"))),
    }
}
