//! The global objects (ECMA-262 §19 to §24) the engine of RefDb 9.0.4 has, measured on a
//! `refrouter` 9.0.4 on 02-11: `Object`, `Function`, `Array`, `String`, `Number`, `Boolean`,
//! `Symbol`, `Math`, `JSON`, `Date`, `RegExp`, `Map`, `Set`, the errors, and RefDb's `print`,
//! `sleep` and `Array.sum`. `Reflect` is not there.

use std::rc::Rc;

use crate::interp::{Ctrl, Interp, R, Realm};
use crate::number;
use crate::value::*;

pub(crate) fn arg(args: &[Val], i: usize) -> Val {
    args.get(i).cloned().unwrap_or(Val::Undef)
}

/// `this` of a native constructor called with `new`: the new target.
pub(crate) fn new_target(this: &Val) -> Option<Val> {
    let Val::Obj(o) = this else { return None };
    o.borrow().get_own("@@new").map(|p| p.value.clone())
}

/// How SpiderMonkey names the type of `this` in «… method called on incompatible X»: an
/// object's class, a primitive's type.
pub(crate) fn class_name(v: &Val) -> &'static str {
    match v {
        Val::Obj(o) => match &o.borrow().kind {
            Kind::Array(_) => "Array",
            Kind::Func(_) => "Function",
            _ => "Object",
        },
        other => other.type_of(),
    }
}

/// The key of the well-known symbol `Symbol.<name>`.
pub(crate) fn well_known(name: &str) -> String {
    format!("@@Symbol.{name}:0")
}

/// `proto[Symbol.toStringTag] = tag`, read by `Object.prototype.toString`.
pub(crate) fn to_string_tag(it: &Interp, proto: &ObjRef, tag: &str) -> R<()> {
    it.define(
        proto,
        &well_known("toStringTag"),
        Prop { writable: false, ..Prop::hidden(Val::str(tag)) },
    )
}

pub(crate) fn method(it: &Interp, o: &ObjRef, name: &str, f: NativeFn, arity: usize) -> R<()> {
    let fo = it.native(name, f, arity)?;
    it.define(o, name, Prop::hidden(Val::Obj(fo)))
}

pub(crate) fn ctor(
    it: &Interp,
    global: &ObjRef,
    name: &str,
    f: NativeFn,
    arity: usize,
    proto: &ObjRef,
) -> R<ObjRef> {
    let c = it.native(name, f, arity)?;
    let fixed =
        Prop { writable: false, configurable: false, ..Prop::hidden(Val::Obj(proto.clone())) };
    it.define(&c, "prototype", fixed)?;
    it.define(proto, "constructor", Prop::hidden(Val::Obj(c.clone())))?;
    it.define(global, name, Prop::hidden(Val::Obj(c.clone())))?;
    Ok(c)
}

fn bare(budget: &Rc<Budget>, proto: Option<ObjRef>, kind: Kind) -> ObjRef {
    ObjRef(Rc::new(ObjCell {
        obj: std::cell::RefCell::new(Obj {
            proto,
            props: Vec::new(),
            kind,
            extensible: true,
            locks: 0,
        }),
        charged: std::cell::Cell::new(0),
        budget: budget.clone(),
        shared: std::cell::Cell::new(false),
    }))
}

/// The prototypes, before the globals are filled in.
pub fn realm(budget: &Rc<Budget>) -> Realm {
    let object = bare(budget, None, Kind::Ordinary);
    let p = |kind: Kind| bare(budget, Some(object.clone()), kind);
    let function = p(Kind::Func(Func::Native(|_, _, _| Ok(Val::Undef), "".into())));
    Realm {
        global: p(Kind::Ordinary),
        array: p(Kind::Array(Vec::new())),
        string: p(Kind::Str("".into())),
        number: p(Kind::Num(0.0)),
        boolean: p(Kind::Bool(false)),
        symbol: p(Kind::Ordinary),
        bigint: p(Kind::Ordinary),
        error: p(Kind::Ordinary),
        date: p(Kind::Ordinary),
        regexp: p(Kind::Ordinary),
        map: p(Kind::Ordinary),
        set: p(Kind::Ordinary),
        iter: p(Kind::Ordinary),
        errors: Vec::new(),
        bson: Vec::new(),
        protos: Vec::new(),
        function,
        object,
    }
}

/// Fills in the global object.
pub fn install(it: &mut Interp) -> R<()> {
    let g = it.realm.global.clone();
    it.define(&g, "globalThis", Prop::hidden(Val::Obj(g.clone())))?;
    it.define(
        &g,
        "NaN",
        Prop { writable: false, configurable: false, ..Prop::hidden(Val::Num(f64::NAN)) },
    )?;
    it.define(
        &g,
        "Infinity",
        Prop { writable: false, configurable: false, ..Prop::hidden(Val::Num(f64::INFINITY)) },
    )?;
    for (name, f, n) in [
        ("parseInt", parse_int as NativeFn, 2),
        ("parseFloat", parse_float, 1),
        ("isNaN", is_nan, 1),
        ("isFinite", is_finite, 1),
        ("print", print, 0),
        ("sleep", sleep, 1),
        ("eval", eval, 1),
    ] {
        method(it, &g, name, f, n)?;
    }
    object_builtins(it, &g)?;
    function_builtins(it, &g)?;
    array_builtins(it, &g)?;
    crate::strings::install(it, &g)?;
    number_builtins(it, &g)?;
    errors(it, &g)?;
    crate::math::install(it, &g)?;
    crate::json::install(it, &g)?;
    crate::date::install(it, &g)?;
    collections(it, &g)?;
    symbol_builtins(it, &g)?;
    crate::bigints::install(it, &g)?;
    crate::weak::install(it, &g)?;
    crate::promise::install(it, &g)?;
    crate::proxy::install(it, &g)?;
    crate::typed::install(it, &g)?;
    crate::iterators::install(it, &g)?;
    regexp_builtins(it, &g)?;
    crate::bson::install(it, &g)?;
    freeze_builtins(it);
    Ok(())
}

/// Freezes the objects of the globals, as RefDb's engine does (measured on a `refrouter` 9.0.4
/// on 02-11: `Object.isFrozen(Array.prototype)` is `true`, a new property of a prototype is not
/// kept, and strict code that assigns `name` on an error throws `"name" is read-only`).
fn freeze_builtins(it: &Interp) {
    fn freeze(o: &ObjRef) {
        crate::props::lock(o, true);
    }
    let mut all: Vec<ObjRef> = vec![
        it.realm.object.clone(),
        it.realm.function.clone(),
        it.realm.array.clone(),
        it.realm.string.clone(),
        it.realm.number.clone(),
        it.realm.boolean.clone(),
        it.realm.symbol.clone(),
        it.realm.bigint.clone(),
        it.realm.error.clone(),
        it.realm.date.clone(),
        it.realm.regexp.clone(),
        it.realm.map.clone(),
        it.realm.set.clone(),
        it.realm.iter.clone(),
    ];
    let globals: Vec<Val> =
        it.realm.global.borrow().props.iter().map(|(_, p)| p.value.clone()).collect();
    for v in globals {
        let Val::Obj(o) = v else { continue };
        if o.ptr_eq(&it.realm.global) {
            continue;
        }
        let proto = o.borrow().get_own("prototype").and_then(|p| p.value.as_obj().cloned());
        all.extend(proto);
        all.push(o);
    }
    for o in &all {
        freeze(o);
    }
}

// Globals.

fn parse_int(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let s = it.str_of(&arg(a, 0))?;
    let r = if matches!(arg(a, 1), Val::Undef) {
        0
    } else {
        number::to_int32(it.number_of(&arg(a, 1))?)
    };
    Ok(Val::Num(number::parse_int(&s, r)))
}

fn parse_float(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let s = it.str_of(&arg(a, 0))?;
    Ok(Val::Num(number::parse_float(&s)))
}

fn is_nan(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    Ok(Val::Bool(it.number_of(&arg(a, 0))?.is_nan()))
}

fn is_finite(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    Ok(Val::Bool(it.number_of(&arg(a, 0))?.is_finite()))
}

/// `print` writes to the server's log in RefDb; here it is kept with the invocation.
fn print(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let mut parts = Vec::new();
    for x in a {
        parts.push(it.str_of(x)?.to_string());
    }
    it.charge(parts.iter().map(String::len).sum())?;
    it.output.push(parts.join(" "));
    Ok(Val::Undef)
}

/// `sleep(ms)` waits on the operation's clock in RefDb; the interpreter has no clock, so it
/// counts the wait as steps against the deadline.
fn sleep(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let ms = it.number_of(&arg(a, 0))?;
    let n = if ms.is_finite() && ms > 0.0 { (ms as u64).min(1_000_000) } else { 0 };
    for _ in 0..n {
        it.step()?;
    }
    Ok(Val::Undef)
}

/// `eval(code)`: a script in the global scope.
fn eval(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let Val::Str(src) = arg(a, 0) else { return Ok(arg(a, 0)) };
    let body =
        crate::parser::parse_script(&src).map_err(|e| it.error("SyntaxError", &e.message))?;
    let env = it.global_env.clone();
    it.hoist(&body, &env, true)?;
    let saved = it.last.take();
    for s in &body {
        it.exec(s, &env)?;
    }
    let r = it.last.take().unwrap_or(Val::Undef);
    it.last = saved;
    Ok(r)
}

// Object.

fn object_builtins(it: &mut Interp, g: &ObjRef) -> R<()> {
    let proto = it.realm.object.clone();
    let c = ctor(it, g, "Object", object_ctor, 1, &proto)?;
    for (name, f, n) in [
        ("keys", obj_keys as NativeFn, 1),
        ("values", obj_values, 1),
        ("entries", obj_entries, 1),
        ("assign", obj_assign, 2),
        ("hasOwn", obj_has_own, 2),
        ("create", obj_create, 2),
        ("getPrototypeOf", obj_get_proto, 1),
        ("setPrototypeOf", obj_set_proto, 2),
        ("getOwnPropertyNames", obj_own_names, 1),
        ("fromEntries", obj_from_entries, 1),
        ("is", obj_is, 2),
        ("groupBy", obj_group_by, 2),
    ] {
        method(it, &c, name, f, n)?;
    }
    for (name, f, n) in [
        ("hasOwnProperty", obj_proto_has_own as NativeFn, 1),
        ("toString", obj_to_string, 0),
        ("toLocaleString", obj_to_string, 0),
        ("valueOf", obj_value_of, 0),
        ("isPrototypeOf", obj_is_prototype_of, 1),
    ] {
        method(it, &proto, name, f, n)?;
    }
    let get = it.native("get __proto__", proto_getter, 0)?;
    let set = it.native("set __proto__", proto_setter, 1)?;
    let p = Prop { getter: Some(get), setter: Some(set), ..Prop::hidden(Val::Undef) };
    it.define(&proto, "__proto__", p)?;
    crate::props::install(it, &c)
}

fn object_ctor(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    match arg(a, 0) {
        Val::Undef | Val::Null => Ok(Val::Obj(it.object()?)),
        v => Ok(Val::Obj(it.object_of(&v)?)),
    }
}

fn keys_of(it: &mut Interp, v: &Val) -> R<(Val, Vec<Name>)> {
    let o = it.object_of(v)?;
    let keys = it.keys(&o, true)?;
    Ok((Val::Obj(o), keys))
}

fn obj_keys(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let (_, keys) = keys_of(it, &arg(a, 0))?;
    it.array(keys.into_iter().map(Val::Str).collect())
}

fn obj_values(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let (o, keys) = keys_of(it, &arg(a, 0))?;
    let mut out = Vec::new();
    for k in keys {
        out.push(it.get(&o, &k)?);
    }
    it.array(out)
}

fn obj_entries(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let (o, keys) = keys_of(it, &arg(a, 0))?;
    let mut out = Vec::new();
    for k in keys {
        let v = it.get(&o, &k)?;
        out.push(it.array(vec![Val::Str(k), v])?);
    }
    it.array(out)
}

fn obj_assign(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let target = Val::Obj(it.object_of(&arg(a, 0))?);
    for src in a.iter().skip(1) {
        if src.is_nullish() {
            continue;
        }
        let (o, keys) = keys_of(it, src)?;
        for k in keys {
            let v = it.get(&o, &k)?;
            // `Object.assign` sets as strict code does: a read-only property throws.
            let outer = std::mem::replace(&mut it.strict, true);
            let r = it.set(&target, &k, v);
            it.strict = outer;
            r?;
        }
    }
    Ok(target)
}

fn obj_has_own(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let o = it.object_of(&arg(a, 0))?;
    let k = it.key_of(&arg(a, 1))?;
    Ok(Val::Bool(it.has_own_property(&o, &k)?))
}

fn obj_proto_has_own(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = it.object_of(this)?;
    let k = it.key_of(&arg(a, 0))?;
    Ok(Val::Bool(it.has_own_property(&o, &k)?))
}

fn obj_create(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let proto = match arg(a, 0) {
        Val::Obj(p) => Some(p),
        Val::Null => None,
        v => return it.type_error(&format!("{} is not an object or null", it.show(&v))),
    };
    let o = it.alloc(proto, Kind::Ordinary)?;
    if !matches!(arg(a, 1), Val::Undef) {
        crate::props::define_all(it, &o, &arg(a, 1))?;
    }
    Ok(Val::Obj(o))
}

fn obj_get_proto(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let o = it.object_of(&arg(a, 0))?;
    let p = o.borrow().proto.clone();
    Ok(p.map_or(Val::Null, Val::Obj))
}

fn obj_set_proto(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let target = arg(a, 0);
    if target.is_nullish() {
        return it.type_error(&format!("can't convert {} to object", it.show(&target)));
    }
    let p = match arg(a, 1) {
        Val::Obj(p) => Some(p),
        Val::Null => None,
        v => {
            return it.type_error(&format!(
                "Object.setPrototypeOf: expected an object or null, got {}",
                v.type_of()
            ));
        }
    };
    if let Val::Obj(o) = &target {
        set_proto(it, o, p)?;
    }
    Ok(target)
}

/// Sets the prototype of `o`, refusing a cycle or a change to an object that is not extensible.
pub(crate) fn set_proto(it: &Interp, o: &ObjRef, p: Option<ObjRef>) -> R<()> {
    let same = match (&o.borrow().proto, &p) {
        (Some(x), Some(y)) => x.ptr_eq(y),
        (None, None) => true,
        _ => false,
    };
    if same {
        return Ok(());
    }
    if !o.borrow().extensible {
        return it.type_error("can't set prototype of this object");
    }
    let mut cur = p.clone();
    while let Some(c) = cur {
        if c.ptr_eq(o) {
            return it.type_error("can't set prototype: it would cause a prototype chain cycle");
        }
        cur = c.borrow().proto.clone();
    }
    o.borrow_mut().proto = p;
    Ok(())
}

fn proto_getter(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let o = it.object_of(this)?;
    let p = o.borrow().proto.clone();
    Ok(p.map_or(Val::Null, Val::Obj))
}

fn proto_setter(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    if this.is_nullish() {
        return it.type_error(&format!("can't convert {} to object", it.show(this)));
    }
    let p = match arg(a, 0) {
        Val::Obj(p) => Some(p),
        Val::Null => None,
        _ => return Ok(Val::Undef),
    };
    if let Val::Obj(o) = this {
        set_proto(it, o, p)?;
    }
    Ok(Val::Undef)
}

fn obj_own_names(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let o = it.object_of(&arg(a, 0))?;
    let mut keys = it.keys(&o, false)?;
    if matches!(o.borrow().kind, Kind::Array(_) | Kind::Str(_)) {
        keys.push("length".into());
    }
    it.array(keys.into_iter().map(Val::Str).collect())
}

fn obj_from_entries(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let o = it.object()?;
    for e in it.iterate(&arg(a, 0))? {
        let k = it.get(&e, "0")?;
        let k = it.key_of(&k)?;
        let v = it.get(&e, "1")?;
        it.define(&o, &k, Prop::data(v))?;
    }
    Ok(Val::Obj(o))
}

fn obj_is(_: &mut Interp, _: &Val, a: &[Val]) -> R {
    let (x, y) = (arg(a, 0), arg(a, 1));
    Ok(Val::Bool(match (&x, &y) {
        (Val::Num(p), Val::Num(q)) => {
            (p.is_nan() && q.is_nan()) || (p == q && p.is_sign_negative() == q.is_sign_negative())
        }
        _ => strict_eq(&x, &y),
    }))
}

fn obj_group_by(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let f = arg(a, 1);
    let o = it.alloc(None, Kind::Ordinary)?;
    for (i, x) in it.iterate(&arg(a, 0))?.into_iter().enumerate() {
        let k = it.call(&f, &Val::Undef, &[x.clone(), Val::Num(i as f64)])?;
        let k = it.key_of(&k)?;
        let cur = it.get(&Val::Obj(o.clone()), &k)?;
        match cur {
            Val::Obj(arr) => {
                it.grow(&arr, 1)?;
                if let Kind::Array(v) = &mut arr.borrow_mut().kind {
                    v.push(x);
                }
            }
            _ => {
                let arr = it.array(vec![x])?;
                it.define(&o, &k, Prop::data(arr))?;
            }
        }
    }
    Ok(Val::Obj(o))
}

fn obj_to_string(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    if let Val::Obj(o) = this
        && crate::proxy::is_proxy(o)
    {
        let tag = if crate::proxy::is_array(it, this)? {
            "Array"
        } else if o.borrow().is_callable() {
            "Function"
        } else {
            "Object"
        };
        return it.string(format!("[object {tag}]"));
    }
    if !this.is_nullish()
        && let Val::Str(tag) = it.get(this, &well_known("toStringTag"))?
    {
        return it.string(format!("[object {tag}]"));
    }
    let tag = match this {
        Val::Undef => "Undefined",
        Val::Null => "Null",
        Val::Obj(o) => match &o.borrow().kind {
            Kind::Array(_) => "Array",
            Kind::Func(_) => "Function",
            Kind::Error => "Error",
            Kind::Date(_) => "Date",
            Kind::RegExp(..) => "RegExp",
            Kind::Arguments => "Arguments",
            Kind::BigInt(_) => "BigInt",
            _ => "Object",
        },
        Val::Num(_) => "Number",
        Val::BigInt(_) => "BigInt",
        Val::Str(_) => "String",
        Val::Bool(_) => "Boolean",
        Val::Sym(_) => "Symbol",
    };
    it.string(format!("[object {tag}]"))
}

fn obj_value_of(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(Val::Obj(it.object_of(this)?))
}

fn obj_is_prototype_of(_: &mut Interp, this: &Val, a: &[Val]) -> R {
    let (Val::Obj(p), Val::Obj(o)) = (this, arg(a, 0)) else { return Ok(Val::Bool(false)) };
    let mut cur = o.borrow().proto.clone();
    while let Some(c) = cur {
        if c.ptr_eq(p) {
            return Ok(Val::Bool(true));
        }
        cur = c.borrow().proto.clone();
    }
    Ok(Val::Bool(false))
}

// Function.

fn function_builtins(it: &mut Interp, g: &ObjRef) -> R<()> {
    let proto = it.realm.function.clone();
    ctor(it, g, "Function", function_ctor, 1, &proto)?;
    // SpiderMonkey's order: the methods, `arguments`, `caller`, `constructor`, `length`, `name`.
    let constructor = proto.borrow().get_own("constructor").cloned();
    proto.borrow_mut().remove("constructor");
    for (name, f, n) in [
        ("toString", fn_to_string as NativeFn, 0),
        ("apply", fn_apply, 2),
        ("call", fn_call, 1),
        ("bind", fn_bind, 1),
    ] {
        method(it, &proto, name, f, n)?;
    }
    for name in ["arguments", "caller"] {
        let f = it.native("", restricted, 0)?;
        let p = Prop { getter: Some(f.clone()), setter: Some(f), ..Prop::hidden(Val::Undef) };
        it.define(&proto, name, p)?;
    }
    if let Some(c) = constructor {
        it.define(&proto, "constructor", c)?;
    }
    let fixed = |v: Val| Prop { writable: false, ..Prop::hidden(v) };
    it.define(&proto, "length", fixed(Val::Num(0.0)))?;
    it.define(&proto, "name", fixed(Val::str("")))?;
    let f = it.native("[Symbol.hasInstance]", fn_has_instance, 1)?;
    let fixed = Prop { writable: false, configurable: false, ..Prop::hidden(Val::Obj(f)) };
    it.define(&proto, &well_known("hasInstance"), fixed)
}

/// `Function.prototype.caller` and `arguments`: `null` for a sloppy function, an error for the
/// others.
fn restricted(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let sloppy = matches!(this, Val::Obj(o) if matches!(&o.borrow().kind,
        Kind::Func(Func::Closure { def, .. })
            if !def.strict && !def.arrow && !def.generator && !def.is_async));
    if sloppy {
        return Ok(Val::Null);
    }
    it.type_error(
        "'caller', 'callee', and 'arguments' properties may not be accessed on strict mode \
         functions or the arguments objects for calls to them",
    )
}

fn fn_has_instance(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    if !this.is_callable() {
        return Ok(Val::Bool(false));
    }
    Ok(Val::Bool(it.ordinary_instance_of(&arg(a, 0), this)?))
}

/// `Function(args…, body)`.
pub(crate) fn function_ctor(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let mut parts = Vec::new();
    for x in a {
        parts.push(it.str_of(x)?.to_string());
    }
    let body = parts.pop().unwrap_or_default();
    let src = format!("(function anonymous({}\n) {{\n{body}\n}})", parts.join(","));
    let stmts =
        crate::parser::parse_script(&src).map_err(|e| it.error("SyntaxError", &e.message))?;
    let env = it.global_env.clone();
    match stmts.first() {
        Some(crate::ast::Stmt::Expr(e)) => it.eval(e, &env),
        _ => it.type_error("bad function"),
    }
}

fn fn_call(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    it.call(this, &arg(a, 0), a.get(1..).unwrap_or(&[]))
}

fn fn_apply(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let args = match arg(a, 1) {
        Val::Undef | Val::Null => Vec::new(),
        v => it.iterate(&v)?,
    };
    it.call(this, &arg(a, 0), &args)
}

fn fn_bind(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let Val::Obj(target) = this else {
        return it.type_error("bind called on a value that is not a function");
    };
    let f = Func::Bound(target.clone(), arg(a, 0), a.get(1..).unwrap_or(&[]).to_vec());
    Ok(Val::Obj(it.alloc(Some(it.realm.function.clone()), Kind::Func(f))?))
}

fn fn_to_string(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let Val::Obj(o) = this else {
        return it.type_error("Function.prototype.toString called on incompatible object");
    };
    let s = match &o.borrow().kind {
        Kind::Func(Func::Closure { def, .. }) => def.source.to_string(),
        Kind::Func(Func::Native(_, n)) => format!("function {n}() {{\n    [native code]\n}}"),
        Kind::Func(Func::Bound(..)) => "function () {\n    [native code]\n}".to_owned(),
        _ => return it.type_error("Function.prototype.toString called on incompatible object"),
    };
    it.string(s)
}

// Array.

fn array_builtins(it: &mut Interp, g: &ObjRef) -> R<()> {
    let proto = it.realm.array.clone();
    let c = ctor(it, g, "Array", array_ctor, 1, &proto)?;
    for (name, f, n) in [
        ("isArray", array_is_array as NativeFn, 1),
        ("from", array_from, 1),
        ("of", array_of, 0),
        ("sum", array_sum, 1),
        ("avg", array_avg, 1),
        ("contains", array_contains, 2),
        ("unique", array_unique, 1),
        ("shuffle", array_shuffle, 1),
        ("stdDev", array_std_dev, 1),
    ] {
        method(it, &c, name, f, n)?;
    }
    crate::arrays::install(it, &proto)
}

fn array_ctor(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    if let [Val::Num(n)] = a {
        if *n < 0.0 || n.fract() != 0.0 || *n > f64::from(u32::MAX) {
            return Err(it.error("RangeError", "invalid array length"));
        }
        let n = *n as usize;
        it.charge(n * crate::interp::PROP_BYTES)?;
        return it.array(vec![Val::Undef; n]);
    }
    it.array(a.to_vec())
}

fn array_is_array(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    Ok(Val::Bool(crate::proxy::is_array(it, &arg(a, 0))?))
}

fn array_from(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let src = arg(a, 0);
    let items = match &src {
        Val::Obj(o)
            if !matches!(
                o.borrow().kind,
                Kind::Array(_) | Kind::Set(_) | Kind::Map(_) | Kind::Iter(_) | Kind::Arguments
            ) && !it.get(&src, "@@Symbol.iterator:0")?.is_callable() =>
        {
            let n = it.get(&src, "length")?;
            let n = it.number_of(&n)?;
            let n = if n.is_finite() && n > 0.0 { n as usize } else { 0 };
            it.charge(n * crate::interp::PROP_BYTES)?;
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                out.push(it.get(&src, &i.to_string())?);
            }
            out
        }
        Val::Undef | Val::Null => {
            return it.type_error(&format!("{} is not iterable", it.show(&src)));
        }
        _ => it.iterate(&src)?,
    };
    let f = arg(a, 1);
    if f.is_callable() {
        let mut out = Vec::with_capacity(items.len());
        for (i, x) in items.into_iter().enumerate() {
            out.push(it.call(&f, &Val::Undef, &[x, Val::Num(i as f64)])?);
        }
        return it.array(out);
    }
    it.array(items)
}

fn array_of(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    it.array(a.to_vec())
}

fn numbers(it: &mut Interp, v: &Val) -> R<Vec<f64>> {
    let mut out = Vec::new();
    for x in it.iterate(v)? {
        out.push(it.number_of(&x)?);
    }
    Ok(out)
}

/// RefDb's `Array.sum`: the elements added with `+` from the first.
fn array_sum(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let items = it.iterate(&arg(a, 0))?;
    let mut acc = match items.first() {
        Some(x) => x.clone(),
        None => return Ok(Val::Null),
    };
    for x in &items[1..] {
        acc = it.binary("+", &acc, x)?;
    }
    Ok(acc)
}

fn array_avg(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let xs = numbers(it, &arg(a, 0))?;
    if xs.is_empty() {
        return Ok(Val::Undef);
    }
    Ok(Val::Num(xs.iter().sum::<f64>() / xs.len() as f64))
}

fn array_std_dev(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let xs = numbers(it, &arg(a, 0))?;
    let n = xs.len() as f64;
    let mean = xs.iter().sum::<f64>() / n;
    Ok(Val::Num((xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n).sqrt()))
}

fn array_contains(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let items = it.iterate(&arg(a, 0))?;
    Ok(Val::Bool(items.iter().any(|x| strict_eq(x, &arg(a, 1)))))
}

fn array_unique(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let mut out: Vec<Val> = Vec::new();
    for x in it.iterate(&arg(a, 0))? {
        if !out.iter().any(|y| strict_eq(y, &x)) {
            out.push(x);
        }
    }
    it.array(out)
}

/// RefDb's `Array.shuffle` calls a `Random` its engine does not define (measured: it fails
/// with `Random is not defined` on a `refrouter` 9.0.4); an empty array has nothing to draw.
fn array_shuffle(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    if let Val::Obj(o) = arg(a, 0) {
        let n = it.array_len(&o);
        if n > 0 {
            return Err(it.error("ReferenceError", "Random is not defined"));
        }
    }
    Ok(arg(a, 0))
}

// Number and Boolean.

fn number_builtins(it: &mut Interp, g: &ObjRef) -> R<()> {
    let proto = it.realm.number.clone();
    let c = ctor(it, g, "Number", number_ctor, 1, &proto)?;
    for (name, v) in [
        ("MAX_SAFE_INTEGER", 9_007_199_254_740_991.0),
        ("MIN_SAFE_INTEGER", -9_007_199_254_740_991.0),
        ("MAX_VALUE", f64::MAX),
        ("MIN_VALUE", 5e-324),
        ("EPSILON", f64::EPSILON),
        ("POSITIVE_INFINITY", f64::INFINITY),
        ("NEGATIVE_INFINITY", f64::NEG_INFINITY),
        ("NaN", f64::NAN),
    ] {
        it.define(&c, name, Prop { writable: false, ..Prop::hidden(Val::Num(v)) })?;
    }
    for (name, f, n) in [
        ("isInteger", num_is_integer as NativeFn, 1),
        ("isSafeInteger", num_is_safe_integer, 1),
        ("isFinite", num_is_finite, 1),
        ("isNaN", num_is_nan, 1),
        ("parseFloat", parse_float, 1),
        ("parseInt", parse_int, 2),
    ] {
        method(it, &c, name, f, n)?;
    }
    for (name, f, n) in [
        ("toString", num_to_string as NativeFn, 1),
        ("toFixed", num_to_fixed, 1),
        ("valueOf", num_value_of, 0),
        ("toLocaleString", num_to_string, 0),
        ("toPrecision", num_to_precision, 1),
        ("toExponential", num_to_exponential, 1),
    ] {
        method(it, &proto, name, f, n)?;
    }
    let bproto = it.realm.boolean.clone();
    ctor(it, g, "Boolean", boolean_ctor, 1, &bproto)?;
    method(it, &bproto, "toString", bool_to_string, 0)?;
    method(it, &bproto, "valueOf", bool_value_of, 0)?;
    Ok(())
}

fn number_ctor(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let n = match a.first() {
        None => 0.0,
        Some(v) => match it.numeric(v)? {
            Val::BigInt(b) => b.to_f64(),
            n => it.number_of(&n)?,
        },
    };
    if new_target(this).is_some() {
        return Ok(Val::Obj(it.alloc(Some(it.realm.number.clone()), Kind::Num(n))?));
    }
    Ok(Val::Num(n))
}

fn boolean_ctor(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let b = arg(a, 0).truthy();
    if new_target(this).is_some() {
        return Ok(Val::Obj(it.alloc(Some(it.realm.boolean.clone()), Kind::Bool(b))?));
    }
    Ok(Val::Bool(b))
}

fn this_number(it: &mut Interp, this: &Val) -> R<f64> {
    match this {
        Val::Num(n) => Ok(*n),
        Val::Obj(o) => match &o.borrow().kind {
            Kind::Num(n) => Ok(*n),
            _ => {
                Err(it.error("TypeError", "Number.prototype method called on incompatible object"))
            }
        },
        _ => it.type_error("Number.prototype method called on incompatible value"),
    }
}

fn num_to_string(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let n = this_number(it, this)?;
    let radix = if matches!(arg(a, 0), Val::Undef) { 10.0 } else { it.number_of(&arg(a, 0))? };
    if !(2.0..=36.0).contains(&radix) {
        return Err(
            it.error("RangeError", "radix must be an integer at least 2 and no greater than 36")
        );
    }
    it.string(number::to_radix(n, radix as u32))
}

fn num_to_fixed(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let n = this_number(it, this)?;
    let d = it.number_of(&arg(a, 0))?;
    let d = if d.is_nan() { 0.0 } else { d.trunc() };
    if !(0.0..=100.0).contains(&d) {
        let m = format!("precision {} out of range", number::to_string(d));
        return Err(it.error("RangeError", &m));
    }
    it.string(number::to_fixed(n, d as usize))
}

/// `toExponential`: one digit, the point, `d` digits (as few as tell the number apart when `d`
/// is undefined) and the exponent; a tie rounds away from zero.
fn num_to_exponential(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let n = this_number(it, this)?;
    let d = match arg(a, 0) {
        Val::Undef => None,
        v => {
            let d = it.number_of(&v)?;
            Some(if d.is_nan() { 0.0 } else { d.trunc() })
        }
    };
    if !n.is_finite() {
        return it.string(number::to_string(n));
    }
    if let Some(d) = d
        && !(0.0..=100.0).contains(&d)
    {
        let m = format!("precision {} out of range", number::to_string(d));
        return Err(it.error("RangeError", &m));
    }
    let x = n.abs();
    let text = match d {
        None => format!("{x:e}"),
        Some(d) => {
            let d = d as usize;
            // The exact digits decide a tie, which ECMAScript rounds up.
            let exact = format!("{:.*e}", d + 1100, x);
            let (mant, _) = exact.split_once('e').unwrap_or((&exact, "0"));
            let rest = mant.replace('.', "");
            let tail = rest.get(d + 1..).unwrap_or("");
            let tie = tail.starts_with('5') && tail[1..].bytes().all(|b| b == b'0');
            let r = format!("{:.*e}", d, x);
            if tie && !rounded_up(&r, &rest, d) {
                format!("{:.*e}", d, x + x * f64::EPSILON)
            } else {
                r
            }
        }
    };
    let (mant, exp) = text.split_once('e').unwrap_or((&text, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let sign = if n < 0.0 { "-" } else { "" };
    let esign = if exp < 0 { "-" } else { "+" };
    it.string(format!("{sign}{mant}e{esign}{}", exp.abs()))
}

/// Whether `rounded` (with `d` digits after the point) went up from the exact digits.
fn rounded_up(rounded: &str, exact_digits: &str, d: usize) -> bool {
    let (mant, _) = rounded.split_once('e').unwrap_or((rounded, "0"));
    let r = mant.replace('.', "");
    r.as_str() != exact_digits.get(..d + 1).unwrap_or("")
}

fn num_to_precision(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let n = this_number(it, this)?;
    if matches!(arg(a, 0), Val::Undef) {
        return it.string(number::to_string(n));
    }
    let p = it.number_of(&arg(a, 0))?;
    let p = if p.is_nan() { 0.0 } else { p.trunc() };
    if !(1.0..=100.0).contains(&p) {
        let m = format!("precision {} out of range", number::to_string(p));
        return Err(it.error("RangeError", &m));
    }
    if !n.is_finite() {
        return it.string(number::to_string(n));
    }
    if n == 0.0 {
        let zeros = "0".repeat(p as usize - 1);
        return it.string(if zeros.is_empty() { "0".into() } else { format!("0.{zeros}") });
    }
    let e = n.abs().log10().floor() as i32;
    let p = p as i32;
    let s = if e < -6 || e >= p {
        let m = format!("{:.*e}", (p - 1) as usize, n);
        let (mant, exp) = m.split_once('e').unwrap_or((&m, "0"));
        let exp: i32 = exp.parse().unwrap_or(0);
        format!("{mant}e{}{}", if exp < 0 { '-' } else { '+' }, exp.abs())
    } else {
        format!("{:.*}", (p - 1 - e).max(0) as usize, n)
    };
    it.string(s)
}

fn num_value_of(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(Val::Num(this_number(it, this)?))
}

fn num_is_integer(_: &mut Interp, _: &Val, a: &[Val]) -> R {
    Ok(Val::Bool(matches!(arg(a, 0), Val::Num(n) if n.is_finite() && n.fract() == 0.0)))
}

fn num_is_safe_integer(_: &mut Interp, _: &Val, a: &[Val]) -> R {
    Ok(Val::Bool(
        matches!(arg(a, 0), Val::Num(n) if n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0),
    ))
}

fn num_is_finite(_: &mut Interp, _: &Val, a: &[Val]) -> R {
    Ok(Val::Bool(matches!(arg(a, 0), Val::Num(n) if n.is_finite())))
}

fn num_is_nan(_: &mut Interp, _: &Val, a: &[Val]) -> R {
    Ok(Val::Bool(matches!(arg(a, 0), Val::Num(n) if n.is_nan())))
}

fn bool_to_string(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let b = bool_value_of(it, this, &[])?;
    Ok(Val::str(if b.truthy() { "true" } else { "false" }))
}

fn bool_value_of(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    match this {
        Val::Bool(b) => Ok(Val::Bool(*b)),
        Val::Obj(o) => {
            match &o.borrow().kind {
                Kind::Bool(b) => Ok(Val::Bool(*b)),
                _ => Err(it
                    .error("TypeError", "Boolean.prototype.valueOf called on incompatible object")),
            }
        }
        _ => it.type_error("Boolean.prototype.valueOf called on incompatible value"),
    }
}

// Errors.

const ERRORS: [&str; 7] = [
    "TypeError",
    "RangeError",
    "SyntaxError",
    "ReferenceError",
    "EvalError",
    "URIError",
    "InternalError",
];

fn errors(it: &mut Interp, g: &ObjRef) -> R<()> {
    let proto = it.realm.error.clone();
    let base = ctor(it, g, "Error", error_ctor, 1, &proto)?;
    it.define(&proto, "name", Prop::hidden(Val::str("Error")))?;
    it.define(&proto, "message", Prop::hidden(Val::str("")))?;
    method(it, &proto, "toString", error_to_string, 0)?;
    it.realm.errors.push(("Error".into(), base.clone()));
    for name in ERRORS {
        let p = it.alloc(Some(proto.clone()), Kind::Ordinary)?;
        it.define(&p, "name", Prop::hidden(Val::str(name)))?;
        it.define(&p, "message", Prop::hidden(Val::str("")))?;
        let c = ctor(it, g, name, error_ctor, 1, &p)?;
        c.borrow_mut().proto = Some(base.clone());
        it.realm.errors.push((name.into(), c));
    }
    let p = it.alloc(Some(proto.clone()), Kind::Ordinary)?;
    it.define(&p, "name", Prop::hidden(Val::str("AggregateError")))?;
    it.define(&p, "message", Prop::hidden(Val::str("")))?;
    let c = ctor(it, g, "AggregateError", aggregate_error_ctor, 2, &p)?;
    c.borrow_mut().proto = Some(base);
    it.realm.errors.push(("AggregateError".into(), c));
    Ok(())
}

/// `new AggregateError(errors, message, options)`.
fn aggregate_error_ctor(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let errors = it.iterate(&arg(a, 0))?;
    let rest: Vec<Val> = a.iter().skip(1).cloned().collect();
    let e = error_ctor(it, this, &rest)?;
    if let Val::Obj(o) = &e {
        let list = it.array(errors)?;
        it.define(o, "errors", Prop::hidden(list))?;
    }
    Ok(e)
}

fn error_ctor(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    // Called without `new`, the error of the callee's own prototype is made the same way.
    let proto = match new_target(this) {
        Some(t) => it.get(&t, "prototype")?.as_obj().cloned(),
        None => Some(it.realm.error.clone()),
    };
    let o = it.alloc(proto, Kind::Error)?;
    if !matches!(arg(a, 0), Val::Undef) {
        let m = it.str_of(&arg(a, 0))?;
        it.define(&o, "message", Prop::hidden(Val::Str(m)))?;
    }
    if let Val::Obj(opts) = arg(a, 1)
        && it.has_property(&opts, "cause")
    {
        let c = it.get(&Val::Obj(opts), "cause")?;
        it.define(&o, "cause", Prop::hidden(c))?;
    }
    it.place_error(&o)?;
    Ok(Val::Obj(o))
}

fn error_to_string(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let n = it.get(this, "name")?;
    let n = if matches!(n, Val::Undef) { "Error".into() } else { it.str_of(&n)? };
    let m = it.get(this, "message")?;
    let m = if matches!(m, Val::Undef) { "".into() } else { it.str_of(&m)? };
    let s = match (n.is_empty(), m.is_empty()) {
        (_, true) => n.to_string(),
        (true, false) => m.to_string(),
        _ => format!("{n}: {m}"),
    };
    it.string(s)
}

// Map and Set.

fn collections(it: &mut Interp, g: &ObjRef) -> R<()> {
    let mp = it.realm.map.clone();
    ctor(it, g, "Map", map_ctor, 0, &mp)?;
    for (name, f, n) in [
        ("get", map_get as NativeFn, 1),
        ("set", map_set, 2),
        ("has", map_has, 1),
        ("delete", map_delete, 1),
        ("clear", coll_clear, 0),
        ("forEach", coll_for_each, 1),
        ("keys", map_keys, 0),
        ("values", map_values, 0),
        ("entries", coll_entries, 0),
    ] {
        method(it, &mp, name, f, n)?;
    }
    it.define(
        &mp,
        "size",
        Prop { getter: Some(it.native("size", coll_size, 0)?), ..Prop::hidden(Val::Undef) },
    )?;
    let sp = it.realm.set.clone();
    ctor(it, g, "Set", set_ctor, 0, &sp)?;
    for (name, f, n) in [
        ("add", set_add as NativeFn, 1),
        ("has", map_has, 1),
        ("delete", map_delete, 1),
        ("clear", coll_clear, 0),
        ("forEach", coll_for_each, 1),
        ("values", map_values, 0),
        ("keys", map_values, 0),
        ("entries", coll_entries, 0),
    ] {
        method(it, &sp, name, f, n)?;
    }
    it.define(
        &sp,
        "size",
        Prop { getter: Some(it.native("size", coll_size, 0)?), ..Prop::hidden(Val::Undef) },
    )?;
    for o in [&mp, &sp] {
        let f = it.native("[Symbol.iterator]", coll_default_iter, 0)?;
        it.define(o, "@@Symbol.iterator:0", Prop::hidden(Val::Obj(f)))?;
    }
    let ip = it.realm.iter.clone();
    method(it, &ip, "next", iter_next, 0)?;
    let f = it.native("[Symbol.iterator]", iter_self, 0)?;
    it.define(&ip, "@@Symbol.iterator:0", Prop::hidden(Val::Obj(f)))?;
    Ok(())
}

fn map_ctor(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    if new_target(this).is_none() {
        return it.type_error("calling a builtin Map constructor without new is forbidden");
    }
    let m = it.alloc(Some(it.realm.map.clone()), Kind::Map(Vec::new()))?;
    if !arg(a, 0).is_nullish() {
        for e in it.iterate(&arg(a, 0))? {
            let k = it.get(&e, "0")?;
            let v = it.get(&e, "1")?;
            map_set(it, &Val::Obj(m.clone()), &[k, v])?;
        }
    }
    Ok(Val::Obj(m))
}

fn set_ctor(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    if new_target(this).is_none() {
        return it.type_error("calling a builtin Set constructor without new is forbidden");
    }
    let s = it.alloc(Some(it.realm.set.clone()), Kind::Set(Vec::new()))?;
    if !arg(a, 0).is_nullish() {
        for x in it.iterate(&arg(a, 0))? {
            set_add(it, &Val::Obj(s.clone()), &[x])?;
        }
    }
    Ok(Val::Obj(s))
}

fn coll(it: &Interp, this: &Val) -> R<ObjRef> {
    match this {
        Val::Obj(o) if matches!(o.borrow().kind, Kind::Map(_) | Kind::Set(_)) => Ok(o.clone()),
        _ => it.type_error("method called on incompatible object"),
    }
}

/// A key of a `Map` or `Set`: -0 and +0 are one key (adding +0 turns -0 into +0).
fn norm(k: Val) -> Val {
    match k {
        Val::Num(n) => Val::Num(n + 0.0),
        k => k,
    }
}

fn map_get(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = coll(it, this)?;
    let k = arg(a, 0);
    Ok(match &o.borrow().kind {
        Kind::Map(e) => {
            e.iter().find(|(x, _)| same_value_zero(x, &k)).map_or(Val::Undef, |(_, v)| v.clone())
        }
        _ => Val::Undef,
    })
}

fn map_set(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = coll(it, this)?;
    let k = norm(arg(a, 0));
    let mut b = o.borrow_mut();
    if let Kind::Map(e) = &mut b.kind {
        if let Some(slot) = e.iter_mut().find(|(x, _)| same_value_zero(x, &k)) {
            slot.1 = arg(a, 1);
        } else {
            drop(b);
            it.grow(&o, 2)?;
            if let Kind::Map(e) = &mut o.borrow_mut().kind {
                e.push((k, arg(a, 1)));
            }
        }
    }
    Ok(this.clone())
}

fn set_add(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = coll(it, this)?;
    let k = norm(arg(a, 0));
    let present =
        matches!(&o.borrow().kind, Kind::Set(e) if e.iter().any(|x| same_value_zero(x, &k)));
    if !present {
        it.grow(&o, 1)?;
        if let Kind::Set(e) = &mut o.borrow_mut().kind {
            e.push(k);
        }
    }
    Ok(this.clone())
}

fn map_has(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = coll(it, this)?;
    let k = arg(a, 0);
    Ok(Val::Bool(match &o.borrow().kind {
        Kind::Map(e) => e.iter().any(|(x, _)| same_value_zero(x, &k)),
        Kind::Set(e) => e.iter().any(|x| same_value_zero(x, &k)),
        _ => false,
    }))
}

fn map_delete(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = coll(it, this)?;
    let k = arg(a, 0);
    let mut b = o.borrow_mut();
    let before;

    let after = match &mut b.kind {
        Kind::Map(e) => {
            before = e.len();
            e.retain(|(x, _)| !same_value_zero(x, &k));
            e.len()
        }
        Kind::Set(e) => {
            before = e.len();
            e.retain(|x| !same_value_zero(x, &k));
            e.len()
        }
        _ => return Ok(Val::Bool(false)),
    };
    Ok(Val::Bool(before != after))
}

fn coll_clear(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let o = coll(it, this)?;
    match &mut o.borrow_mut().kind {
        Kind::Map(e) => e.clear(),
        Kind::Set(e) => e.clear(),
        _ => {}
    }
    Ok(Val::Undef)
}

fn coll_size(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let o = coll(it, this)?;
    let n = match &o.borrow().kind {
        Kind::Map(e) => e.len(),
        Kind::Set(e) => e.len(),
        _ => 0,
    };
    Ok(Val::Num(n as f64))
}

fn coll_for_each(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = coll(it, this)?;
    let f = arg(a, 0);
    let pairs: Vec<(Val, Val)> = match &o.borrow().kind {
        Kind::Map(e) => e.iter().map(|(k, v)| (v.clone(), k.clone())).collect(),
        Kind::Set(e) => e.iter().map(|v| (v.clone(), v.clone())).collect(),
        _ => Vec::new(),
    };
    for (v, k) in pairs {
        it.call(&f, &arg(a, 1), &[v, k, this.clone()])?;
    }
    Ok(Val::Undef)
}

pub(crate) fn make_iter(it: &Interp, items: Vec<Val>) -> R {
    let state = Rc::new(std::cell::RefCell::new((items, 0usize)));
    Ok(Val::Obj(it.alloc(Some(it.realm.iter.clone()), Kind::Iter(state))?))
}

fn map_keys(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let o = coll(it, this)?;
    let items = match &o.borrow().kind {
        Kind::Map(e) => e.iter().map(|(k, _)| k.clone()).collect(),
        _ => Vec::new(),
    };
    make_iter(it, items)
}

fn map_values(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let o = coll(it, this)?;
    let items = match &o.borrow().kind {
        Kind::Map(e) => e.iter().map(|(_, v)| v.clone()).collect(),
        Kind::Set(e) => e.clone(),
        _ => Vec::new(),
    };
    make_iter(it, items)
}

fn coll_entries(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let o = coll(it, this)?;
    let pairs: Vec<(Val, Val)> = match &o.borrow().kind {
        Kind::Map(e) => e.clone(),
        Kind::Set(e) => e.iter().map(|v| (v.clone(), v.clone())).collect(),
        _ => Vec::new(),
    };
    let mut items = Vec::new();
    for (k, v) in pairs {
        items.push(it.array(vec![k, v])?);
    }
    make_iter(it, items)
}

fn coll_default_iter(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = coll(it, this)?;
    let is_map = matches!(o.borrow().kind, Kind::Map(_));
    if is_map { coll_entries(it, this, a) } else { map_values(it, this, a) }
}

fn iter_next(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let Val::Obj(o) = this else { return it.type_error("next called on incompatible object") };
    let Kind::Iter(state) = o.borrow().kind.clone() else {
        return it.type_error("next called on incompatible object");
    };
    let (v, done) = {
        let (items, pos) = &mut *state.borrow_mut();
        match items.get(*pos) {
            Some(v) => {
                *pos += 1;
                (v.clone(), false)
            }
            None => (Val::Undef, true),
        }
    };
    let r = it.object()?;
    it.define(&r, "value", Prop::data(v))?;
    it.define(&r, "done", Prop::data(Val::Bool(done)))?;
    Ok(Val::Obj(r))
}

fn iter_self(_: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(this.clone())
}

// Symbol.

fn symbol_builtins(it: &mut Interp, g: &ObjRef) -> R<()> {
    crate::symbols::install(it, g)?;
    let (mp, sp) = (it.realm.map.clone(), it.realm.set.clone());
    to_string_tag(it, &mp, "Map")?;
    to_string_tag(it, &sp, "Set")?;
    for name in ["Math", "JSON"] {
        if let Val::Obj(o) = it.get(&Val::Obj(g.clone()), name)? {
            to_string_tag(it, &o, name)?;
        }
    }
    let arr = it.realm.array.clone();
    let values = it.get(&Val::Obj(arr.clone()), "values")?;
    it.define(&arr, "@@Symbol.iterator:0", Prop::hidden(values))?;
    let unscopables = it.alloc(None, Kind::Ordinary)?;
    for name in [
        "at",
        "copyWithin",
        "entries",
        "fill",
        "find",
        "findIndex",
        "findLast",
        "findLastIndex",
        "flat",
        "flatMap",
        "includes",
        "keys",
        "toReversed",
        "toSorted",
        "toSpliced",
        "values",
    ] {
        it.define(&unscopables, name, Prop::data(Val::Bool(true)))?;
    }
    let fixed = Prop { writable: false, ..Prop::hidden(Val::Obj(unscopables)) };
    it.define(&arr, &well_known("unscopables"), fixed)?;
    let sp = it.realm.string.clone();
    let f = it.native("[Symbol.iterator]", string_iter, 0)?;
    it.define(&sp, "@@Symbol.iterator:0", Prop::hidden(Val::Obj(f)))?;
    Ok(())
}

fn string_iter(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let s = it.str_of(this)?;
    make_iter(it, s.chars().map(|c| Val::Str(c.to_string().into())).collect())
}

// RegExp: the literal and constructor keep the source and flags; matching is in `strings`.

fn regexp_builtins(it: &mut Interp, g: &ObjRef) -> R<()> {
    let proto = it.realm.regexp.clone();
    ctor(it, g, "RegExp", regexp_ctor, 2, &proto)?;
    method(it, &proto, "test", crate::regex::test, 1)?;
    method(it, &proto, "exec", crate::regex::exec, 1)?;
    method(it, &proto, "toString", regexp_to_string, 0)?;
    for (name, f) in
        [("source", regexp_source as NativeFn), ("flags", regexp_flags), ("global", regexp_global)]
    {
        let getter = it.native(name, f, 0)?;
        it.define(&proto, name, Prop { getter: Some(getter), ..Prop::hidden(Val::Undef) })?;
    }
    Ok(())
}

pub fn make_regexp(it: &mut Interp, source: Name, flags: Name) -> R {
    crate::regex::check(it, &source, &flags)?;
    let o = it.alloc(Some(it.realm.regexp.clone()), Kind::RegExp(source, flags))?;
    it.define(&o, "lastIndex", Prop::hidden(Val::Num(0.0)))?;
    Ok(Val::Obj(o))
}

fn regexp_ctor(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let (src, flags) = match arg(a, 0) {
        Val::Obj(o) if matches!(o.borrow().kind, Kind::RegExp(..)) => match &o.borrow().kind {
            Kind::RegExp(s, f) => (s.clone(), f.clone()),
            _ => unreachable_pair(),
        },
        Val::Undef => ("(?:)".into(), "".into()),
        v => (it.str_of(&v)?, "".into()),
    };
    let flags = if matches!(arg(a, 1), Val::Undef) { flags } else { it.str_of(&arg(a, 1))? };
    make_regexp(it, src, flags)
}

fn unreachable_pair() -> (Name, Name) {
    ("(?:)".into(), "".into())
}

fn regexp_parts(it: &Interp, this: &Val) -> R<(Name, Name)> {
    match this {
        Val::Obj(o) => match &o.borrow().kind {
            Kind::RegExp(s, f) => Ok((s.clone(), f.clone())),
            _ => it.type_error("RegExp method called on incompatible object"),
        },
        _ => it.type_error("RegExp method called on incompatible value"),
    }
}

fn regexp_to_string(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let (s, f) = regexp_parts(it, this)?;
    it.string(format!("/{s}/{f}"))
}

fn regexp_source(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(Val::Str(regexp_parts(it, this)?.0))
}

fn regexp_flags(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(Val::Str(regexp_parts(it, this)?.1))
}

fn regexp_global(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(Val::Bool(regexp_parts(it, this)?.1.contains('g')))
}

/// The error a native function gives for `this` of the wrong kind.
pub fn incompatible(it: &Interp, what: &str) -> Ctrl {
    it.error("TypeError", &format!("{what} called on incompatible object"))
}
