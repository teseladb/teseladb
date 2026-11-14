//! Reading Extended JSON v2, canonical or relaxed, into a [`Document`]. A type wrapper is known
//! by its key and must have exactly its shape: a field too many, one missing or of the wrong JSON
//! type is an error. An object with `$` keys that are not a wrapper (a `DBRef`'s `$ref` and `$id`,
//! a query operator) is an ordinary document. Plain JSON numbers become int32, int64 or double
//! by their range.

use teseladb_json::{Number, Object, Value as Json};

use super::{ExtJsonError, base64, date};
use crate::decimal128::Decimal128;
use crate::types;
use crate::value::{Document, Value};

/// Keys that make an object a type wrapper.
const WRAPPERS: [&str; 16] = [
    "$oid",
    "$symbol",
    "$numberInt",
    "$numberLong",
    "$numberDouble",
    "$numberDecimal",
    "$binary",
    "$code",
    "$timestamp",
    "$regularExpression",
    "$dbPointer",
    "$date",
    "$minKey",
    "$maxKey",
    "$undefined",
    "$uuid",
];

fn err(what: &str) -> ExtJsonError {
    ExtJsonError(what.to_owned())
}

pub(crate) fn document(o: &Object) -> Result<Document, ExtJsonError> {
    let mut d = Document::new();
    for (k, v) in o.iter() {
        if k.contains('\0') {
            return Err(err("a field name with a 00 byte"));
        }
        d.push(k, value(v)?);
    }
    Ok(d)
}

fn value(v: &Json) -> Result<Value, ExtJsonError> {
    Ok(match v {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Boolean(*b),
        Json::String(s) => Value::String(s.clone()),
        Json::Number(n) => number(n),
        Json::Array(items) => Value::Array(items.iter().map(value).collect::<Result<_, _>>()?),
        Json::Object(o) => match WRAPPERS.iter().find(|w| o.get(w).is_some()) {
            Some(w) => wrapper(w, o)?,
            None => Value::Document(document(o)?),
        },
    })
}

/// A plain JSON number: int32 or int64 if it is an integer that fits, a double otherwise.
#[expect(
    clippy::cast_precision_loss,
    reason = "an integer above i64::MAX becomes the nearest double, as RefDb reads it"
)]
fn number(n: &Number) -> Value {
    match n {
        Number::Int(i) => i32::try_from(*i).map_or(Value::Int64(*i), Value::Int32),
        Number::UInt(u) => Value::Double(*u as f64),
        Number::Float(x) => Value::Double(*x),
        // Digits only: as a double they cannot fail (a huge one is an infinity).
        Number::Big(text) => Value::Double(text.parse().expect("digits read as a double")),
    }
}

fn wrapper(key: &str, o: &Object) -> Result<Value, ExtJsonError> {
    let only = |keys: &[&str]| o.len() == keys.len() && keys.iter().all(|k| o.get(k).is_some());
    let shape = |ok: bool| {
        if ok { Ok(()) } else { Err(err(&format!("{key} with other fields"))) }
    };
    let text = |k: &str| {
        o.get(k).and_then(Json::as_str).ok_or_else(|| err(&format!("{k} needs a string")))
    };
    if key == "$code" {
        shape(only(&["$code"]) || only(&["$code", "$scope"]))?;
        let code = text("$code")?.to_owned();
        return match o.get("$scope") {
            None => Ok(Value::JavaScript(code)),
            Some(Json::Object(scope)) => {
                Ok(Value::JavaScriptWithScope { code, scope: document(scope)? })
            }
            Some(_) => Err(err("$scope needs a document")),
        };
    }
    shape(only(&[key]))?;
    let inner = o.get(key).expect("the wrapper's key");
    Ok(match key {
        "$oid" => Value::ObjectId(object_id(text(key)?)?),
        "$symbol" => Value::Symbol(text(key)?.to_owned()),
        "$numberInt" => Value::Int32(integer(text(key)?)?),
        "$numberLong" => Value::Int64(integer(text(key)?)?),
        "$numberDouble" => Value::Double(double(text(key)?)?),
        "$numberDecimal" => {
            Value::Decimal128(text(key)?.parse::<Decimal128>().map_err(|e| err(&e.to_string()))?)
        }
        "$binary" => binary(inner)?,
        "$uuid" => uuid(text(key)?)?,
        "$timestamp" => {
            let t = fields(inner, &["t", "i"])?;
            let n = |j: &Json| {
                j.as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| err("$timestamp needs 32-bit integers"))
            };
            Value::Timestamp { time: n(t[0])?, increment: n(t[1])? }
        }
        "$regularExpression" => {
            let r = fields(inner, &["pattern", "options"])?;
            let s = |j: &Json| {
                j.as_str().map(str::to_owned).ok_or_else(|| err("$regularExpression needs strings"))
            };
            let (pattern, options) = (s(r[0])?, s(r[1])?);
            if pattern.contains('\0') || options.contains('\0') {
                return Err(err("a regular expression with a 00 byte"));
            }
            Value::Regex { pattern, options }
        }
        "$dbPointer" => {
            let p = fields(inner, &["$ref", "$id"])?;
            let namespace = p[0].as_str().ok_or_else(|| err("$ref needs a string"))?.to_owned();
            match p[1] {
                Json::Object(id) if id.len() == 1 => match id.get("$oid").and_then(Json::as_str) {
                    Some(hex) => Value::DbPointer { namespace, id: object_id(hex)? },
                    None => return Err(err("$id needs an $oid")),
                },
                _ => return Err(err("$id needs an $oid")),
            }
        }
        "$date" => Value::DateTime(match inner {
            Json::String(iso) => {
                date::from_iso(iso).ok_or_else(|| err("$date with a date that is not ISO-8601"))?
            }
            Json::Object(n) if n.len() == 1 => match n.get("$numberLong").and_then(Json::as_str) {
                Some(t) => integer(t)?,
                None => return Err(err("$date needs $numberLong")),
            },
            _ => return Err(err("$date needs a string or $numberLong")),
        }),
        "$minKey" | "$maxKey" => match inner.as_i64() {
            Some(1) if key == "$minKey" => Value::MinKey,
            Some(1) => Value::MaxKey,
            _ => return Err(err(&format!("{key} needs 1"))),
        },
        _ => match inner {
            Json::Bool(true) => Value::Undefined,
            _ => return Err(err("$undefined needs true")),
        },
    })
}

/// The values of exactly `names`, in that order, from an object.
fn fields<'a>(v: &'a Json, names: &[&str]) -> Result<Vec<&'a Json>, ExtJsonError> {
    let o = v.as_object().ok_or_else(|| err("a wrapper needs a document"))?;
    if o.len() != names.len() {
        return Err(err("a wrapper with other fields"));
    }
    names.iter().map(|n| o.get(n).ok_or_else(|| err(&format!("missing {n}")))).collect()
}

fn binary(inner: &Json) -> Result<Value, ExtJsonError> {
    let b = fields(inner, &["base64", "subType"])?;
    let (data, sub) = (b[0].as_str(), b[1].as_str());
    let (Some(data), Some(sub)) = (data, sub) else {
        return Err(err("$binary needs strings"));
    };
    if sub.is_empty() || sub.len() > 2 {
        return Err(err("$binary subType needs one or two hexadecimal digits"));
    }
    let subtype =
        u8::from_str_radix(sub, 16).map_err(|_| err("$binary subType is not hexadecimal"))?;
    let bytes = base64::decode(data).ok_or_else(|| err("$binary is not base64"))?;
    Ok(Value::Binary { subtype, bytes })
}

/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`, subtype 4.
fn uuid(text: &str) -> Result<Value, ExtJsonError> {
    let groups: Vec<&str> = text.split('-').collect();
    let lens: Vec<usize> = groups.iter().map(|g| g.len()).collect();
    if lens != [8, 4, 4, 4, 12] {
        return Err(err("$uuid is not 8-4-4-4-12 hexadecimal digits"));
    }
    let hex: String = groups.concat();
    Ok(Value::Binary {
        subtype: types::BINARY_UUID,
        bytes: hex_bytes(&hex).ok_or_else(|| err("$uuid is not hexadecimal"))?,
    })
}

fn object_id(text: &str) -> Result<[u8; 12], ExtJsonError> {
    let bytes = hex_bytes(text)
        .filter(|b| b.len() == 12)
        .ok_or_else(|| err("$oid needs 24 hexadecimal digits"))?;
    Ok(bytes.try_into().expect("12 bytes"))
}

fn hex_bytes(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect()
}

/// A decimal integer: optional `-` and digits.
fn integer<T: std::str::FromStr>(text: &str) -> Result<T, ExtJsonError> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return Err(err("an integer needs digits"));
    }
    text.parse().map_err(|_| err("an integer out of range"))
}

/// `Infinity`, `-Infinity`, `NaN` or a decimal number.
fn double(text: &str) -> Result<f64, ExtJsonError> {
    match text {
        "Infinity" => Ok(f64::INFINITY),
        "-Infinity" => Ok(f64::NEG_INFINITY),
        "NaN" => Ok(f64::NAN),
        _ if text.bytes().all(|c| c.is_ascii_digit() || b"+-.eE".contains(&c)) => {
            text.parse().map_err(|_| err("$numberDouble is not a number"))
        }
        _ => Err(err("$numberDouble is not a number")),
    }
}
