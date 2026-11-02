//! BSON in JavaScript (016 FR-003): the types RefDb shows with its own constructors
//! (`ObjectId`, `NumberLong`, `NumberInt`, `NumberDecimal`, `ISODate`, `BinData`, `Timestamp`,
//! `MinKey`, `MaxKey`, `DBRef`, `DBPointer`) and the conversions both ways, as a `refrouter` 9.0.4
//! does them (measured on 02-11): every number comes back as a double except a `NumberInt`, an
//! `int32` argument is a plain number, `undefined` is `null`, a function is `Code` and a symbol
//! cannot be written.
//!
//! The prototypes hold, in the order the `refrouter` lists them, the natives of each type (not
//! enumerable); `prelude.js` then assigns the methods the shell writes in JavaScript (enumerable).
//! Each prototype is an object of its type: `NumberLong.prototype` is `NumberLong(0)`, and the
//! natives of `ObjectId` and `BinData` refuse theirs ("Cannot call … on prototype of …").

use teseladb_bson::{Decimal128, Document, Value};

use crate::interp::{Ctrl, Fatal, Interp, R};
use crate::value::*;

fn arg(args: &[Val], i: usize) -> Val {
    args.get(i).cloned().unwrap_or(Val::Undef)
}

/// The largest time and increment of a `Timestamp`.
const TIMESTAMP_MAX: f64 = 4_294_967_295.0;
/// 2⁶⁴, the modulus of the conversion of a number to a 64-bit integer.
const TWO_TO_64: f64 = 18_446_744_073_709_551_616.0;
/// The error code of a value BSON cannot hold (a symbol).
const NOT_CONVERTIBLE: i32 = 16_662;
/// `BadValue`: a malformed value of a BSON type.
const BAD_VALUE: i32 = 2;
/// A `Timestamp` without its `t` or its `i`.
const MISSING_TIME: i32 = 6_900_900;
const MISSING_INCREMENT: i32 = 6_900_901;

/// The types whose natives are shared, by their index in [`NATIVE_TYPES`].
const LONG: usize = 0;
const INT: usize = 1;
const DECIMAL: usize = 2;
const OBJECT_ID_T: usize = 3;
const BIN_DATA_T: usize = 4;
const NATIVE_TYPES: [&str; 5] = ["NumberLong", "NumberInt", "NumberDecimal", "ObjectId", "BinData"];

/// A native of a prototype: its name, its function and its `length`.
type Native = (&'static str, NativeFn, usize);

/// A BSON type with a constructor: its name, the constructor, the value its prototype holds,
/// the natives and the getters (enumerable, as the `refrouter` lists them) of the prototype.
struct TypeSpec {
    name: &'static str,
    ctor: NativeFn,
    proto: fn() -> Bson,
    natives: &'static [Native],
    getters: &'static [(&'static str, NativeFn)],
}

const NUMBER_LONG: TypeSpec = TypeSpec {
    name: "NumberLong",
    ctor: number_long,
    proto: || Bson::Long(0),
    natives: &[
        ("toNumber", value_of::<LONG>, 0),
        ("toString", to_string::<LONG>, 0),
        ("toJSON", to_json::<LONG>, 0),
        ("valueOf", value_of::<LONG>, 0),
        ("compare", compare, 0),
    ],
    getters: &[
        ("floatApprox", float_approx),
        ("top", top),
        ("bottom", bottom),
        ("exactValueString", exact_value_string),
    ],
};

const NUMBER_INT: TypeSpec = TypeSpec {
    name: "NumberInt",
    ctor: number_int,
    proto: || Bson::Int(0),
    natives: &[
        ("toNumber", value_of::<INT>, 0),
        ("toString", to_string::<INT>, 0),
        ("toJSON", to_json::<INT>, 0),
        ("valueOf", value_of::<INT>, 0),
    ],
    getters: &[],
};

const NUMBER_DECIMAL: TypeSpec = TypeSpec {
    name: "NumberDecimal",
    ctor: number_decimal,
    proto: || Bson::Decimal(Decimal128::from_le_bytes(DECIMAL_ZERO.to_le_bytes())),
    natives: &[("toString", to_string::<DECIMAL>, 0), ("toJSON", to_json::<DECIMAL>, 0)],
    getters: &[],
};

/// The bits of the decimal `0`.
const DECIMAL_ZERO: u128 = 0x3040u128 << 112;

const OBJECT_ID: TypeSpec = TypeSpec {
    name: "ObjectId",
    ctor: object_id,
    proto: || Bson::ObjectId([0; 12]),
    natives: &[("toString", to_string::<OBJECT_ID_T>, 0), ("toJSON", to_json::<OBJECT_ID_T>, 0)],
    getters: &[("str", object_id_hex)],
};

const BIN_DATA: TypeSpec = TypeSpec {
    name: "BinData",
    ctor: bin_data,
    proto: || Bson::BinData(0, Vec::new()),
    natives: &[
        ("base64", bin_base64, 0),
        ("hex", bin_hex, 0),
        ("toString", to_string::<BIN_DATA_T>, 0),
        ("toJSON", to_json::<BIN_DATA_T>, 0),
    ],
    getters: &[],
};

const TIMESTAMP: TypeSpec = TypeSpec {
    name: "Timestamp",
    ctor: timestamp,
    proto: || Bson::Timestamp(0, 0),
    natives: &[("toJSON", timestamp_to_json, 0)],
    getters: &[],
};

const DB_REF: TypeSpec =
    TypeSpec { name: "DBRef", ctor: db_ref, proto: || Bson::DbRef, natives: &[], getters: &[] };

const DB_POINTER: TypeSpec = TypeSpec {
    name: "DBPointer",
    ctor: db_pointer,
    proto: || Bson::DbPointer(String::new(), [0; 12]),
    natives: &[],
    getters: &[],
};

pub(crate) fn type_name(b: &Bson) -> &'static str {
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
        Bson::DbRef => "DBRef",
        Bson::Symbol(_) => "Symbol",
        Bson::Code(_) => "Code",
    }
}

/// The global constructor (for `MinKey` and `MaxKey`, the key itself) of the type `name`.
fn ctor_of(it: &Interp, name: &str) -> Option<ObjRef> {
    it.realm.bson.iter().find(|(n, _)| &**n == name).map(|(_, c)| c.clone())
}

pub(crate) fn proto_of(it: &Interp, name: &str) -> Option<ObjRef> {
    let c = ctor_of(it, name)?;
    c.borrow().get_own("prototype")?.value.as_obj().cloned()
}

/// The class the `refrouter` gives a BSON object: its type, and `MinKey` or `MaxKey` for the key
/// and the prototype of the key.
pub(crate) fn bson_class(it: &Interp, o: &ObjRef) -> Option<&'static str> {
    if let Kind::Bson(b) = &o.borrow().kind {
        return Some(type_name(b));
    }
    ["MinKey", "MaxKey"].into_iter().find(|name| is_key(it, o, name))
}

/// Whether `o` is the key `name` (`MinKey`, `MaxKey`) or its prototype.
fn is_key(it: &Interp, o: &ObjRef, name: &str) -> bool {
    ctor_of(it, name)
        .is_some_and(|k| k.ptr_eq(o) || k.borrow().proto.as_ref().is_some_and(|p| p.ptr_eq(o)))
}

pub fn make(it: &Interp, b: Bson) -> R {
    if let Bson::MinKey | Bson::MaxKey = b {
        return ctor_of(it, type_name(&b)).map(Val::Obj).ok_or(Ctrl::Fatal(Fatal::Unreachable));
    }
    let proto = proto_of(it, type_name(&b)).or_else(|| Some(it.realm.object.clone()));
    let o = it.alloc(proto, Kind::Bson(b.clone()))?;
    // Measured on a `refrouter` 9.0.4 on 02-11: the own properties of each type.
    let fixed = |v: Val| Prop { writable: false, ..Prop::hidden(v) };
    match b {
        Bson::Timestamp(t, i) => {
            it.define(&o, "t", Prop::data(Val::Num(f64::from(t))))?;
            it.define(&o, "i", Prop::data(Val::Num(f64::from(i))))?;
        }
        Bson::BinData(sub, bytes) => {
            it.define(&o, "len", fixed(Val::Num(bytes.len() as f64)))?;
            it.define(&o, "type", fixed(Val::Num(f64::from(sub))))?;
        }
        Bson::DbPointer(ns, id) => {
            let ns = it.string(crate::text::encode(&ns).into_owned())?;
            it.define(&o, "ns", fixed(ns))?;
            let id = make(it, Bson::ObjectId(id))?;
            it.define(&o, "id", fixed(id))?;
        }
        _ => {}
    }
    Ok(Val::Obj(o))
}

/// The value of `this` for the native `method` of the type `ty`, with the errors of the
/// `refrouter` 9.0.4 (measured on 02-11).
pub(crate) fn this_of(it: &Interp, this: &Val, method: &str, ty: &str) -> R<Bson> {
    let o = match this {
        Val::Obj(o) => o,
        Val::Sym(_) => return Err(it.error("Error", "unable to get type")),
        v => {
            let t = if matches!(v, Val::Null) { "null" } else { v.type_of() };
            let m = format!("Cannot call \"{method}\" on non-object of type \"{t}\"");
            return Err(it.error("Error", &m));
        }
    };
    let class = if crate::proxy::is_proxy(o) {
        "Proxy"
    } else {
        bson_class(it, o).unwrap_or_else(|| crate::builtins::class_kind(this))
    };
    if class != ty {
        let m = format!("Cannot call \"{method}\" on object of type \"{class}\"");
        return Err(it.error("Error", &m));
    }
    if matches!(ty, "ObjectId" | "BinData") && proto_of(it, ty).is_some_and(|p| p.ptr_eq(o)) {
        let m = format!("Cannot call \"{method}\" on prototype of \"{ty}\"");
        return Err(it.error("Error", &m));
    }
    Ok(match &o.borrow().kind {
        Kind::Bson(b) => b.clone(),
        _ if ty == "MaxKey" => Bson::MaxKey,
        _ => Bson::MinKey,
    })
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn install(it: &mut Interp, g: &ObjRef) -> R<()> {
    for spec in [NUMBER_LONG, NUMBER_INT, NUMBER_DECIMAL, OBJECT_ID, BIN_DATA, TIMESTAMP] {
        install_type(it, g, &spec)?;
    }
    install_key(
        it,
        g,
        "MinKey",
        [key_call::<false>, key_text::<false, true>, key_text::<false, false>],
    )?;
    install_key(
        it,
        g,
        "MaxKey",
        [key_call::<true>, key_text::<true, true>, key_text::<true, false>],
    )?;
    install_type(it, g, &DB_REF)
}

/// `DBPointer`, which the shell defines after `Code`.
pub(crate) fn install_pointer(it: &mut Interp, g: &ObjRef) -> R<()> {
    install_type(it, g, &DB_POINTER)
}

fn install_type(it: &mut Interp, g: &ObjRef, spec: &TypeSpec) -> R<()> {
    let proto = it.alloc(Some(it.realm.object.clone()), Kind::Bson((spec.proto)()))?;
    let c = it.native(spec.name, spec.ctor, 0)?;
    // The `refrouter` lists `prototype` before `length` and `name`.
    let fixed =
        Prop { writable: false, configurable: false, ..Prop::hidden(Val::Obj(proto.clone())) };
    c.borrow_mut().props.insert(0, ("prototype".into(), fixed));
    it.define(&proto, "constructor", Prop::hidden(Val::Obj(c.clone())))?;
    it.define(g, spec.name, Prop::hidden(Val::Obj(c.clone())))?;
    for &(name, f, n) in spec.natives {
        crate::builtins::method(it, &proto, name, f, n)?;
    }
    for &(name, f) in spec.getters {
        let getter = it.native(&format!("get {name}"), f, 0)?;
        it.define(&proto, name, Prop { getter: Some(getter), ..Prop::data(Val::Undef) })?;
    }
    it.realm.bson.push((spec.name.into(), c));
    Ok(())
}

/// `MinKey` and `MaxKey` (measured on a `refrouter` 9.0.4 on 02-11): a frozen callable object
/// without properties whose call gives itself, over a callable prototype (not frozen) with
/// `constructor`, `tojson`, `toJSON` and `singleton`, over `Object.prototype`.
fn install_key(
    it: &mut Interp,
    g: &ObjRef,
    name: &str,
    [call, tojson, to_json]: [NativeFn; 3],
) -> R<()> {
    let proto =
        it.alloc(Some(it.realm.object.clone()), Kind::Func(Func::Native(call, name.into())))?;
    let key = it.alloc(Some(proto.clone()), Kind::Func(Func::Native(call, name.into())))?;
    let ctor = it.native(name, call, 0)?;
    ctor.borrow_mut().props.insert(0, ("prototype".into(), Prop::hidden(Val::Obj(proto.clone()))));
    it.define(&proto, "constructor", Prop::hidden(Val::Obj(ctor)))?;
    crate::builtins::method(it, &proto, "tojson", tojson, 0)?;
    crate::builtins::method(it, &proto, "toJSON", to_json, 0)?;
    it.define(&proto, "singleton", Prop::data(Val::Obj(key.clone())))?;
    crate::props::lock(&key, true);
    it.define(g, name, Prop::hidden(Val::Obj(key.clone())))?;
    it.realm.bson.push((name.into(), key));
    Ok(())
}

/// The key a call of `MinKey` (`MaxKey` with `MAX`), of its prototype or of its constructor
/// gives: the key itself.
fn key_call<const MAX: bool>(it: &mut Interp, _: &Val, _: &[Val]) -> R {
    make(it, if MAX { Bson::MaxKey } else { Bson::MinKey })
}

/// `tojson` (`{ "$minKey" : 1 }`, `TEXT`) and `toJSON` (`{$minKey: 1}`) of `MinKey` and, with
/// `MAX`, of `MaxKey`.
fn key_text<const MAX: bool, const TEXT: bool>(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let (ty, key) = if MAX { ("MaxKey", "$maxKey") } else { ("MinKey", "$minKey") };
    this_of(it, this, if TEXT { "tojson" } else { "toJSON" }, ty)?;
    if TEXT {
        return it.string(format!("{{ \"{key}\" : 1 }}"));
    }
    single(it, key, Val::Num(1.0))
}

/// A number made a 64-bit integer as the `refrouter` does: truncated, modulo 2⁶⁴, `NaN` is 0.
fn to_int64(n: f64) -> i64 {
    if !n.is_finite() {
        return 0;
    }
    let m = n.trunc() % TWO_TO_64;
    let m = if m < 0.0 { m + TWO_TO_64 } else { m };
    m as u64 as i64
}

/// The time (`increment` false) or the increment of a `Timestamp` from `v`, with the
/// `refrouter`'s message when it is not a number in `0..=4294967295`.
fn timestamp_part(v: &Val, increment: bool) -> Result<u32, String> {
    let what = if increment { "Timestamp increment" } else { "Timestamp time (seconds)" };
    let Val::Num(n) = v else {
        return Err(format!("{what} must be a number"));
    };
    let n = to_int64(*n);
    if !(0..=TIMESTAMP_MAX as i64).contains(&n) {
        return Err(format!(
            "{what} must be non-negative and not greater than {TIMESTAMP_MAX}, got {n}"
        ));
    }
    Ok(n as u32)
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
        // Measured on a `refrouter` 9.0.4 on 02-11: the length is checked before the digits.
        Val::Str(s) if s.len() != 24 => return Err(it.error("Error", "invalid object id: length")),
        Val::Str(_) => return Err(it.error("Error", "invalid object id: not hex")),
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

/// `Timestamp(t, i)` (measured on a `refrouter` 9.0.4 on 02-11): none or both, numbers truncated
/// to `0..=4294967295`.
fn timestamp(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let (t, i) = match a {
        [] => (0, 0),
        [t, i] => {
            let t = timestamp_part(t, false).map_err(|m| it.error("Error", &m))?;
            let i = timestamp_part(i, true).map_err(|m| it.error("Error", &m))?;
            (t, i)
        }
        _ => return Err(it.error("Error", "Timestamp needs 0 or 2 arguments")),
    };
    make(it, Bson::Timestamp(t, i))
}

/// `DBRef(collection, id, db)`: an object with `$ref`, `$id` and `$db` (measured on a `refrouter`
/// 9.0.4 on 02-11: two or three arguments, the first and the third strings).
fn db_ref(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    if !(2..=3).contains(&a.len()) {
        return Err(it.error("Error", "DBRef needs 2 or 3 arguments"));
    }
    if !matches!(a[0], Val::Str(_)) {
        return Err(it.error("Error", "DBRef 1st parameter must be a string"));
    }
    if a.len() == 3 && !matches!(a[2], Val::Str(_)) {
        return Err(it.error("Error", "DBRef 3rd parameter must be a string"));
    }
    let o = it.alloc(proto_of(it, "DBRef"), Kind::Bson(Bson::DbRef))?;
    it.define(&o, "$ref", Prop::data(a[0].clone()))?;
    it.define(&o, "$id", Prop::data(a[1].clone()))?;
    if let Some(db) = a.get(2) {
        it.define(&o, "$db", Prop::data(db.clone()))?;
    }
    Ok(Val::Obj(o))
}

/// `DBPointer(ns, id)` (measured on a `refrouter` 9.0.4 on 02-11): exactly two arguments, a string
/// and an `ObjectId`.
fn db_pointer(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let [ns, id] = a else {
        return Err(it.error("Error", "DBPointer needs 2 arguments"));
    };
    let Val::Str(ns) = ns else {
        return Err(it.error("Error", "DBPointer 1st parameter must be a string"));
    };
    let id = match id {
        Val::Obj(o) => match &o.borrow().kind {
            Kind::Bson(Bson::ObjectId(id)) => Some(*id),
            _ => None,
        },
        _ => None,
    };
    let Some(id) = id else {
        return Err(it.error("Error", "DBPointer 2nd parameter must be an ObjectId"));
    };
    make(it, Bson::DbPointer(crate::text::to_utf8(ns).into_owned(), id))
}

/// `toString` of the type `T`.
fn to_string<const T: usize>(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let s = match this_of(it, this, "toString", NATIVE_TYPES[T])? {
        Bson::Long(n) => format!("NumberLong({n})"),
        Bson::Int(n) => format!("NumberInt({n})"),
        Bson::Decimal(d) => format!("NumberDecimal(\"{d}\")"),
        Bson::ObjectId(id) => format!("ObjectId(\"{}\")", hex(&id)),
        Bson::BinData(4, b) if b.len() == 16 => {
            let h = hex(&b);
            format!(
                "UUID(\"{}-{}-{}-{}-{}\")",
                &h[..8],
                &h[8..12],
                &h[12..16],
                &h[16..20],
                &h[20..]
            )
        }
        other => {
            format!("BinData({},\"{}\")", binary_subtype(&other), base64_encode(&binary(other)))
        }
    };
    it.string(s)
}

/// The subtype and the bytes of a `BinData` (the only type left when the others are matched).
fn binary_subtype(b: &Bson) -> u8 {
    if let Bson::BinData(t, _) = b { *t } else { 0 }
}

fn binary(b: Bson) -> Vec<u8> {
    if let Bson::BinData(_, bytes) = b { bytes } else { Vec::new() }
}

/// `toJSON` of the numbers, `ObjectId` and `BinData`: their Extended JSON.
fn to_json<const T: usize>(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(match this_of(it, this, "toJSON", NATIVE_TYPES[T])? {
        Bson::ObjectId(id) => single(it, "$oid", Val::Str(hex(&id).into()))?,
        Bson::Long(n) => single(it, "$numberLong", Val::Str(n.to_string().into()))?,
        Bson::Int(n) => Val::Num(f64::from(n)),
        Bson::Decimal(d) => single(it, "$numberDecimal", Val::Str(d.to_string().into()))?,
        other => {
            let o = it.object()?;
            let sub = binary_subtype(&other);
            it.define(&o, "$binary", Prop::data(Val::Str(base64_encode(&binary(other)).into())))?;
            it.define(&o, "$type", Prop::data(Val::Str(format!("{sub:02x}").into())))?;
            Val::Obj(o)
        }
    })
}

/// `Timestamp.prototype.toJSON`: `{$timestamp: {t, i}}` from the properties `t` and `i`.
fn timestamp_to_json(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    this_of(it, this, "toJSON", "Timestamp")?;
    let inner = it.object()?;
    for key in ["t", "i"] {
        let v = match it.get(this, key)? {
            Val::Undef => Val::Null,
            v => v,
        };
        it.define(&inner, key, Prop::data(v))?;
    }
    single(it, "$timestamp", Val::Obj(inner))
}

/// An object of one property, for `toJSON`.
fn single(it: &mut Interp, key: &str, v: Val) -> R {
    let o = it.object()?;
    it.define(&o, key, Prop::data(v))?;
    Ok(Val::Obj(o))
}

/// `valueOf` and `toNumber` of `NumberLong` and `NumberInt`.
fn value_of<const T: usize>(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let method = if it.native.as_ref() == "toNumber" { "toNumber" } else { "valueOf" };
    Ok(Val::Num(match this_of(it, this, method, NATIVE_TYPES[T])? {
        Bson::Long(n) => n as f64,
        b => f64::from(if let Bson::Int(n) = b { n } else { 0 }),
    }))
}

/// The `NumberLong` of `this` for the native `method`.
fn this_long(it: &Interp, this: &Val, method: &str) -> R<i64> {
    match this_of(it, this, method, "NumberLong")? {
        Bson::Long(n) => Ok(n),
        _ => Err(Ctrl::Fatal(Fatal::Unreachable)),
    }
}

/// `NumberLong.prototype.compare(other)`: -1, 0 or 1.
fn compare(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let n = this_long(it, this, "compare")?;
    let Some(other) = a.first() else {
        return Err(it.error("Error", "NumberLong.compare() needs 1 argument"));
    };
    let m = match other {
        Val::Obj(o) => match &o.borrow().kind {
            Kind::Bson(Bson::Long(m)) => Some(*m),
            _ => None,
        },
        _ => None,
    };
    let Some(m) = m else {
        return Err(it.error("Error", "NumberLong.compare() argument must be a NumberLong"));
    };
    Ok(Val::Num(match n.cmp(&m) {
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Equal => 0.0,
        std::cmp::Ordering::Greater => 1.0,
    }))
}

fn float_approx(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(Val::Num(this_long(it, this, "floatApprox")? as f64))
}

/// The high 32 bits of the `NumberLong`.
fn top(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(Val::Num(f64::from((this_long(it, this, "top")? as u64 >> 32) as u32)))
}

/// The low 32 bits of the `NumberLong`.
fn bottom(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(Val::Num(f64::from(this_long(it, this, "bottom")? as u64 as u32)))
}

fn exact_value_string(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let n = this_long(it, this, "exactValueString")?;
    it.string(n.to_string())
}

fn object_id_hex(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    match this_of(it, this, "getter", "ObjectId")? {
        Bson::ObjectId(id) => it.string(hex(&id)),
        _ => Err(Ctrl::Fatal(Fatal::Unreachable)),
    }
}

/// The bytes of the `BinData` of `this` for the native `method`.
fn this_bytes(it: &Interp, this: &Val, method: &str) -> R<Vec<u8>> {
    match this_of(it, this, method, "BinData")? {
        Bson::BinData(_, bytes) => Ok(bytes),
        _ => Err(Ctrl::Fatal(Fatal::Unreachable)),
    }
}

fn bin_base64(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let bytes = this_bytes(it, this, "base64")?;
    it.string(base64_encode(&bytes))
}

fn bin_hex(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let bytes = this_bytes(it, this, "hex")?;
    it.string(hex(&bytes))
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64_encode(b: &[u8]) -> String {
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
        Value::String(s) => it.string(crate::text::encode(s).into_owned())?,
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
            crate::builtins::make_regexp(it, crate::text::encode(pattern).into(), flags.into())?
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
    let Ok(body) = crate::parser::parse_script(&crate::text::encode(code)) else {
        return Ok(None);
    };
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
        o.borrow_mut().props.push((crate::text::encode(k).into(), Prop::data(v)));
    }
    Ok(Val::Obj(o))
}

/// The error of a value that has no BSON form (measured: 16662 for a symbol).
pub fn not_convertible(name: &str) -> Ctrl {
    convert_error(
        NOT_CONVERTIBLE,
        format!("unable to convert JavaScript property to ref element {name}"),
    )
}

/// A value BSON cannot hold: RefDb's code and message.
fn convert_error(code: i32, message: String) -> Ctrl {
    Ctrl::Fatal(Fatal::Convert(code, message))
}

/// The BSON of a `Timestamp` object: its properties `t` and `i` (measured on a `refrouter` 9.0.4
/// on 02-11).
fn timestamp_bson(it: &mut Interp, v: &Val) -> R<Value> {
    let mut part = |key: &str, increment: bool| -> R<u32> {
        let has = matches!(v, Val::Obj(o) if o.borrow().get_own(key).is_some());
        if !has {
            let (code, what) = if increment {
                (MISSING_INCREMENT, "increment field, 'i'")
            } else {
                (MISSING_TIME, "timestamp field, 't'")
            };
            return Err(convert_error(
                code,
                format!("Malformed timestamp in JavaScript: missing {what}"),
            ));
        }
        let x = it.get(v, key)?;
        timestamp_part(&x, increment).map_err(|m| convert_error(BAD_VALUE, m))
    };
    let time = part("t", false)?;
    let increment = part("i", true)?;
    Ok(Value::Timestamp { time, increment })
}

/// A `DBPointer` as the `refrouter` writes it: a reference `{$ref, $id, $db}`, the namespace split
/// at its dot when it has one only.
fn pointer_bson(namespace: &str, id: [u8; 12]) -> Value {
    let mut d = Document::new();
    let (db, coll) = match namespace.split_once('.') {
        Some((db, coll)) if !coll.contains('.') => (db, coll),
        _ => ("", namespace),
    };
    d.push("$ref", Value::String(coll.into()));
    d.push("$id", Value::ObjectId(id));
    if !db.is_empty() {
        d.push("$db", Value::String(db.into()));
    }
    Value::Document(d)
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
                Kind::Bson(b) => bson_value(it, o, v, b, depth)?,
                Kind::Date(t) => Value::DateTime(if t.is_nan() { i64::MIN } else { t as i64 }),
                Kind::Num(n) => Value::Double(n),
                Kind::Str(s) => Value::String(crate::text::to_utf8(&s).into_owned()),
                Kind::Bool(b) => Value::Boolean(b),
                Kind::RegExp(p, f) => Value::Regex {
                    pattern: crate::text::to_utf8(&p).into_owned(),
                    options: f.chars().filter(|c| *c != 'g').collect(),
                },
                Kind::Func(Func::Native(..)) if bson_class(it, o).is_some() => {
                    if bson_class(it, o) == Some("MinKey") { Value::MinKey } else { Value::MaxKey }
                }
                Kind::Func(Func::Closure { def, .. }) => {
                    Value::JavaScript(crate::text::to_utf8(&def.source).into_owned())
                }
                Kind::Func(_) => Value::JavaScript("function() { [native code] }".into()),
                Kind::Array(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for (i, x) in items.iter().enumerate() {
                        out.push(to_bson(it, x, &i.to_string(), depth + 1)?);
                    }
                    Value::Array(out)
                }
                _ => document_bson(it, o, v, depth)?,
            }
        }
    })
}

/// An object of a BSON type as BSON (measured on a `refrouter` 9.0.4 on 02-11: the prototypes of
/// `ObjectId`, `BinData` and `DBPointer` cannot be written; a `Timestamp` is its `t` and `i`; a
/// `DBRef` is its properties).
fn bson_value(it: &mut Interp, o: &ObjRef, v: &Val, b: Bson, depth: usize) -> R<Value> {
    let is_proto = |ty: &str| proto_of(it, ty).is_some_and(|p| p.ptr_eq(o));
    let refused = match &b {
        Bson::ObjectId(_) if is_proto("ObjectId") => "Can't call getOID on OID prototype",
        Bson::BinData(..) if is_proto("BinData") => "Cannot call getter on BinData prototype",
        Bson::DbPointer(..) if is_proto("DBPointer") => "can't serialize DBPointer prototype",
        _ => "",
    };
    if !refused.is_empty() {
        return Err(convert_error(BAD_VALUE, refused.into()));
    }
    Ok(match b {
        Bson::Long(n) => Value::Int64(n),
        Bson::Int(n) => Value::Int32(n),
        Bson::Decimal(d) => Value::Decimal128(d),
        Bson::ObjectId(id) => Value::ObjectId(id),
        Bson::BinData(subtype, bytes) => Value::Binary { subtype, bytes },
        Bson::Timestamp(..) => timestamp_bson(it, v)?,
        Bson::MinKey => Value::MinKey,
        Bson::MaxKey => Value::MaxKey,
        Bson::DbPointer(namespace, id) => pointer_bson(&namespace, id),
        Bson::DbRef => document_bson(it, o, v, depth)?,
        Bson::Symbol(s) => Value::Symbol(s),
        Bson::Code(c) => Value::JavaScript(c),
    })
}

/// An object as a document of its enumerable properties (an error's methods left out).
fn document_bson(it: &mut Interp, o: &ObjRef, v: &Val, depth: usize) -> R<Value> {
    let mut d = Document::new();
    for k in it.keys(o, true)? {
        let x = it.get(v, &k)?;
        if x.is_callable() && matches!(o.borrow().kind, Kind::Error) {
            continue;
        }
        d.push(crate::text::to_utf8(&k).as_ref(), to_bson(it, &x, &k, depth + 1)?);
    }
    Ok(Value::Document(d))
}
