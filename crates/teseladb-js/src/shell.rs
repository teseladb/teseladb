//! The globals of the RefDb shell that the JavaScript engine of RefDb 9.0.4 defines in every
//! scope (measured on a `refrouter` 9.0.4 on 02-11): the native ones here, and those that engine
//! writes in JavaScript (`tojson`, `assert`, `hex_md5`…) in `prelude.js`, TeselaDB's own code
//! written from their measured inputs, outputs and errors.

use teseladb_bson::{Document, RawDoc, Value};

use crate::bson::{base64_encode, make, to_bson};
use crate::builtins::method;
use crate::interp::{Ctrl, Fatal, Interp, R};
use crate::value::*;

/// What `getJSHeapLimitMB()` gives on the `refrouter` 9.0.4.
const HEAP_LIMIT_MB: f64 = 100.0;
/// Binary subtypes of `UUID` and `MD5`.
const SUBTYPE_UUID_OLD: u8 = 3;
const SUBTYPE_UUID: u8 = 4;
const SUBTYPE_MD5: u8 = 5;

fn arg(a: &[Val], i: usize) -> Val {
    a.get(i).cloned().unwrap_or(Val::Undef)
}

/// Defines the shell's globals on `g`, after the other built-ins and before they are frozen.
pub fn install(it: &mut Interp, g: &ObjRef) -> R<()> {
    let natives: [(&str, NativeFn); 13] = [
        ("gc", |_, _, _| Ok(Val::Undef)),
        ("version", |_, _, _| Err(Ctrl::Fatal(Fatal::Unreachable))),
        ("buildInfo", |it, _, _| Ok(Val::Obj(it.object()?))),
        ("getJSHeapLimitMB", |_, _, _| Ok(Val::Num(HEAP_LIMIT_MB))),
        ("HexData", hex_data),
        ("MD5", md5),
        ("UUID", uuid),
        ("bsonWoCompare", wo_compare),
        ("bsonUnorderedFieldsCompare", unordered_compare),
        ("bsonBinaryEqual", binary_equal),
        ("bsonObjToArray", obj_to_array),
        ("bsonToBase64", to_base64),
        ("bsonGetImmutable", get_immutable),
    ];
    for (name, f) in natives {
        method(it, g, name, f, 0)?;
    }
    code_and_pointer(it, g)?;
    prototype_additions(it, g)?;
    let undefined = Prop { writable: false, configurable: false, ..Prop::hidden(Val::Undef) };
    it.define(g, "undefined", undefined)?;
    run_prelude(it)
}

/// A function property that the shell assigns: enumerable (frozen with the rest later).
fn assigned(it: &Interp, o: &ObjRef, key: &str, name: &str, f: NativeFn) -> R<()> {
    let fo = it.native(name, f, 0)?;
    it.define(o, key, Prop::data(Val::Obj(fo)))
}

fn proto_of_ctor(g: &ObjRef, name: &str) -> Option<ObjRef> {
    let c = g.borrow().get_own(name)?.value.as_obj().cloned()?;
    c.borrow().get_own("prototype")?.value.as_obj().cloned()
}

/// What the shell adds to the prototypes of the built-ins and of the BSON types (measured on a
/// `refrouter` 9.0.4 on 02-11): `tojson` and `toJSON` of each type, `ltrim` and `rtrim` of strings,
/// the methods of `BinData`.
fn prototype_additions(it: &mut Interp, g: &ObjRef) -> R<()> {
    if let Some(p) = proto_of_ctor(g, "String") {
        assigned(it, &p, "ltrim", "trimStart", crate::strings::trim_start)?;
        assigned(it, &p, "rtrim", "trimEnd", crate::strings::trim_end)?;
    }
    if let Some(p) = proto_of_ctor(g, "RegExp") {
        assigned(it, &p, "tojson", "toString", crate::builtins::regexp_to_string)?;
        method(it, &p, "toJSON", regexp_to_json, 0)?;
    }
    for name in ["ObjectId", "NumberLong", "NumberInt", "NumberDecimal", "Timestamp", "BinData"] {
        let Some(p) = proto_of_ctor(g, name) else { continue };
        let tojson = p.borrow().get_own("tojson").map(|q| q.value.clone());
        if let Some(f) = tojson {
            it.define(&p, "tojson", Prop::data(f))?;
        }
        method(it, &p, "toJSON", bson_to_json, 0)?;
    }
    if let Some(p) = proto_of_ctor(g, "ObjectId") {
        it.define(&p, "isObjectId", Prop::data(Val::Bool(true)))?;
    }
    if let Some(p) = proto_of_ctor(g, "BinData") {
        method(it, &p, "base64", |it, t, _| bin_part(it, t, BinPart::Base64), 0)?;
        method(it, &p, "hex", |it, t, _| bin_part(it, t, BinPart::Hex), 0)?;
        assigned(it, &p, "length", "", |it, t, _| bin_part(it, t, BinPart::Length))?;
        assigned(it, &p, "subtype", "", |it, t, _| bin_part(it, t, BinPart::Subtype))?;
    }
    for name in ["MinKey", "MaxKey"] {
        if let Some(Val::Obj(c)) = g.borrow().get_own(name).map(|p| p.value.clone()) {
            method(it, &c, "tojson", key_tojson, 0)?;
            method(it, &c, "toJSON", key_to_json, 0)?;
            method(it, &c, "toString", key_to_string, 0)?;
        }
    }
    Ok(())
}

enum BinPart {
    Base64,
    Hex,
    Length,
    Subtype,
}

fn bin_part(it: &mut Interp, this: &Val, part: BinPart) -> R {
    let Bson::BinData(sub, bytes) = crate::bson::this_bson(it, this)? else {
        return Err(crate::builtins::incompatible(it, "BinData"));
    };
    match part {
        BinPart::Base64 => it.string(base64_encode(&bytes)),
        BinPart::Hex => it.string(crate::bson::hex(&bytes)),
        BinPart::Length => Ok(Val::Num(bytes.len() as f64)),
        BinPart::Subtype => Ok(Val::Num(f64::from(sub))),
    }
}

/// An object of one property, for `toJSON`.
fn single(it: &mut Interp, key: &str, v: Val) -> R {
    let o = it.object()?;
    it.define(&o, key, Prop::data(v))?;
    Ok(Val::Obj(o))
}

/// `toJSON` of the BSON types: their Extended JSON (`{"$oid": …}`, `{"$numberLong": …}`…).
fn bson_to_json(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(match crate::bson::this_bson(it, this)? {
        Bson::ObjectId(id) => single(it, "$oid", Val::Str(crate::bson::hex(&id).into()))?,
        Bson::Long(n) => single(it, "$numberLong", Val::Str(n.to_string().into()))?,
        Bson::Int(n) => Val::Num(f64::from(n)),
        Bson::Decimal(d) => single(it, "$numberDecimal", Val::Str(d.to_string().into()))?,
        Bson::Timestamp(t, i) => {
            let inner = it.object()?;
            it.define(&inner, "t", Prop::data(Val::Num(f64::from(t))))?;
            it.define(&inner, "i", Prop::data(Val::Num(f64::from(i))))?;
            single(it, "$timestamp", Val::Obj(inner))?
        }
        Bson::BinData(sub, bytes) => {
            let o = it.object()?;
            it.define(&o, "$binary", Prop::data(Val::Str(base64_encode(&bytes).into())))?;
            it.define(&o, "$type", Prop::data(Val::Str(format!("{sub:02x}").into())))?;
            Val::Obj(o)
        }
        _ => Val::Undef,
    })
}

fn key_to_json(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let is_max = matches!(this, Val::Obj(o)
        if matches!(o.borrow().get_own("name").map(|p| p.value.clone()), Some(Val::Str(n)) if &*n == "MaxKey"));
    single(it, if is_max { "$maxKey" } else { "$minKey" }, Val::Num(1.0))
}

/// `MinKey.toString()`: `[object MinKey]` (measured on a `refrouter` 9.0.4 on 02-11).
fn key_to_string(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let key = key_to_json(it, this, a)?;
    let name = if matches!(it.get(&key, "$maxKey")?, Val::Undef) { "MinKey" } else { "MaxKey" };
    it.string(format!("[object {name}]"))
}

fn regexp_to_json(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let source = it.get(this, "source")?;
    let flags = it.get(this, "flags")?;
    let o = it.object()?;
    it.define(&o, "$regex", Prop::data(source))?;
    it.define(&o, "$options", Prop::data(flags))?;
    Ok(Val::Obj(o))
}

/// The global through which `prelude.js` receives its native helpers; it is removed once the
/// prelude has run, so no program sees it.
const HOST: &str = "__teseladbShellHost";

/// Runs `prelude.js` (TeselaDB's own code for the shell's JavaScript globals) in the global scope.
fn run_prelude(it: &mut Interp) -> R<()> {
    let host = it.object()?;
    method(it, &host, "md5", units_md5, 1)?;
    let g = it.realm.global.clone();
    it.define(&g, HOST, Prop::hidden(Val::Obj(host)))?;
    let body = crate::parser::parse_script(include_str!("prelude.js"))
        .map_err(|e| it.error("SyntaxError", &e.message))?;
    let env = it.global_env.clone();
    it.hoist(&body, &env, true)?;
    let ran = body.iter().try_for_each(|s| it.exec(s, &env));
    g.borrow_mut().remove(HOST);
    ran
}

/// `md5(units)`: the hexadecimal MD5 of the UTF-8 of an array of code units (`hex_md5`).
fn units_md5(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let list = arg(a, 0);
    let len = it.get(&list, "length")?;
    let len = it.number_of(&len)? as usize;
    let mut units = Vec::with_capacity(len);
    for i in 0..len {
        let v = it.get(&list, &i.to_string())?;
        units.push(it.number_of(&v)?);
    }
    it.string(crate::md5::hex_digest(&crate::md5::encode(&units)))
}

fn key_tojson(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let name = match this {
        Val::Obj(o) => match o.borrow().get_own("name").map(|p| p.value.clone()) {
            Some(Val::Str(n)) => n,
            _ => "Object".into(),
        },
        _ => "Object".into(),
    };
    it.string(format!("[object {name}]"))
}

fn needs(it: &Interp, a: &[Val], n: usize, name: &str) -> R<()> {
    if a.len() < n {
        let what = if n == 1 { "argument" } else { "arguments" };
        return Err(it.error("Error", &format!("{name} needs {n} {what}")));
    }
    Ok(())
}

/// `{a: v}` as BSON, which is how the natives of the shell receive a value; `undefined` stays.
fn wrapped(it: &mut Interp, v: &Val) -> R<Document> {
    let x = match v {
        Val::Undef => Value::Undefined,
        v => to_bson(it, v, "a", 0)?,
    };
    let mut d = Document::new();
    d.push("a", x);
    Ok(d)
}

fn bytes_of(it: &Interp, d: &Document) -> R<Vec<u8>> {
    d.to_bytes().map_err(|e| it.error("Error", &e.to_string()))
}

fn wo_compare(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    needs(it, a, 2, "bsonWoCompare")?;
    let (x, y) = (wrapped(it, &a[0])?, wrapped(it, &a[1])?);
    Ok(Val::Num(wo_docs(it, &x, &y)? as f64))
}

fn unordered_compare(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    needs(it, a, 2, "bsonWoCompare")?;
    let (x, y) = (wrapped(it, &a[0])?, wrapped(it, &a[1])?);
    Ok(Val::Num(wo_docs(it, &sorted(&x), &sorted(&y))? as f64))
}

/// `d` with the fields of each document in it sorted by name.
fn sorted(d: &Document) -> Document {
    let mut fields: Vec<(String, Value)> = d
        .iter()
        .map(|(k, v)| {
            let v = match v {
                Value::Document(inner) => Value::Document(sorted(inner)),
                other => other.clone(),
            };
            (k.to_string(), v)
        })
        .collect();
    fields.sort_by(|p, q| p.0.cmp(&q.0));
    let mut out = Document::new();
    for (k, v) in fields {
        out.push(&k, v);
    }
    out
}

fn binary_equal(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    needs(it, a, 2, "bsonBinaryEqual")?;
    let (x, y) = (wrapped(it, &a[0])?, wrapped(it, &a[1])?);
    Ok(Val::Bool(bytes_of(it, &x)? == bytes_of(it, &y)?))
}

fn obj_to_array(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    needs(it, a, 1, "bsonObjToArray")?;
    let items = match to_bson(it, &a[0], "a", 0)? {
        Value::Document(d) => d.iter().map(|(_, v)| v.clone()).collect(),
        Value::Array(v) if matches!(a[0], Val::Obj(_)) => v,
        _ => return Err(it.error("Error", "argument must be an object")),
    };
    let mut out = Vec::with_capacity(items.len());
    for v in &items {
        out.push(crate::bson::from_bson(it, v, 1)?);
    }
    it.array(out)
}

fn to_base64(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    needs(it, a, 1, "bsonToBase64")?;
    let d = wrapped(it, &a[0])?;
    let bytes = bytes_of(it, &d)?;
    it.string(base64_encode(&bytes))
}

fn get_immutable(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    needs(it, a, 1, "bsonGetImmutable")?;
    let d = wrapped(it, &a[0])?;
    crate::bson::from_document(it, &d, 0)
}

/// RefDb's canonical order of BSON types, whose difference `woCompare` gives.
fn canonical(v: &Value) -> i64 {
    match v {
        Value::MinKey => -1,
        Value::Undefined => 0,
        Value::Null => 5,
        Value::Double(_) | Value::Int32(_) | Value::Int64(_) | Value::Decimal128(_) => 10,
        Value::String(_) | Value::Symbol(_) => 15,
        Value::Document(_) => 20,
        Value::Array(_) => 25,
        Value::Binary { .. } => 30,
        Value::ObjectId(_) => 35,
        Value::Boolean(_) => 40,
        Value::DateTime(_) => 45,
        Value::Timestamp { .. } => 47,
        Value::Regex { .. } => 50,
        Value::DbPointer { .. } => 55,
        Value::JavaScript(_) => 60,
        Value::JavaScriptWithScope { .. } => 65,
        Value::MaxKey => 127,
    }
}

/// `strcmp`: the difference of the first bytes that differ.
fn strcmp(a: &str, b: &str) -> i64 {
    let (x, y) = (a.as_bytes(), b.as_bytes());
    for i in 0..x.len().max(y.len()) {
        let (p, q) = (x.get(i).copied().unwrap_or(0), y.get(i).copied().unwrap_or(0));
        if p != q {
            return i64::from(p) - i64::from(q);
        }
    }
    0
}

/// `BSONObj::woCompare` with field names: the first difference of type, name or value.
fn wo_docs(it: &Interp, a: &Document, b: &Document) -> R<i64> {
    let (mut ia, mut ib) = (a.iter(), b.iter());
    loop {
        let ((ka, va), (kb, vb)) = match (ia.next(), ib.next()) {
            (None, None) => return Ok(0),
            (None, Some(_)) => return Ok(-1),
            (Some(_), None) => return Ok(1),
            (Some(x), Some(y)) => (x, y),
        };
        let mut x = canonical(va) - canonical(vb);
        if x == 0 {
            x = strcmp(ka, kb);
        }
        if x == 0 {
            x = wo_values(it, va, vb)?;
        }
        if x != 0 {
            return Ok(x);
        }
    }
}

fn as_document(items: &[Value]) -> Document {
    let mut d = Document::new();
    for (i, v) in items.iter().enumerate() {
        d.push(i.to_string(), v.clone());
    }
    d
}

fn wo_values(it: &Interp, a: &Value, b: &Value) -> R<i64> {
    Ok(match (a, b) {
        (Value::Document(x), Value::Document(y)) => wo_docs(it, x, y)?,
        (Value::Array(x), Value::Array(y)) => wo_docs(it, &as_document(x), &as_document(y))?,
        (Value::String(x) | Value::Symbol(x), Value::String(y) | Value::Symbol(y)) => strcmp(x, y),
        _ => {
            let mut x = Document::new();
            x.push("", a.clone());
            let mut y = Document::new();
            y.push("", b.clone());
            let (bx, by) = (bytes_of(it, &x)?, bytes_of(it, &y)?);
            let (Ok(rx), Ok(ry)) = (RawDoc::from_bytes(&bx), RawDoc::from_bytes(&by)) else {
                return Ok(0);
            };
            teseladb_bson::compare_docs(rx, ry, None) as i64
        }
    })
}

fn hex_bytes(it: &Interp, text: &str) -> R<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return Err(it.error("Error", "BinData hex string must be an even length"));
    }
    let digit = |c: u8| -> R<u8> {
        (c as char).to_digit(16).map(|d| d as u8).ok_or_else(|| {
            it.error("Error", &format!("The character \\x{c:02x} failed to parse from hex."))
        })
    };
    text.as_bytes().chunks(2).map(|p| Ok(digit(p[0])? << 4 | digit(p[1])?)).collect()
}

fn hex_data(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    needs(it, a, 2, "HexData")?;
    let sub = match &a[0] {
        Val::Num(n) if (0.0..=255.0).contains(n) => *n as u8,
        _ => {
            return Err(
                it.error("Error", "HexData subtype must be a Number between 0 and 255 inclusive")
            );
        }
    };
    let Val::Str(text) = &a[1] else {
        return Err(it.error("Error", "BinData data must be a String"));
    };
    let bytes = hex_bytes(it, text)?;
    make(it, Bson::BinData(sub, bytes))
}

fn md5(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let text = match arg(a, 0) {
        Val::Str(s) if s.len() == 32 => s,
        _ => return Err(it.error("Error", "MD5 string must have 32 characters")),
    };
    let bytes = hex_bytes(it, &text)?;
    make(it, Bson::BinData(SUBTYPE_MD5, bytes))
}

fn uuid(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let (sub, bytes) = match arg(a, 0) {
        Val::Undef => {
            let mut b = [0u8; 16];
            for chunk in b.chunks_mut(4) {
                let r = (crate::math::next_random(it) * f64::from(u32::MAX)) as u32;
                chunk.copy_from_slice(&r.to_be_bytes());
            }
            (SUBTYPE_UUID, b.to_vec())
        }
        Val::Str(s) if s.len() == 32 && s.bytes().all(|c| c.is_ascii_hexdigit()) => {
            (SUBTYPE_UUID_OLD, hex_bytes(it, &s)?)
        }
        Val::Str(s) if is_dashed_uuid(&s) => (SUBTYPE_UUID, hex_bytes(it, &s.replace('-', ""))?),
        v => {
            let shown = it.str_of(&v)?;
            return Err(it.error("Error", &format!("Invalid UUID string: {shown}")));
        }
    };
    make(it, Bson::BinData(sub, bytes))
}

fn is_dashed_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
        && groups.iter().all(|g| g.bytes().all(|c| c.is_ascii_hexdigit()))
}

/// `Code(code, scope)` and `DBPointer(ns, id)`.
fn code_and_pointer(it: &mut Interp, g: &ObjRef) -> R<()> {
    let proto = it.object()?;
    crate::builtins::ctor(it, g, "Code", code, 0, &proto)?;
    method(it, &proto, "toString", code_to_string, 0)?;
    let proto = it.object()?;
    let c = crate::builtins::ctor(it, g, "DBPointer", db_pointer, 0, &proto)?;
    it.realm.bson.push(("DBPointer".into(), c));
    // It reads `ns` and `id` of any object.
    let to_string: NativeFn = |it, this, _| {
        let ns = it.get(this, "ns")?;
        let ns = it.str_of(&ns)?;
        let id = it.get(this, "id")?;
        let id = it.str_of(&id)?;
        it.string(format!("DBPointer(\"{ns}\", {id})"))
    };
    method(it, &proto, "toString", to_string, 0)?;
    method(it, &proto, "tojson", to_string, 0)?;
    method(it, &proto, "toJSON", pointer_to_json, 0)?;
    let ns = it.native("ns", |it, this, _| pointer_part(it, this, true), 0)?;
    it.define(&proto, "ns", Prop { getter: Some(ns), ..Prop::hidden(Val::Undef) })?;
    let id = it.native("id", |it, this, _| pointer_part(it, this, false), 0)?;
    it.define(&proto, "id", Prop { getter: Some(id), ..Prop::hidden(Val::Undef) })?;
    Ok(())
}

/// `Code.prototype`, read from the global object.
fn code_proto(it: &Interp) -> Option<ObjRef> {
    let c = it.realm.global.borrow().get_own("Code")?.value.as_obj().cloned()?;
    c.borrow().get_own("prototype")?.value.as_obj().cloned()
}

fn code(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let proto = code_proto(it);
    let o = it.alloc(proto, Kind::Ordinary)?;
    let text = match arg(a, 0) {
        Val::Undef => Val::str(""),
        v => Val::Str(it.str_of(&v)?),
    };
    it.define(&o, "code", Prop::data(text))?;
    if a.len() > 1 {
        it.define(&o, "scope", Prop::data(a[1].clone()))?;
    }
    Ok(Val::Obj(o))
}

fn code_to_string(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let proto = code_proto(it);
    let is_code = matches!(this, Val::Obj(o)
        if o.borrow().proto.as_ref().zip(proto.as_ref()).is_some_and(|(p, q)| p.ptr_eq(q)));
    if !is_code {
        let kind = crate::builtins::class_name(this);
        return Err(
            it.error("Error", &format!("Cannot call \"toString\" on object of type \"{kind}\""))
        );
    }
    let text = it.get(this, "code")?;
    let text = it.str_of(&text)?;
    let scope = match it.get(this, "scope")? {
        Val::Undef | Val::Null => "{}".to_string(),
        v => match to_bson(it, &v, "scope", 0)? {
            Value::Document(d) => {
                let bytes = bytes_of(it, &d)?;
                RawDoc::from_bytes(&bytes).map(teseladb_bson::shell::shell_doc).unwrap_or_default()
            }
            _ => "{}".to_string(),
        },
    };
    it.string(format!("Code({{\"code\":{},\"scope\":{scope}\"}})", crate::json::quote(&text)))
}

fn db_pointer(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    needs(it, a, 2, "DBPointer")?;
    let ns = it.str_of(&a[0])?;
    let id = match &a[1] {
        Val::Obj(o) => match &o.borrow().kind {
            Kind::Bson(Bson::ObjectId(id)) => Some(*id),
            _ => None,
        },
        _ => None,
    };
    let Some(id) = id else {
        return Err(it.error("Error", "DBPointer 2nd parameter must be an ObjectId"));
    };
    make(it, Bson::DbPointer(crate::text::to_utf8(&ns).into_owned(), id))
}

fn pointer_part(it: &mut Interp, this: &Val, ns: bool) -> R {
    match crate::bson::this_bson(it, this)? {
        Bson::DbPointer(n, _) if ns => it.string(crate::text::encode(&n).into_owned()),
        Bson::DbPointer(_, id) => make(it, Bson::ObjectId(id)),
        _ => Err(crate::builtins::incompatible(it, "DBPointer")),
    }
}

fn pointer_to_json(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let o = it.object()?;
    let ns = pointer_part(it, this, true)?;
    let id = pointer_part(it, this, false)?;
    it.define(&o, "ns", Prop::data(ns))?;
    it.define(&o, "id", Prop::data(id))?;
    Ok(Val::Obj(o))
}
