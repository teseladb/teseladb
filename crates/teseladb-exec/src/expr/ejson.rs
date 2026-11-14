//! `$serializeEJSON` and `$deserializeEJSON` (measured on a `refrouter` 9.0.4): a value to its
//! Extended JSON v2 form as documents (relaxed by default: numbers stay numbers, dates from 1970
//! to 9999 as ISO-8601 text), and back. Reading refuses a BSON-only value in the input and a type
//! wrapper of the wrong shape with RefDb's 241, which `onError` replaces.

use teseladb_bson::{Decimal128, Document, RawValue, Value};
use teseladb_mql::{ErrorCode, MqlError};

use super::arith::type_name_of;
use super::convert_bin::{self as bin, UUID_SUBTYPE};
use super::string::{date_text, double_text};
use super::values::display_eval;
use super::{Ctx, Eval, Expr, Scope, codes, error};

/// A conversion that failed.
const CONVERSION_FAILURE: ErrorCode = ErrorCode { code: 241, name: "ConversionFailure" };

/// The first and last milliseconds that relaxed Extended JSON writes as ISO-8601 text.
const FIRST_ISO_MS: i64 = 0;
const LAST_ISO_MS: i64 = 253_402_300_799_999;

/// `$serializeEJSON` or `$deserializeEJSON`.
#[derive(Clone, Debug, PartialEq)]
pub struct Ejson {
    serialize: bool,
    input: Expr,
    relaxed: Option<Expr>,
    on_error: Option<Expr>,
}

/// Parses one of the two operators; `None` for any other.
pub(super) fn parse(name: &str, arg: RawValue<'_>, ctx: &Ctx) -> Result<Option<Ejson>, MqlError> {
    let serialize = match name {
        "$serializeEJSON" => true,
        "$deserializeEJSON" => false,
        _ => return Ok(None),
    };
    let RawValue::Document(d) = arg else {
        return Err(error(
            codes::FAILED_TO_PARSE,
            format!(
                "{name} expects an object of named arguments but found: {}",
                super::arith::raw_type_name(arg)
            ),
        ));
    };
    let (mut input, mut relaxed, mut on_error) = (None, None, None);
    for (k, v) in d.iter() {
        match k {
            "input" => input = Some(Expr::parse_in(v, ctx)?),
            "relaxed" if serialize => relaxed = Some(Expr::parse_in(v, ctx)?),
            "onError" => on_error = Some(Expr::parse_in(v, ctx)?),
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
    Ok(Some(Ejson { serialize, input, relaxed, on_error }))
}

impl Ejson {
    /// Evaluates the expression.
    pub fn eval(&self, doc: &Document, scope: &Scope) -> Result<Eval, MqlError> {
        let relaxed = match &self.relaxed {
            None => true,
            Some(e) => match e.eval(doc, scope)? {
                Some(Value::Boolean(b)) => b,
                other => {
                    let shown = display_eval(&other);
                    return Err(error(
                        codes::BAD_VALUE,
                        format!("Unexpected value for relaxed: {shown}"),
                    ));
                }
            },
        };
        let input = match self.input.eval(doc, scope)? {
            None | Some(Value::Null | Value::Undefined) => return Ok(Some(Value::Null)),
            Some(v) => v,
        };
        let out = if self.serialize { Ok(serialize(&input, relaxed)) } else { deserialize(&input) };
        match out {
            Ok(v) => Ok(Some(v)),
            Err(msg) => match &self.on_error {
                Some(e) => e.eval(doc, scope),
                None => Err(error(CONVERSION_FAILURE, msg)),
            },
        }
    }
}

fn wrap(key: &str, v: impl Into<Value>) -> Value {
    Value::Document(Document::new().with(key, v))
}

/// `v` in Extended JSON v2, as documents.
fn serialize(v: &Value, relaxed: bool) -> Value {
    match v {
        Value::Int32(n) if relaxed => Value::Int32(*n),
        Value::Int32(n) => wrap("$numberInt", n.to_string()),
        Value::Int64(n) if relaxed => Value::Int64(*n),
        Value::Int64(n) => wrap("$numberLong", n.to_string()),
        Value::Double(x) if relaxed && x.is_finite() && !(*x == 0.0 && x.is_sign_negative()) => {
            Value::Double(*x)
        }
        Value::Double(x) if relaxed && *x == 0.0 => wrap("$numberDouble", "-0.0"),
        Value::Double(x) => wrap("$numberDouble", double_text(*x)),
        Value::Decimal128(d) => wrap("$numberDecimal", d.to_string()),
        Value::DateTime(ms) if relaxed && (FIRST_ISO_MS..=LAST_ISO_MS).contains(ms) => {
            wrap("$date", date_text(*ms))
        }
        Value::DateTime(ms) => wrap("$date", wrap("$numberLong", ms.to_string())),
        Value::ObjectId(id) => wrap("$oid", bin::object_id_text(id)),
        Value::Binary { subtype, bytes } if *subtype == UUID_SUBTYPE && bytes.len() == 16 => {
            wrap("$uuid", bin::uuid_text(bytes).unwrap_or_default())
        }
        Value::Binary { subtype, bytes } => wrap(
            "$binary",
            Document::new()
                .with("base64", bin::base64(bytes, false))
                .with("subType", format!("{subtype:x}")),
        ),
        Value::Regex { pattern, options } => wrap(
            "$regularExpression",
            Document::new().with("pattern", pattern.clone()).with("options", options.clone()),
        ),
        Value::Timestamp { time, increment } => wrap(
            "$timestamp",
            Document::new()
                .with("t", Value::Int64(i64::from(*time)))
                .with("i", Value::Int64(i64::from(*increment))),
        ),
        Value::MinKey => wrap("$minKey", Value::Int32(1)),
        Value::MaxKey => wrap("$maxKey", Value::Int32(1)),
        Value::JavaScript(c) => wrap("$code", c.clone()),
        Value::JavaScriptWithScope { code, scope } => Value::Document(
            Document::new()
                .with("$code", code.clone())
                .with("$scope", serialize(&Value::Document(scope.clone()), relaxed)),
        ),
        Value::Symbol(s) => wrap("$symbol", s.clone()),
        Value::Undefined => wrap("$undefined", true),
        Value::DbPointer { namespace, id } => wrap(
            "$dbPointer",
            // The ObjectId's text, as RefDb 9.0.4 writes it.
            Document::new().with("$ref", namespace.clone()).with("$id", bin::object_id_text(id)),
        ),
        Value::Document(d) => {
            let mut out = Document::new();
            for (k, x) in d.iter() {
                out.push(k, serialize(x, relaxed));
            }
            Value::Document(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(|x| serialize(x, relaxed)).collect()),
        other => other.clone(),
    }
}

/// The keys that make a document a type wrapper.
const WRAPPERS: [&str; 16] = [
    "$numberLong",
    "$numberInt",
    "$numberDouble",
    "$numberDecimal",
    "$date",
    "$oid",
    "$uuid",
    "$symbol",
    "$code",
    "$undefined",
    "$minKey",
    "$maxKey",
    "$timestamp",
    "$binary",
    "$regularExpression",
    "$dbPointer",
];

/// The value of Extended JSON `v`, or why it is not valid.
fn deserialize(v: &Value) -> Result<Value, String> {
    match v {
        Value::Document(d) => deserialize_doc(d),
        Value::Array(items) => {
            items.iter().map(deserialize).collect::<Result<_, _>>().map(Value::Array)
        }
        Value::Int32(_)
        | Value::Int64(_)
        | Value::Double(_)
        | Value::String(_)
        | Value::Boolean(_)
        | Value::Null => Ok(v.clone()),
        other => Err(format!(
            "Extended JSON cannot contain BSON-only types: {} is not available in JSON",
            type_name_of(other)
        )),
    }
}

fn deserialize_doc(d: &Document) -> Result<Value, String> {
    let first = d.iter().next().map(|(k, _)| k);
    let wrapper = first.filter(|k| WRAPPERS.contains(k));
    let Some(key) = wrapper else {
        if let Some(k) = d.iter().map(|(k, _)| k).find(|k| WRAPPERS.contains(k)) {
            return Err(format!("{k} must be the first field in the object"));
        }
        let mut out = Document::new();
        for (k, x) in d.iter() {
            out.push(k, deserialize(x)?);
        }
        return Ok(Value::Document(out));
    };
    let inner = d.get(key).cloned().unwrap_or(Value::Null);
    let text = |what: &str| -> Result<String, String> {
        match &inner {
            Value::String(s) => Ok(s.clone()),
            _ => Err(format!("{key} value must be of type {what}")),
        }
    };
    let number_words = "must be a string specifying a number, NaN, Infinity or -Infinity";
    let value = match key {
        "$numberLong" => Value::Int64(
            text("string")?
                .parse()
                .map_err(|_| format!("{key} value must be a string specifying an integer"))?,
        ),
        "$numberInt" => Value::Int32(
            text("string")?
                .parse()
                .map_err(|_| format!("{key} value must be a string specifying an integer"))?,
        ),
        "$numberDouble" => {
            let s = text("string")?;
            Value::Double(match s.as_str() {
                "NaN" => f64::NAN,
                "Infinity" => f64::INFINITY,
                "-Infinity" => f64::NEG_INFINITY,
                other => other.parse().map_err(|_| format!("{key} value {number_words}"))?,
            })
        }
        "$numberDecimal" => {
            let s = text("string")?;
            Value::Decimal128(
                s.parse::<Decimal128>().map_err(|_| format!("{key} value {number_words}"))?,
            )
        }
        "$date" => match &inner {
            Value::String(s) => Value::DateTime(
                super::date_parse::parse_iso(s)
                    .ok_or_else(|| "$date must be a valid ISO-8601 string".to_owned())?,
            ),
            // The inner document is read as Extended JSON and must give a long.
            Value::Document(n) if matches!(n.get("$numberLong"), Some(Value::String(_))) => {
                match deserialize_doc(n)? {
                    Value::Int64(ms) => Value::DateTime(ms),
                    _ => return Err("$date must be a 64-bit signed integer as a string".to_owned()),
                }
            }
            Value::Document(_) => {
                return Err("$date must be a 64-bit signed integer as a string".to_owned());
            }
            _ => return Err("$date value must be of type object or string".to_owned()),
        },
        "$oid" => {
            let s = text("string")?;
            if s.len() != 24 {
                return Err("$oid must be a 24 character hexadecimal string".to_owned());
            }
            let bytes =
                bin::from_hex(&s).map_err(|_| "$oid value must be a valid objectId".to_owned())?;
            let mut id = [0u8; 12];
            id.copy_from_slice(&bytes);
            Value::ObjectId(id)
        }
        "$uuid" => {
            let s = text("string")?;
            let bytes = bin::from_uuid(&s)
                .ok_or_else(|| "$uuid value must be a valid UUID string".to_owned())?;
            Value::Binary { subtype: UUID_SUBTYPE, bytes }
        }
        "$symbol" => Value::Symbol(text("string")?),
        "$code" => {
            let code = text("string")?;
            match d.get("$scope") {
                Some(Value::Document(scope)) => match deserialize_doc(scope)? {
                    Value::Document(scope) => Value::JavaScriptWithScope { code, scope },
                    _ => return Err("$scope value must be of type object".to_owned()),
                },
                Some(_) => return Err("$scope value must be of type object".to_owned()),
                None => Value::JavaScript(code),
            }
        }
        "$undefined" => match inner {
            Value::Boolean(true) => Value::Undefined,
            _ => return Err("$undefined value must be true".to_owned()),
        },
        "$minKey" | "$maxKey" => match inner {
            Value::Int32(1) => {
                if key == "$minKey" {
                    Value::MinKey
                } else {
                    Value::MaxKey
                }
            }
            Value::Int32(_) => return Err(format!("{key} value must be the integer 1")),
            _ => return Err(format!("{key} value must be of type int")),
        },
        "$timestamp" => timestamp(&inner)?,
        "$binary" => binary(&inner)?,
        "$regularExpression" => regex(&inner)?,
        _ => dbpointer(&inner)?,
    };
    // The value is checked before the fields beside it.
    let others = d.iter().filter(|(k, _)| *k != key && !(key == "$code" && *k == "$scope")).count();
    if others > 0 {
        // RefDb words a field beside `$code` other than `$scope` as a misplaced `$code`.
        if key == "$code" {
            return Err("$code must be the first field in the object".to_owned());
        }
        return Err(format!("{key} object contains additional fields"));
    }
    Ok(value)
}

fn object_of<'a>(key: &str, v: &'a Value) -> Result<&'a Document, String> {
    match v {
        Value::Document(d) => Ok(d),
        _ => Err(format!("{key} value must be of type object")),
    }
}

fn timestamp(v: &Value) -> Result<Value, String> {
    let d = object_of("$timestamp", v)?;
    let part = |name: &str| -> Result<u32, String> {
        let n = match d.get(name) {
            Some(Value::Int32(n)) => i64::from(*n),
            Some(Value::Int64(n)) => *n,
            _ => return Err(format!("$timestamp.{name} value must be of type int or long")),
        };
        // A wider number keeps its low 32 bits, as RefDb reads it.
        Ok(n as u32)
    };
    Ok(Value::Timestamp { time: part("t")?, increment: part("i")? })
}

fn binary(v: &Value) -> Result<Value, String> {
    let d = object_of("$binary", v)?;
    let Some(Value::String(data)) = d.get("base64") else {
        return Err("$binary.base64 value must be of type string".to_owned());
    };
    let Some(Value::String(sub)) = d.get("subType") else {
        return Err("$binary.subType value must be of type string".to_owned());
    };
    let bytes = bin::from_base64(data, false)
        .ok_or_else(|| "$binary.base64 must be a valid base64 encoded string".to_owned())?;
    let subtype = (1..=2)
        .contains(&sub.len())
        .then(|| u8::from_str_radix(sub, 16).ok())
        .flatten()
        .ok_or_else(|| {
            "$binary.subType must be a hex representation of a single byte".to_owned()
        })?;
    Ok(Value::Binary { subtype, bytes })
}

/// The flags a regular expression keeps, in the order RefDb writes them.
const REGEX_FLAGS: &str = "ilmsux";

fn regex(v: &Value) -> Result<Value, String> {
    let d = object_of("$regularExpression", v)?;
    let Some(Value::String(pattern)) = d.get("pattern") else {
        return Err("$regularExpression.pattern value must be of type string".to_owned());
    };
    let Some(Value::String(options)) = d.get("options") else {
        return Err("$regularExpression.options value must be of type string".to_owned());
    };
    let kept: String = REGEX_FLAGS.chars().filter(|c| options.contains(*c)).collect();
    Ok(Value::Regex { pattern: pattern.clone(), options: kept })
}

fn dbpointer(v: &Value) -> Result<Value, String> {
    let d = object_of("$dbPointer", v)?;
    let Some(Value::String(namespace)) = d.get("$ref") else {
        return Err("$dbPointer.$ref value must be of type string".to_owned());
    };
    // `$id` is read as the ObjectId's text, as RefDb 9.0.4 reads it.
    let Some(Value::String(hex)) = d.get("$id") else {
        return Err("$dbPointer.$id value must be of type string".to_owned());
    };
    if d.iter().count() > 2 {
        return Err(
            "$dbPointer object contains additional fields, expected $ref and $id only".to_owned()
        );
    }
    if hex.len() != 24 {
        return Err("$id must be a 24 character hexadecimal string".to_owned());
    }
    let bytes = bin::from_hex(hex)
        .map_err(|_| "$dbPointer value must contain a valid $ref and $id".to_owned())?;
    let mut id = [0u8; 12];
    id.copy_from_slice(&bytes);
    Ok(Value::DbPointer { namespace: namespace.clone(), id })
}
