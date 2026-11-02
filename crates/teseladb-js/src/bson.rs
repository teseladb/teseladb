//! BSON in JavaScript (016 FR-003): the types RefDb shows with its own constructors
//! (`ObjectId`, `NumberLong`, `NumberInt`, `NumberDecimal`, `ISODate`, `BinData`, `Timestamp`,
//! `MinKey`, `MaxKey`, `DBRef`) and the conversions both ways, as a `refrouter` 9.0.4 does them
//! (measured on 02-11): every number comes back as a double except a `NumberInt`, an `int32`
//! argument is a plain number, `undefined` is `null`, a function is `Code` and a symbol cannot be
//! written.

use teseladb_bson::{Decimal128, Document, Value};

use crate::builtins::method;
use crate::interp::{Ctrl, Interp, R};
use crate::value::*;

fn arg(args: &[Val], i: usize) -> Val {
    args.get(i).cloned().unwrap_or(Val::Undef)
}

/// The constructor names, in the order of [`Bson`]'s variants that have one.
const TYPES: [&str; 9] = [
    "NumberLong",
    "NumberInt",
    "NumberDecimal",
    "ObjectId",
    "BinData",
    "Timestamp",
    "MinKey",
    "MaxKey",
    "DBRef",
];

fn type_name(b: &Bson) -> &'static str {
    match b {
        Bson::Long(_) => "NumberLong",
        Bson::Int(_) => "NumberInt",
        Bson::Decimal(_) => "NumberDecimal",
        Bson::ObjectId(_) => "ObjectId",
        Bson::BinData(..) => "BinData",
        Bson::Timestamp(..) => "Timestamp",
        Bson::MinKey => "MinKey",
        Bson::MaxKey => "MaxKey",
        Bson::DbPointer(..) => "DBPointer",
        Bson::Symbol(_) => "Symbol",
        Bson::Code(_) => "Code",
    }
}

fn proto_of(it: &Interp, name: &str) -> Option<ObjRef> {
    let c = it.realm.bson.iter().find(|(n, _)| &**n == name)?.1.clone();

    c.borrow().get_own("prototype")?.value.as_obj().cloned()
}

pub fn make(it: &Interp, b: Bson) -> R {
    let proto = proto_of(it, type_name(&b)).or_else(|| Some(it.realm.object.clone()));
    Ok(Val::Obj(it.alloc(proto, Kind::Bson(b))?))
}

fn this_bson(it: &Interp, this: &Val) -> R<Bson> {
    match this {
        Val::Obj(o) => match &o.borrow().kind {
            Kind::Bson(b) => Ok(b.clone()),
            _ => Err(crate::builtins::incompatible(it, "BSON method")),
        },
        _ => Err(crate::builtins::incompatible(it, "BSON method")),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn install(it: &mut Interp, g: &ObjRef) -> R<()> {
    let ctors: [NativeFn; 9] = [
        number_long,
        number_int,
        number_decimal,
        object_id,
        bin_data,
        timestamp,
        min_key,
        max_key,
        db_ref,
    ];
    for (name, f) in TYPES.iter().zip(ctors) {
        let proto = it.object()?;
        let c = it.native(name, f, 1)?;
        it.define(&c, "prototype", Prop::hidden(Val::Obj(proto.clone())))?;
        it.define(&proto, "constructor", Prop::hidden(Val::Obj(c.clone())))?;
        it.define(g, name, Prop::hidden(Val::Obj(c.clone())))?;
        method(it, &proto, "toString", to_string, 0)?;
        method(it, &proto, "tojson", to_string, 0)?;
        method(it, &proto, "valueOf", value_of, 0)?;
        method(it, &proto, "toNumber", value_of, 0)?;
        method(it, &proto, "equals", equals, 1)?;
        it.realm.bson.push(((*name).into(), c));
    }
    if let Some(p) = proto_of(it, "ObjectId") {
        method(it, &p, "getTimestamp", object_id_time, 0)?;
        let getter = it.native("str", object_id_hex, 0)?;
        it.define(&p, "str", Prop { getter: Some(getter), ..Prop::hidden(Val::Undef) })?;
    }
    if let Some(p) = proto_of(it, "Timestamp") {
        let t = it.native(
            "t",
            |it, this, _| match this_bson(it, this)? {
                Bson::Timestamp(t, _) => Ok(Val::Num(f64::from(t))),
                _ => Ok(Val::Undef),
            },
            0,
        )?;
        let i = it.native(
            "i",
            |it, this, _| match this_bson(it, this)? {
                Bson::Timestamp(_, i) => Ok(Val::Num(f64::from(i))),
                _ => Ok(Val::Undef),
            },
            0,
        )?;
        it.define(&p, "t", Prop { getter: Some(t), ..Prop::hidden(Val::Undef) })?;
        it.define(&p, "i", Prop { getter: Some(i), ..Prop::hidden(Val::Undef) })?;
    }
    // `ISODate(text)` is a `Date`.
    method(it, g, "ISODate", iso_date, 1)?;
    Ok(())
}

fn number_long(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let n = match arg(a, 0) {
        Val::Undef => 0,
        Val::Str(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| it.error("Error", &format!("could not convert \"{s}\" to NumberLong")))?,
        v => {
            let n = it.number_of(&v)?;
            if !n.is_finite() {
                return Err(it.error("Error", "number passed to NumberLong must be finite"));
            }
            n as i64
        }
    };
    make(it, Bson::Long(n))
}

fn number_int(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let n = match arg(a, 0) {
        Val::Undef => 0,
        Val::Str(s) => s.trim().parse::<i32>().unwrap_or(0),
        v => crate::number::to_int32(it.number_of(&v)?),
    };
    make(it, Bson::Int(n))
}

fn number_decimal(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let text = match arg(a, 0) {
        Val::Undef => "0".into(),
        v => it.str_of(&v)?,
    };
    let d: Decimal128 = text
        .trim()
        .parse()
        .map_err(|_| it.error("Error", &format!("Invalid decimal string: {text}")))?;
    make(it, Bson::Decimal(d))
}

fn object_id(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let id = match arg(a, 0) {
        Val::Undef => {
            let mut id = [0u8; 12];
            let secs = (it.now_ms / 1000.0) as u32;
            id[..4].copy_from_slice(&secs.to_be_bytes());
            let r = (crate::math::next_random(it) * (1u64 << 53) as f64) as u64;
            id[4..].copy_from_slice(&r.to_be_bytes());
            id
        }
        Val::Str(s) if s.len() == 24 && s.bytes().all(|b| b.is_ascii_hexdigit()) => {
            let mut id = [0u8; 12];
            for (i, slot) in id.iter_mut().enumerate() {
                *slot = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap_or(0);
            }
            id
        }
        v => return Err(it.error("Error", &format!("invalid object id: {}", it.show(&v)))),
    };
    make(it, Bson::ObjectId(id))
}

fn bin_data(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let sub = it.number_of(&arg(a, 0))?;
    let text = it.str_of(&arg(a, 1))?;
    let bytes = base64_decode(&text).ok_or_else(|| it.error("Error", "BinData: invalid base64"))?;
    make(it, Bson::BinData(sub as u8, bytes))
}

fn timestamp(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let t = if matches!(arg(a, 0), Val::Undef) { 0.0 } else { it.number_of(&arg(a, 0))? };
    let i = if matches!(arg(a, 1), Val::Undef) { 0.0 } else { it.number_of(&arg(a, 1))? };
    make(it, Bson::Timestamp(t as u32, i as u32))
}

fn min_key(it: &mut Interp, _: &Val, _: &[Val]) -> R {
    make(it, Bson::MinKey)
}

fn max_key(it: &mut Interp, _: &Val, _: &[Val]) -> R {
    make(it, Bson::MaxKey)
}

/// `DBRef(collection, id, db)`: an object with `$ref`, `$id` and `$db`.
fn db_ref(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let o = it.alloc(proto_of(it, "DBRef"), Kind::Ordinary)?;
    it.define(&o, "$ref", Prop::data(arg(a, 0)))?;
    it.define(&o, "$id", Prop::data(arg(a, 1)))?;
    if !matches!(arg(a, 2), Val::Undef) {
        it.define(&o, "$db", Prop::data(arg(a, 2)))?;
    }
    Ok(Val::Obj(o))
}

fn iso_date(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let t = match arg(a, 0) {
        Val::Undef => it.now_ms,
        v => {
            let s = it.str_of(&v)?;
            let t = crate::date::parse(&s);
            if t.is_nan() {
                return Err(it.error("Error", &format!("invalid ISO date: {s}")));
            }
            t
        }
    };
    crate::date::make_date(it, t)
}

fn to_string(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let s = match this_bson(it, this)? {
        Bson::Long(n) => format!("NumberLong({n})"),
        Bson::Int(n) => format!("NumberInt({n})"),
        Bson::Decimal(d) => format!("NumberDecimal(\"{d}\")"),
        Bson::ObjectId(id) => format!("ObjectId(\"{}\")", hex(&id)),
        Bson::BinData(t, b) => format!("BinData({t},\"{}\")", base64_encode(&b)),
        Bson::Timestamp(t, i) => format!("Timestamp({t}, {i})"),
        Bson::MinKey => "[object MinKey]".into(),
        Bson::MaxKey => "[object MaxKey]".into(),
        Bson::DbPointer(ns, id) => format!("DBPointer(\"{ns}\", ObjectId(\"{}\"))", hex(&id)),
        Bson::Symbol(s) | Bson::Code(s) => s,
    };
    it.string(s)
}

fn value_of(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(match this_bson(it, this)? {
        Bson::Long(n) => Val::Num(n as f64),
        Bson::Int(n) => Val::Num(f64::from(n)),
        Bson::Decimal(d) => Val::Num(d.to_string().parse().unwrap_or(f64::NAN)),
        Bson::Timestamp(t, _) => Val::Num(f64::from(t)),
        _ => this.clone(),
    })
}

fn equals(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let x = this_bson(it, this)?;
    Ok(Val::Bool(match &arg(a, 0) {
        Val::Obj(o) => matches!(&o.borrow().kind, Kind::Bson(y) if *y == x),
        _ => false,
    }))
}

fn object_id_hex(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    match this_bson(it, this)? {
        Bson::ObjectId(id) => it.string(hex(&id)),
        _ => Err(crate::builtins::incompatible(it, "ObjectId.prototype.toHexString")),
    }
}

fn object_id_time(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    match this_bson(it, this)? {
        Bson::ObjectId(id) => {
            let secs = u32::from_be_bytes([id[0], id[1], id[2], id[3]]);
            crate::date::make_date(it, f64::from(secs) * 1000.0)
        }
        _ => Err(crate::builtins::incompatible(it, "ObjectId.prototype.getTimestamp")),
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(b: &[u8]) -> String {
    let mut out = String::new();
    for chunk in b.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for k in 0..4 {
            if k <= chunk.len() {
                out.push(B64[((n >> (18 - 6 * k)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = B64.iter().position(|&x| x == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

// Conversions.

/// A BSON value as JavaScript sees it.
pub fn from_bson(it: &mut Interp, v: &Value, depth: usize) -> R {
    if depth > 200 {
        return Err(it.error("InternalError", "too much recursion"));
    }
    Ok(match v {
        Value::Double(x) => Val::Num(*x),
        Value::Int32(n) => Val::Num(f64::from(*n)),
        Value::Int64(n) => make(it, Bson::Long(*n))?,
        Value::Decimal128(d) => make(it, Bson::Decimal(*d))?,
        Value::String(s) => it.string(s.clone())?,
        Value::Boolean(b) => Val::Bool(*b),
        Value::Null => Val::Null,
        Value::Undefined => Val::Undef,
        Value::DateTime(t) => crate::date::make_date(it, *t as f64)?,
        Value::ObjectId(id) => make(it, Bson::ObjectId(*id))?,
        Value::Binary { subtype, bytes } => make(it, Bson::BinData(*subtype, bytes.clone()))?,
        Value::Timestamp { time, increment } => make(it, Bson::Timestamp(*time, *increment))?,
        Value::MinKey => make(it, Bson::MinKey)?,
        Value::MaxKey => make(it, Bson::MaxKey)?,
        Value::Regex { pattern, options } => {
            let flags: String = options.chars().filter(|c| "gimsuy".contains(*c)).collect();
            crate::builtins::make_regexp(it, pattern.as_str().into(), flags.into())?
        }
        Value::DbPointer { namespace, id } => make(it, Bson::DbPointer(namespace.clone(), *id))?,
        Value::Symbol(s) => make(it, Bson::Symbol(s.clone()))?,
        Value::JavaScript(code) | Value::JavaScriptWithScope { code, .. } => {
            match function_of(it, code)? {
                Some(f) => f,
                None => make(it, Bson::Code(code.clone()))?,
            }
        }
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for x in items {
                out.push(from_bson(it, x, depth + 1)?);
            }
            it.array(out)?
        }
        Value::Document(d) => from_document(it, d, depth)?,
    })
}

/// The function `code` defines, as RefDb gives a `Code` value to JavaScript (a function
/// stored in `system.js` is called by its name); `None` when the code is not one function.
fn function_of(it: &mut Interp, code: &str) -> R<Option<Val>> {
    let Ok(body) = crate::parser::parse_script(code) else { return Ok(None) };
    let [crate::ast::Stmt::Expr(crate::ast::Expr::Func(def))] = body.as_slice() else {
        return Ok(None);
    };
    let env = it.global_env.clone();
    it.closure(def, &env).map(Some)
}

pub fn from_document(it: &mut Interp, d: &Document, depth: usize) -> R {
    let o = it.object()?;
    it.grow(&o, d.len())?;
    for (k, x) in d.iter() {
        let v = from_bson(it, x, depth + 1)?;
        o.borrow_mut().props.push((k.into(), Prop::data(v)));
    }
    Ok(Val::Obj(o))
}

/// The error of a value that has no BSON form (measured: 16662 for a symbol).
pub fn not_convertible(name: &str) -> Ctrl {
    Ctrl::Fatal(crate::interp::Fatal::Convert(format!(
        "unable to convert JavaScript property to ref element {name}"
    )))
}

/// A JavaScript value as BSON; `name` is the field it is written to, for the error.
pub fn to_bson(it: &mut Interp, v: &Val, name: &str, depth: usize) -> R<Value> {
    it.step()?;
    if depth > 150 {
        return Err(it.error("InternalError", "too much recursion"));
    }
    Ok(match v {
        Val::Undef | Val::Null => Value::Null,
        Val::Bool(b) => Value::Boolean(*b),
        Val::Num(n) => Value::Double(*n),
        Val::Str(s) => Value::String(crate::strings::to_utf8(s).into_owned()),
        Val::Sym(_) | Val::BigInt(_) => return Err(not_convertible(name)),
        Val::Obj(o) => {
            let kind = o.borrow().kind.clone();
            match kind {
                Kind::Bson(b) => match b {
                    Bson::Long(n) => Value::Int64(n),
                    Bson::Int(n) => Value::Int32(n),
                    Bson::Decimal(d) => Value::Decimal128(d),
                    Bson::ObjectId(id) => Value::ObjectId(id),
                    Bson::BinData(subtype, bytes) => Value::Binary { subtype, bytes },
                    Bson::Timestamp(time, increment) => Value::Timestamp { time, increment },
                    Bson::MinKey => Value::MinKey,
                    Bson::MaxKey => Value::MaxKey,
                    Bson::DbPointer(namespace, id) => Value::DbPointer { namespace, id },
                    Bson::Symbol(s) => Value::Symbol(s),
                    Bson::Code(c) => Value::JavaScript(c),
                },
                Kind::Date(t) => Value::DateTime(if t.is_nan() { i64::MIN } else { t as i64 }),
                Kind::Num(n) => Value::Double(n),
                Kind::Str(s) => Value::String(s.to_string()),
                Kind::Bool(b) => Value::Boolean(b),
                Kind::RegExp(p, f) => Value::Regex {
                    pattern: p.to_string(),
                    options: f.chars().filter(|c| *c != 'g').collect(),
                },
                Kind::Func(Func::Closure { def, .. }) => Value::JavaScript(def.source.to_string()),
                Kind::Func(_) => Value::JavaScript("function() { [native code] }".into()),
                Kind::Array(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for (i, x) in items.iter().enumerate() {
                        out.push(to_bson(it, x, &i.to_string(), depth + 1)?);
                    }
                    Value::Array(out)
                }
                _ => {
                    let mut d = Document::new();
                    for k in it.keys(o, true)? {
                        let x = it.get(v, &k)?;
                        if x.is_callable() && matches!(o.borrow().kind, Kind::Error) {
                            continue;
                        }
                        d.push(&*k, to_bson(it, &x, &k, depth + 1)?);
                    }
                    Value::Document(d)
                }
            }
        }
    })
}
