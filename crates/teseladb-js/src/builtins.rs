//! The global objects (ECMA-262 §19 to §24) the engine of RefDb 9.0.4 has, measured on a
//! `refrouter` 9.0.4 on 02-11: `Object`, `Function`, `Array`, `String`, `Number`, `Boolean`,
//! `Symbol`, `Math`, `JSON`, `Date`, `RegExp`, `Map`, `Set`, the errors, and RefDb's `print`,
//! `sleep` and `Array.sum`. `Reflect` is not there.

use std::rc::Rc;

use crate::interp::{Ctrl, Interp, R, Realm};
use crate::keyed::{Cursor, Table};
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

/// `number is not a non-null object`: what SpiderMonkey says of a value that had to be an
/// object (measured on a `refrouter` 9.0.4).
pub(crate) fn non_object(v: &Val) -> String {
    let t = if matches!(v, Val::Null) { "null" } else { v.type_of() };
    format!("{t} is not a non-null object")
}

/// How SpiderMonkey names the type of `this` in «… method called on incompatible X»: an
/// object's class, a primitive's type.
pub(crate) fn class_name(v: &Val) -> String {
    if let Val::Obj(o) = v {
        if crate::proxy::is_proxy(o) {
            return "Proxy".into();
        }
        // A built-in prototype is named after its constructor (measured on a `refrouter` 9.0.4 on
        // 02-11: `Date.prototype.getTime called on incompatible Date.prototype`).
        if let Some(name) = prototype_of_ctor(o) {
            return format!("{name}.prototype");
        }
    }
    class_kind(v).into()
}

/// The name of the constructor whose `prototype` is `o`, when `o` is that constructor's.
fn prototype_of_ctor(o: &ObjRef) -> Option<String> {
    let ctor = o.borrow().get_own("constructor")?.value.as_obj().cloned()?;
    if !matches!(ctor.borrow().kind, Kind::Func(Func::Native(..))) {
        return None;
    }
    let proto = ctor.borrow().get_own("prototype")?.value.as_obj().cloned()?;
    if !proto.ptr_eq(o) {
        return None;
    }
    let name = ctor.borrow().get_own("name")?.value.clone();
    match name {
        Val::Str(n) if !n.is_empty() => Some(n.to_string()),
        _ => None,
    }
}

pub(crate) fn class_kind(v: &Val) -> &'static str {
    match v {
        Val::Obj(o) => match &o.borrow().kind {
            Kind::Array(_) => "Array",
            Kind::Func(Func::Bound(..)) => "BoundFunctionObject",
            Kind::Func(_) => "Function",
            Kind::Map(_) => "Map",
            Kind::Set(_) => "Set",
            Kind::WeakMap(_) => "WeakMap",
            Kind::WeakSet(_) => "WeakSet",
            Kind::Date(_) => "Date",
            Kind::RegExp(..) => "RegExp",
            Kind::Error => "Error",
            Kind::Bool(_) => "Boolean",
            Kind::Num(_) => "Number",
            Kind::Str(_) => "String",
            Kind::BigInt(_) => "BigInt",
            Kind::Symbol(_) => "Symbol",
            Kind::Proxy(..) => "Proxy",
            Kind::ArrayBuffer(_) => "ArrayBuffer",
            Kind::DataView(_) => "DataView",
            Kind::Typed(t) => t.ty.name(),
            Kind::Promise(_) => "Promise",
            Kind::WeakRef(_) => "WeakRef",
            Kind::Arguments(_) => "Arguments",
            Kind::ArrayIter(_) => ARRAY_ITERATOR,
            Kind::CollIter(state) => state.borrow().class,
            Kind::Iter(_, class) => class,
            Kind::Helper(_) => "Iterator Helper",
            Kind::Generator(_) => "Generator",
            Kind::AsyncGenerator => "AsyncGenerator",
            Kind::Ordinary | Kind::Bson(_) => "Object",
        },
        Val::Null => "null",
        other => other.type_of(),
    }
}

/// The class SpiderMonkey names in «X is not extensible»: a BSON type, or the kind of object.
pub(crate) fn object_class(it: &Interp, o: &ObjRef) -> &'static str {
    if o.ptr_eq(&it.realm.global) {
        return "global";
    }
    crate::bson::bson_class(it, o).unwrap_or_else(|| class_kind(&Val::Obj(o.clone())))
}

/// The key of the well-known symbol `Symbol.<name>`.
pub(crate) fn well_known(name: &str) -> String {
    format!("@@Symbol.{name}:0")
}

/// The global object `name`, which the realm installed before.
pub(crate) fn installed(g: &ObjRef, name: &str) -> ObjRef {
    match g.borrow().get_own(name).map(|p| &p.value) {
        Some(Val::Obj(o)) => o.clone(),
        _ => unreachable!("{name} is installed before"),
    }
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
        charged: std::cell::Cell::new(crate::value::Cost::ZERO),
        budget: budget.clone(),
        shared: std::cell::Cell::new(false),
        pristine: std::cell::Cell::new(false),
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
    crate::setmethods::install(it, &g)?;
    symbol_builtins(it, &g)?;
    crate::bigints::install(it, &g)?;
    crate::weak::install(it, &g)?;
    crate::promise::install(it, &g)?;
    crate::proxy::install(it, &g)?;
    crate::typed::install(it, &g)?;
    crate::iterators::install(it, &g)?;
    regexp_builtins(it, &g)?;
    crate::bson::install(it, &g)?;
    crate::uri::install(it, &g)?;
    crate::symbols::install_species(it, &g)?;
    crate::shell::install(it, &g)?;
    crate::order::apply(it);
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
    // The functions of those objects (methods, accessors) are frozen too.
    let mut functions = Vec::new();
    for o in &all {
        for (_, p) in &o.borrow().props {
            let parts = [p.value.as_obj(), p.getter.as_ref(), p.setter.as_ref()];
            functions
                .extend(parts.into_iter().flatten().filter(|f| f.borrow().is_callable()).cloned());
        }
    }
    for o in all.iter().chain(&functions) {
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
    it.charge(crate::value::Cost::text(parts.iter().map(String::len).sum()))?;
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
    if let Some(v) = crate::json::eval_json(it, &src)? {
        return Ok(v);
    }
    let room = it.parse_room();
    let body = crate::parser::parse_eval(&src, room).map_err(|e| it.syntax_error(&e))?;
    let env = it.global_env.clone();
    it.hoist(&body, &env, true)?;
    if !crate::parser::has_use_strict(&body) {
        it.hoist_block_functions(&body, &env, &[]);
    }
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
        ("toLocaleString", obj_to_locale_string, 0),
        ("valueOf", obj_value_of, 0),
        ("isPrototypeOf", obj_is_prototype_of, 1),
        ("__defineGetter__", |it, t, a| define_legacy_accessor(it, t, a, true), 2),
        ("__defineSetter__", |it, t, a| define_legacy_accessor(it, t, a, false), 2),
        ("__lookupGetter__", |it, t, a| lookup_legacy_accessor(it, t, a, true), 1),
        ("__lookupSetter__", |it, t, a| lookup_legacy_accessor(it, t, a, false), 1),
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
        let (o, mut keys) = keys_of(it, src)?;
        if let Val::Obj(so) = &o {
            keys.extend(crate::props::enumerable_symbol_keys(so));
        }
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
    Ok(it.proto_of(&o)?.map_or(Val::Null, Val::Obj))
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
        set_proto_of(it, o, p)?;
    }
    Ok(target)
}

/// Sets the prototype of `o`, through a proxy's `setPrototypeOf`.
pub(crate) fn set_proto_of(it: &mut Interp, o: &ObjRef, p: Option<ObjRef>) -> R<()> {
    if crate::proxy::is_proxy(o) {
        return crate::proxy::set_proto(it, o, p);
    }
    set_proto(it, o, p)
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
    Ok(it.proto_of(&o)?.map_or(Val::Null, Val::Obj))
}

fn proto_setter(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    if this.is_nullish() {
        let m = format!("std_Object_setProto method called on incompatible {}", it.show(this));
        return it.type_error(&m);
    }
    match (this, arg(a, 0)) {
        (Val::Obj(o), Val::Obj(p)) => set_proto_of(it, o, Some(p))?,
        (Val::Obj(o), Val::Null) => set_proto_of(it, o, None)?,
        _ => {}
    }
    Ok(Val::Undef)
}

fn obj_own_names(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let o = it.object_of(&arg(a, 0))?;
    let mut keys = it.keys(&o, false)?;
    if matches!(o.borrow().kind, Kind::Array(_) | Kind::Str(_)) {
        // After the indices and before the other keys (ECMA-262 §10.1.11.1).
        let at = keys.iter().take_while(|k| array_index(k).is_some()).count();
        keys.insert(at, "length".into());
    }
    it.array(keys.into_iter().map(Val::Str).collect())
}

fn obj_from_entries(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let o = it.object()?;
    for e in it.iterate(&arg(a, 0))? {
        let k = it.get(&e, "0")?;
        let k = it.key_of(&k);
        let k = it.operand_named(k, "pair[0]")?;
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
                it.grow_elements(&arr, 1)?;
                match &mut arr.borrow_mut().kind {
                    Kind::Array(v) => v.push(x),
                    _ => unreachable!("a group made as an array"),
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

/// `Object.prototype.toLocaleString`: `this.toString()`.
fn obj_to_locale_string(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    if this.is_nullish() {
        let m = format!("can't access property \"toString\", O is {}", it.show(this));
        return it.type_error(&m);
    }
    let f = it.get(this, "toString")?;
    it.call(&f, this, &[])
}

pub(crate) fn obj_to_string(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let builtin = builtin_tag(it, this)?;
    if !this.is_nullish()
        && let Val::Str(tag) = it.get(this, &well_known("toStringTag"))?
    {
        return it.string(format!("[object {tag}]"));
    }
    it.string(format!("[object {builtin}]"))
}

/// The tag `Object.prototype.toString` gives `this` when it has no `Symbol.toStringTag`.
fn builtin_tag(it: &mut Interp, this: &Val) -> R<&'static str> {
    if let Val::Obj(o) = this
        && let Some(class) = crate::bson::bson_class(it, o)
    {
        return Ok(class);
    }
    if let Val::Obj(o) = this
        && crate::proxy::is_proxy(o)
    {
        return Ok(if crate::proxy::is_array(it, this)? {
            "Array"
        } else if o.borrow().is_callable() {
            "Function"
        } else {
            "Object"
        });
    }
    Ok(match this {
        Val::Undef | Val::Hole => "Undefined",
        Val::Null => "Null",
        Val::Obj(o) => match &o.borrow().kind {
            Kind::Array(_) => "Array",
            Kind::Func(_) => "Function",
            Kind::Error => "Error",
            Kind::Date(_) => "Date",
            Kind::RegExp(..) => "RegExp",
            Kind::Arguments(_) => "Arguments",
            Kind::BigInt(_) => "BigInt",
            Kind::Str(_) => "String",
            Kind::Num(_) => "Number",
            Kind::Bool(_) => "Boolean",
            _ => "Object",
        },
        Val::Num(_) => "Number",
        Val::BigInt(_) => "BigInt",
        Val::Str(_) => "String",
        Val::Bool(_) => "Boolean",
        Val::Sym(_) => "Symbol",
    })
}

fn obj_value_of(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(Val::Obj(it.object_of(this)?))
}

fn obj_is_prototype_of(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let Val::Obj(o) = arg(a, 0) else { return Ok(Val::Bool(false)) };
    let p = it.object_of(this)?;
    let p = &p;
    let mut cur = it.proto_of(&o)?;
    while let Some(c) = cur {
        if c.ptr_eq(p) {
            return Ok(Val::Bool(true));
        }
        cur = it.proto_of(&c)?;
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
        let get = it.native(&format!("get {name}"), restricted, 0)?;
        let set = it.native(&format!("set {name}"), restricted, 0)?;
        let p = Prop { getter: Some(get), setter: Some(set), ..Prop::hidden(Val::Undef) };
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
    let function = matches!(this, Val::Obj(o)
        if matches!(o.borrow().kind, Kind::Func(Func::Closure { .. } | Func::Native(..))));
    if !function {
        let class = class_name(this);
        let name = it.native.clone();
        return it.type_error(&format!("{name} method called on incompatible {class}"));
    }
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
    let room = it.parse_room();
    let stmts = crate::parser::parse_eval(&src, room).map_err(|e| it.syntax_error(&e))?;
    let env = it.global_env.clone();
    match stmts.first() {
        Some(crate::ast::Stmt::Expr(e)) => it.eval(e, &env),
        _ => it.type_error("bad function"),
    }
}

/// `this` of `Function.prototype.<m>` that cannot be called: the error SpiderMonkey gives.
fn callable_this(it: &Interp, m: &str, this: &Val) -> R<ObjRef> {
    if let Val::Obj(o) = this
        && o.borrow().is_callable()
    {
        return Ok(o.clone());
    }
    let m = format!("Function.prototype.{m} called on incompatible {}", class_name(this));
    Err(it.error("TypeError", &m))
}

fn fn_call(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    callable_this(it, "call", this)?;
    it.call(this, &arg(a, 0), a.get(1..).unwrap_or(&[]))
}

fn fn_apply(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    callable_this(it, "apply", this)?;
    let args = match arg(a, 1) {
        Val::Undef | Val::Null => Vec::new(),
        v => it.iterate(&v)?,
    };
    it.call(this, &arg(a, 0), &args)
}

fn fn_bind(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let target = &callable_this(it, "bind", this)?;
    let bound_args = a.get(1..).unwrap_or(&[]).to_vec();
    // `length` is the target's less the bound arguments, `name` is `bound ` and the target's;
    // reading them does not make the target's lazy ones (measured on a `refrouter` 9.0.4 on 02-11).
    let mut read = |key: &str| -> R {
        let own =
            target.borrow().get_own(key).filter(|p| !p.is_accessor()).map(|p| p.value.clone());
        own.map_or_else(|| it.get(this, key), Ok)
    };
    let length = match read("length")? {
        Val::Num(n) if n.is_nan() => 0.0,
        Val::Num(n) => (n.trunc() - bound_args.len() as f64).max(0.0),
        _ => 0.0,
    };
    let name = match read("name")? {
        Val::Str(s) => format!("bound {s}"),
        _ => "bound ".to_owned(),
    };
    let f = Func::Bound(target.clone(), arg(a, 0), bound_args);
    let o = it.alloc(Some(it.realm.function.clone()), Kind::Func(f))?;
    let fixed = |v: Val| Prop { writable: false, ..Prop::hidden(v) };
    it.define(&o, "length", fixed(Val::Num(length)))?;
    it.define(&o, "name", fixed(Val::str(&name)))?;
    Ok(Val::Obj(o))
}

fn fn_to_string(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let Val::Obj(o) = this else {
        return it.type_error("Function.prototype.toString called on incompatible object");
    };
    let s = match &o.borrow().kind {
        Kind::Func(Func::Closure { class: Some(c), .. }) => c.source.to_string(),
        Kind::Func(Func::Closure { def, .. }) => def.source.to_string(),
        Kind::Func(Func::Native(_, n)) => format!("function {n}() {{\n    [native code]\n}}"),
        // A bound function and a callable proxy (measured on a `refrouter` 9.0.4 on 02-11).
        Kind::Func(Func::Bound(..)) | Kind::Proxy(_, true, _) => {
            "function () {\n    [native code]\n}".to_owned()
        }
        _ => return it.type_error("Function.prototype.toString called on incompatible object"),
    };
    it.string(s)
}

// Array.

fn array_builtins(it: &mut Interp, g: &ObjRef) -> R<()> {
    let proto = it.realm.array.clone();
    let c = ctor(it, g, "Array", array_ctor, 1, &proto)?;
    for (name, f, n) in
        [("isArray", array_is_array as NativeFn, 1), ("from", array_from, 1), ("of", array_of, 0)]
    {
        method(it, &c, name, f, n)?;
    }
    // RefDb's own helpers, which the shell's prelude defines: their places in the order of
    // `Array`'s properties, before the prelude replaces them.
    for name in ["sum", "avg", "contains", "unique", "shuffle", "stdDev"] {
        method(it, &c, name, array_is_array, 1)?;
    }
    crate::arrays::install(it, &proto)
}

fn array_ctor(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    if let [Val::Num(n)] = a {
        if *n < 0.0 || n.fract() != 0.0 || *n > f64::from(u32::MAX) {
            return Err(it.error("RangeError", "invalid array length"));
        }
        let n = *n as usize;
        it.reserve_elements(n)?;
        return it.array(vec![Val::Hole; n]);
    }
    it.array(a.to_vec())
}

fn array_is_array(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    Ok(Val::Bool(crate::proxy::is_array(it, &arg(a, 0))?))
}

fn array_from(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let src = arg(a, 0);
    let mut target = None;
    let items = match &src {
        Val::Obj(o)
            if !matches!(
                o.borrow().kind,
                Kind::Array(_) | Kind::Set(_) | Kind::Map(_) | Kind::Iter(..) | Kind::Arguments(_)
            ) && !it.get(&src, "@@Symbol.iterator:0")?.is_callable() =>
        {
            let n = it.get(&src, "length")?;
            let n = it.number_of(&n);
            let n = it.operand_named(n, "arrayLike.length")?;
            if n > f64::from(u32::MAX) {
                return Err(it.error("RangeError", "invalid array length"));
            }
            let n = if n > 0.0 { n as usize } else { 0 };
            it.reserve_elements(n)?;
            target = crate::arrays::made_by(it, this, &[Val::Num(n as f64)])?;
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                out.push(it.get(&src, &i.to_string())?);
            }
            out
        }
        Val::Undef | Val::Null => {
            return it.type_error(&format!("{} is not iterable", it.show(&src)));
        }
        // A number, boolean, symbol or bigint has no iterator and no length: no elements
        // (measured on a `refrouter` 9.0.4 on 02-11: `Set.tojson(5)` is `new Set([ ])`).
        Val::Num(_) | Val::Bool(_) | Val::Sym(_) | Val::BigInt(_) => Vec::new(),
        _ => {
            target = crate::arrays::made_by(it, this, &[])?;
            it.iterate(&src)?
        }
    };
    let f = arg(a, 1);
    if f.is_callable() {
        let mut out = Vec::with_capacity(items.len());
        for (i, x) in items.into_iter().enumerate() {
            out.push(it.call(&f, &Val::Undef, &[x, Val::Num(i as f64)])?);
        }
        return crate::arrays::species_result(it, target, out, true);
    }
    crate::arrays::species_result(it, target, items, true)
}

fn array_of(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let target = crate::arrays::made_by(it, this, &[Val::Num(a.len() as f64)])?;
    crate::arrays::species_result(it, target, a.to_vec(), true)
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

/// The native errors and their constructors: each makes its own kind called without `new`.
const ERRORS: [(&str, NativeFn); 7] = [
    ("TypeError", |it, t, a| error_of_kind(it, t, a, "TypeError")),
    ("RangeError", |it, t, a| error_of_kind(it, t, a, "RangeError")),
    ("SyntaxError", |it, t, a| error_of_kind(it, t, a, "SyntaxError")),
    ("ReferenceError", |it, t, a| error_of_kind(it, t, a, "ReferenceError")),
    ("EvalError", |it, t, a| error_of_kind(it, t, a, "EvalError")),
    ("URIError", |it, t, a| error_of_kind(it, t, a, "URIError")),
    ("InternalError", |it, t, a| error_of_kind(it, t, a, "InternalError")),
];

/// `XError(…)` without `new`: an error with the prototype of the global `XError`.
fn error_of_kind(it: &mut Interp, this: &Val, a: &[Val], kind: &str) -> R {
    if new_target(this).is_some() {
        return error_ctor(it, this, a);
    }
    let c = it.realm.errors.iter().find(|(n, _)| &**n == kind).map(|(_, c)| c.clone());
    let proto = match c {
        Some(c) => it.get(&Val::Obj(c), "prototype")?.as_obj().cloned(),
        None => Some(it.realm.error.clone()),
    };
    make_error_object(it, proto, a)
}

fn errors(it: &mut Interp, g: &ObjRef) -> R<()> {
    let proto = it.realm.error.clone();
    let base = ctor(it, g, "Error", error_ctor, 1, &proto)?;
    it.define(&proto, "name", Prop::hidden(Val::str("Error")))?;
    it.define(&proto, "message", Prop::hidden(Val::str("")))?;
    method(it, &proto, "toString", error_to_string, 0)?;
    let get = it.native("get stack", stack_getter, 0)?;
    let set = it.native("set stack", stack_setter, 1)?;
    let p = Prop { getter: Some(get), setter: Some(set), ..Prop::hidden(Val::Undef) };
    it.define(&proto, "stack", p)?;
    method(it, &base, "isError", |_, _, a| Ok(Val::Bool(is_error(&arg(a, 0)))), 1)?;
    method(it, &base, "captureStackTrace", capture_stack_trace, 2)?;
    it.realm.errors.push(("Error".into(), base.clone()));
    for (name, f) in ERRORS {
        let p = it.alloc(Some(proto.clone()), Kind::Ordinary)?;
        it.define(&p, "name", Prop::hidden(Val::str(name)))?;
        it.define(&p, "message", Prop::hidden(Val::str("")))?;
        let c = ctor(it, g, name, f, 1, &p)?;
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
    make_error_object(it, proto, a)
}

/// An error with `proto` from the arguments of its constructor.
fn make_error_object(it: &mut Interp, proto: Option<ObjRef>, a: &[Val]) -> R {
    let o = it.alloc(proto, Kind::Error)?;
    it.place_error(&o)?;
    // SpiderMonkey's `Error(message, fileName, lineNumber)`: a second argument that is not an
    // options object is the file, a third the line, and then the column is 1 (measured).
    if a.len() > 1 && !matches!(a[1], Val::Obj(_)) {
        let f = it.str_of(&a[1])?;
        it.define(&o, "fileName", Prop::hidden(Val::Str(f)))?;
    }
    if a.len() > 2 && !matches!(a[1], Val::Obj(_)) {
        let n = it.number_of(&a[2])?;
        it.define(&o, "lineNumber", Prop::hidden(Val::Num(f64::from(number::to_int32(n)))))?;
        it.define(&o, "columnNumber", Prop::hidden(Val::Num(1.0)))?;
    }
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
    Ok(Val::Obj(o))
}

/// `Error.isError(v)`: whether `v` is an error object (made by an error constructor).
fn is_error(v: &Val) -> bool {
    matches!(v, Val::Obj(o) if matches!(o.borrow().kind, Kind::Error))
}

/// `Error.captureStackTrace(target, fn)`: gives `target` an own `stack` with the calls in
/// progress; with `fn`, the frames from the newest down to `fn`'s are left out, and none remain
/// when `fn` is not running (measured on a `refrouter` 9.0.4 on 02-11).
fn capture_stack_trace(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let Some(target) = a.first() else {
        return it.type_error(
            "Error.captureStackTrace: At least 1 argument required, but only 0 passed",
        );
    };
    let Val::Obj(o) = target else {
        let shown = match target {
            Val::Num(_) => format!("the number {}", it.show(target)),
            Val::Str(_) => format!("the string {}", it.show(target)),
            _ => it.show(target),
        };
        return it.type_error(&format!(
            "`target` argument of Error.captureStackTrace must be an object, got {shown}"
        ));
    };
    let mut stack = it.stack_text();
    let until = arg(a, 1);
    if until.is_callable() {
        let name = it.get(&until, "name")?;
        let name = it.str_of(&name)?;
        let marker = format!("{name}@");
        stack = match stack.lines().position(|l| l.starts_with(&marker)) {
            Some(i) => stack.lines().skip(i + 1).map(|l| format!("{l}\n")).collect(),
            None => String::new(),
        };
    }
    it.define(o, "stack", Prop::data(Val::Str(stack.into())))?;
    Ok(Val::Undef)
}

/// `Error.prototype.stack`: the stack of the error, kept when it was made.
/// An object that is not an error reads the stack of the first error of its prototypes, and an
/// empty one when it reaches `Error.prototype` (measured on a `refrouter` 9.0.4).
fn stack_getter(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let Val::Obj(o) = this else {
        return it
            .type_error(&format!("get stack method called on incompatible {}", class_name(this)));
    };
    let mut cur = Some(o.clone());
    while let Some(c) = cur {
        if matches!(c.borrow().kind, Kind::Error) {
            return Ok(c.borrow().get_own("@@stack").map_or(Val::Undef, |p| p.value.clone()));
        }
        if c.ptr_eq(&it.realm.error) {
            return Ok(Val::str(""));
        }
        cur = c.borrow().proto.clone();
    }
    it.type_error(&format!(
        "Error.prototype.(get stack) called on incompatible {}",
        class_name(this)
    ))
}

/// Setting `stack` gives the object its own `stack`.
fn stack_setter(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let Val::Obj(o) = this else {
        return it
            .type_error(&format!("set stack method called on incompatible {}", class_name(this)));
    };
    it.define(o, "stack", Prop::data(arg(a, 0)))?;
    Ok(Val::Undef)
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

/// A method of `Map.prototype` or `Set.prototype` that first checks that `this` is one of them:
/// `entries method called on incompatible Object` (measured on a `refrouter` 9.0.4 on 02-11).
macro_rules! branded {
    ($map:expr, $f:path) => {
        |it: &mut Interp, t: &Val, a: &[Val]| {
            brand(it, t, $map)?;
            $f(it, t, a)
        }
    };
}

fn brand(it: &mut Interp, this: &Val, map: bool) -> R<()> {
    let ok = matches!(this, Val::Obj(o) if match o.borrow().kind {
        Kind::Map(_) => map,
        Kind::Set(_) => !map,
        _ => false,
    });
    if ok {
        return Ok(());
    }
    let name = it.native.clone();
    let name = if &*name == "[Symbol.iterator]" {
        if map { "entries".into() } else { "values".into() }
    } else {
        name
    };
    it.type_error(&format!("{name} method called on incompatible {}", class_name(this)))
}

fn collections(it: &mut Interp, g: &ObjRef) -> R<()> {
    let mp = it.realm.map.clone();
    ctor(it, g, "Map", map_ctor, 0, &mp)?;
    for (name, f, n) in [
        ("get", branded!(true, map_get) as NativeFn, 1),
        ("set", branded!(true, map_set), 2),
        ("has", branded!(true, map_has), 1),
        ("delete", branded!(true, map_delete), 1),
        ("clear", branded!(true, coll_clear), 0),
        ("forEach", branded!(true, coll_for_each), 1),
        ("keys", branded!(true, map_keys), 0),
        ("values", branded!(true, map_values), 0),
        ("entries", branded!(true, coll_entries), 0),
    ] {
        method(it, &mp, name, f, n)?;
    }
    it.define(
        &mp,
        "size",
        Prop {
            getter: Some(it.native("get size", branded!(true, coll_size), 0)?),
            ..Prop::hidden(Val::Undef)
        },
    )?;
    let sp = it.realm.set.clone();
    ctor(it, g, "Set", set_ctor, 0, &sp)?;
    for (name, f, n) in [
        ("add", branded!(false, set_add) as NativeFn, 1),
        ("has", branded!(false, map_has), 1),
        ("delete", branded!(false, map_delete), 1),
        ("clear", branded!(false, coll_clear), 0),
        ("forEach", branded!(false, coll_for_each), 1),
        ("values", branded!(false, map_values), 0),
        ("entries", branded!(false, coll_entries), 0),
    ] {
        method(it, &sp, name, f, n)?;
    }
    // `Set.prototype.keys` is the same function as `values` (ECMA-262 §24.2.4.17).
    let values = sp.borrow().get_own("values").map(|p| p.value.clone()).unwrap_or(Val::Undef);
    it.define(&sp, "keys", Prop::hidden(values))?;
    it.define(
        &sp,
        "size",
        Prop {
            getter: Some(it.native("get size", branded!(false, coll_size), 0)?),
            ..Prop::hidden(Val::Undef)
        },
    )?;
    // `Map.prototype[Symbol.iterator]` is `entries` and `Set.prototype[Symbol.iterator]` is
    // `values`, the same functions (ECMA-262 §24.1.3.12, §24.2.3.11).
    for (o, name) in [(&mp, "entries"), (&sp, "values")] {
        let f = o.borrow().get_own(name).map(|p| p.value.clone()).unwrap_or(Val::Undef);
        it.define(o, "@@Symbol.iterator:0", Prop::hidden(f))?;
    }
    let ip = it.realm.iter.clone();
    let f = it.native("[Symbol.iterator]", iter_self, 0)?;
    it.define(&ip, "@@Symbol.iterator:0", Prop::hidden(Val::Obj(f)))?;
    // Each kind of iterator has its prototype, with `next` and its tag, over
    // `Iterator.prototype` (which has no `next`).
    for (name, next) in ITERATOR_KINDS {
        let p = it.alloc(Some(ip.clone()), Kind::Ordinary)?;
        method(it, &p, "next", next, 0)?;
        to_string_tag(it, &p, name)?;
        it.realm.protos.push((name, p));
    }
    Ok(())
}

/// Adds what `iterable` gives to the new collection `target` with `adder`, one value at a time:
/// each an entry `[key, value]` when `entries`. An error closes the iterator (ECMA-262 §24.1.1.2).
pub(crate) fn add_from(
    it: &mut Interp,
    target: &Val,
    iterable: &Val,
    adder: NativeFn,
    entries: Option<&str>,
) -> R<()> {
    if iterable.is_nullish() {
        return Ok(());
    }
    let mut rec = it.open_iter(iterable)?;
    while let Some(item) = it.iter_step(&mut rec)? {
        let r = if let Some(kind) = entries.filter(|_| !matches!(item, Val::Obj(_))) {
            it.type_error(&format!("iterable for {kind} should have array-like objects"))
        } else if entries.is_none() {
            adder(it, target, &[item])
        } else {
            it.get(&item, "0")
                .and_then(|k| Ok((k, it.get(&item, "1")?)))
                .and_then(|(k, v)| adder(it, target, &[k, v]))
        };
        if let Err(e) = r {
            // Closing keeps the first error; only a fatal stop while closing replaces it.
            if matches!(e, Ctrl::Throw(_))
                && let Err(fatal @ Ctrl::Fatal(_)) = it.iter_close(&rec)
            {
                return Err(fatal);
            }
            return Err(e);
        }
    }
    Ok(())
}

fn map_ctor(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    if new_target(this).is_none() {
        return it.type_error("calling a builtin Map constructor without new is forbidden");
    }
    let m = Val::Obj(it.alloc(Some(it.realm.map.clone()), Kind::Map(Table::default()))?);
    add_from(it, &m, &arg(a, 0), map_set, Some("Map"))?;
    Ok(m)
}

fn set_ctor(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    if new_target(this).is_none() {
        return it.type_error("calling a builtin Set constructor without new is forbidden");
    }
    let s = it.alloc(Some(it.realm.set.clone()), Kind::Set(Table::default()))?;
    add_from(it, &Val::Obj(s.clone()), &arg(a, 0), set_add, None)?;
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
        Kind::Map(e) => e.get(&k).map_or(Val::Undef, |(_, v)| v.clone()),
        _ => Val::Undef,
    })
}

/// The entries of `o`, a `Map` that `set` checked.
fn map_table(o: &ObjRef) -> std::cell::RefMut<'_, Table<(Val, Val)>> {
    std::cell::RefMut::map(o.borrow_mut(), |b| match &mut b.kind {
        Kind::Map(e) => e,
        _ => unreachable!("`set` checked a Map"),
    })
}

/// The values of `o`, a `Set` that `add` checked.
fn set_table(o: &ObjRef) -> std::cell::RefMut<'_, Table<Val>> {
    std::cell::RefMut::map(o.borrow_mut(), |b| match &mut b.kind {
        Kind::Set(e) => e,
        _ => unreachable!("`add` checked a Set"),
    })
}

fn map_set(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = coll(it, this)?;
    let k = norm(arg(a, 0));
    if let Some(slot) = map_table(&o).get_mut(&k) {
        slot.1 = arg(a, 1);
        return Ok(this.clone());
    }
    it.grow_elements(&o, 2)?;
    map_table(&o).push((k, arg(a, 1)));
    Ok(this.clone())
}

fn set_add(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = coll(it, this)?;
    let k = norm(arg(a, 0));
    if set_table(&o).contains(&k) {
        return Ok(this.clone());
    }
    it.grow_elements(&o, 1)?;
    set_table(&o).push(k);
    Ok(this.clone())
}

fn map_has(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = coll(it, this)?;
    let k = arg(a, 0);
    Ok(Val::Bool(match &o.borrow().kind {
        Kind::Map(e) => e.contains(&k),
        Kind::Set(e) => e.contains(&k),
        _ => false,
    }))
}

fn map_delete(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = coll(it, this)?;
    let k = arg(a, 0);
    Ok(Val::Bool(match &mut o.borrow_mut().kind {
        Kind::Map(e) => e.remove(&k),
        Kind::Set(e) => e.remove(&k),
        _ => false,
    }))
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

/// A cursor at the first entry of the `Map` or `Set` `o`.
pub(crate) fn coll_cursor(o: &ObjRef) -> Cursor {
    match &mut o.borrow_mut().kind {
        Kind::Map(e) => e.cursor(),
        Kind::Set(e) => e.cursor(),
        _ => Cursor::default(),
    }
}

/// The entry of the `Map` or `Set` `o` at or after `cursor` (a `Set`'s value twice), which then
/// points past it.
pub(crate) fn coll_next(o: &ObjRef, cursor: &Cursor) -> Option<(Val, Val)> {
    match &o.borrow().kind {
        Kind::Map(e) => e.next(cursor),
        Kind::Set(e) => e.next(cursor).map(|v| (v.clone(), v)),
        _ => None,
    }
}

fn coll_for_each(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = coll(it, this)?;
    let f = arg(a, 0);
    if a.is_empty() {
        // Measured on a `refrouter` 9.0.4 on 03-11.
        return it.type_error("undefined is not a function");
    }
    if !f.is_callable() {
        return Err(crate::arrays::not_a_function(it, &f));
    }
    let cursor = coll_cursor(&o);
    while let Some((k, v)) = coll_next(&o, &cursor) {
        it.call(&f, &arg(a, 1), &[v, k, this.clone()])?;
    }
    Ok(Val::Undef)
}

/// The kinds of iterator the built-ins make, by their `Symbol.toStringTag`.
pub(crate) const ARRAY_ITERATOR: &str = "Array Iterator";
pub(crate) const MAP_ITERATOR: &str = "Map Iterator";
pub(crate) const SET_ITERATOR: &str = "Set Iterator";
pub(crate) const STRING_ITERATOR: &str = "String Iterator";
pub(crate) const REGEXP_STRING_ITERATOR: &str = "RegExp String Iterator";
/// Each kind with its `next`, which refuses an iterator of another kind (measured on a
/// `refrouter` 9.0.4 on 02-11: `next method called on incompatible Map Iterator`).
const ITERATOR_KINDS: [(&str, NativeFn); 5] = [
    (ARRAY_ITERATOR, |it, this, _| iter_next(it, this, ARRAY_ITERATOR)),
    (MAP_ITERATOR, |it, this, _| iter_next(it, this, MAP_ITERATOR)),
    (SET_ITERATOR, |it, this, _| iter_next(it, this, SET_ITERATOR)),
    (STRING_ITERATOR, |it, this, _| iter_next(it, this, STRING_ITERATOR)),
    (REGEXP_STRING_ITERATOR, |it, this, _| iter_next(it, this, REGEXP_STRING_ITERATOR)),
];

/// An iterator of kind `kind` over `items`.
pub(crate) fn make_iter(it: &Interp, kind: &'static str, items: Vec<Val>) -> R {
    let state = Rc::new(std::cell::RefCell::new((items, 0usize)));
    Ok(Val::Obj(it.alloc(Some(it.realm.proto(kind)), Kind::Iter(state, kind))?))
}

/// The state of an iteration of the `Map` or `Set` `o` that gives `kind`.
pub(crate) fn coll_iter(o: &ObjRef, kind: ArrayIterKind) -> Rc<std::cell::RefCell<CollIter>> {
    let class = if matches!(o.borrow().kind, Kind::Set(_)) { SET_ITERATOR } else { MAP_ITERATOR };
    let cursor = coll_cursor(o);
    Rc::new(std::cell::RefCell::new(CollIter { source: Some(o.clone()), cursor, kind, class }))
}

/// An iterator of `this`, a `Map` or a `Set`, that gives `kind`.
fn coll_iter_obj(it: &mut Interp, this: &Val, kind: ArrayIterKind) -> R {
    let o = coll(it, this)?;
    let state = coll_iter(&o, kind);
    let class = state.borrow().class;
    Ok(Val::Obj(it.alloc(Some(it.realm.proto(class)), Kind::CollIter(state))?))
}

/// The next value of an iteration of a `Map` or a `Set`; once done, it stays done.
pub(crate) fn coll_iter_next(
    it: &mut Interp,
    state: &Rc<std::cell::RefCell<CollIter>>,
) -> R<Option<Val>> {
    let (source, cursor, kind) = {
        let s = state.borrow();
        let Some(source) = s.source.clone() else { return Ok(None) };
        (source, s.cursor.clone(), s.kind)
    };
    let Some((k, v)) = coll_next(&source, &cursor) else {
        state.borrow_mut().source = None;
        return Ok(None);
    };
    Ok(Some(match kind {
        ArrayIterKind::Keys => k,
        ArrayIterKind::Values => v,
        ArrayIterKind::Entries => it.array(vec![k, v])?,
    }))
}

fn map_keys(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    coll_iter_obj(it, this, ArrayIterKind::Keys)
}

fn map_values(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    coll_iter_obj(it, this, ArrayIterKind::Values)
}

fn coll_entries(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    coll_iter_obj(it, this, ArrayIterKind::Entries)
}

/// `next` of an iterator of the kind `class`.
fn iter_next(it: &mut Interp, this: &Val, class: &str) -> R {
    let kind = match this {
        Val::Obj(o) if class_kind(this) == class => o.borrow().kind.clone(),
        _ => {
            let m = format!("next method called on incompatible {}", class_name(this));
            return it.type_error(&m);
        }
    };
    let state = match kind {
        Kind::ArrayIter(state) => {
            let v = crate::arrays::array_iter_next(it, &state)?;
            let done = v.is_none();
            return iter_result(it, v.unwrap_or(Val::Undef), done);
        }
        Kind::CollIter(state) => {
            let v = coll_iter_next(it, &state)?;
            let done = v.is_none();
            return iter_result(it, v.unwrap_or(Val::Undef), done);
        }
        Kind::Iter(state, _) => state,
        _ => return it.type_error("next method called on incompatible object"),
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
    iter_result(it, v, done)
}

/// `{value, done}`.
fn iter_result(it: &mut Interp, v: Val, done: bool) -> R {
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
        to_string_tag(it, &installed(g, name), name)?;
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
    make_iter(
        it,
        STRING_ITERATOR,
        crate::text::code_points(&s).map(|c| Val::Str(c.into())).collect(),
    )
}

// RegExp: the literal and constructor keep the source and flags; matching is in `strings`.

fn regexp_builtins(it: &mut Interp, g: &ObjRef) -> R<()> {
    let proto = it.realm.regexp.clone();
    ctor(it, g, "RegExp", regexp_ctor, 2, &proto)?;
    method(it, &proto, "test", crate::regex::test, 1)?;
    method(it, &proto, "exec", crate::regex::exec, 1)?;
    method(it, &proto, "toString", regexp_to_string, 0)?;
    method(it, &proto, "compile", regexp_compile, 2)?;
    let symbols: [(&str, NativeFn, usize); 5] = [
        ("match", crate::regex::symbol_match, 1),
        ("matchAll", crate::regex::symbol_match_all, 1),
        ("replace", crate::regex::symbol_replace, 2),
        ("search", crate::regex::symbol_search, 1),
        ("split", crate::regex::symbol_split, 2),
    ];
    for (name, f, n) in symbols {
        let fo = it.native(&format!("[Symbol.{name}]"), f, n)?;
        it.define(&proto, &well_known(name), Prop::hidden(Val::Obj(fo)))?;
    }
    let getter = it.native("get source", regexp_source, 0)?;
    it.define(&proto, "source", Prop { getter: Some(getter), ..Prop::hidden(Val::Undef) })?;
    let getter = it.native("get flags", regexp_flags, 0)?;
    it.define(&proto, "flags", Prop { getter: Some(getter), ..Prop::hidden(Val::Undef) })?;
    for (name, _) in REGEXP_FLAGS {
        let getter = it.native(&format!("get {name}"), regexp_flag, 0)?;
        it.define(&proto, name, Prop { getter: Some(getter), ..Prop::hidden(Val::Undef) })?;
    }
    {
        let c = installed(g, "RegExp");
        method(it, &c, "escape", regexp_escape, 1)?;
        let names = [
            "input",
            "$_",
            "lastMatch",
            "$&",
            "lastParen",
            "$+",
            "leftContext",
            "$`",
            "rightContext",
            "$'",
            "$1",
            "$2",
            "$3",
            "$4",
            "$5",
            "$6",
            "$7",
            "$8",
            "$9",
        ];
        for name in names {
            let get = it.native(name, crate::regex::legacy_static, 0)?;
            let set = if matches!(name, "input" | "$_") {
                Some(it.native(name, crate::regex::legacy_input_setter, 1)?)
            } else {
                None
            };
            // The long names and `$1`…`$9` are enumerable, the other short ones are not.
            let enumerable = !name.starts_with('$') || name[1..].parse::<u8>().is_ok();
            let p = Prop { getter: Some(get), setter: set, enumerable, ..Prop::hidden(Val::Undef) };
            it.define(&c, name, p)?;
        }
    }
    Ok(())
}

/// The flag accessors of `RegExp.prototype`, in the order `flags` reads them.
const REGEXP_FLAGS: [(&str, char); 8] = [
    ("hasIndices", 'd'),
    ("global", 'g'),
    ("ignoreCase", 'i'),
    ("multiline", 'm'),
    ("dotAll", 's'),
    ("unicode", 'u'),
    ("unicodeSets", 'v'),
    ("sticky", 'y'),
];

/// `this` of a `RegExp.prototype` accessor: the pattern and flags of a regular expression,
/// `None` for `RegExp.prototype` itself, an error otherwise (measured on a `refrouter` 9.0.4 on
/// 02-11: `RegExp.prototype.global getter called on non-RegExp object: Object`).
fn regexp_this(it: &mut Interp, this: &Val) -> R<Option<(Name, Name)>> {
    if let Val::Obj(o) = this {
        if let Kind::RegExp(s, f) = &o.borrow().kind {
            return Ok(Some((s.clone(), f.clone())));
        }
        if o.ptr_eq(&it.realm.regexp) {
            return Ok(None);
        }
    }
    let native = it.native.clone();
    let name = native.strip_prefix("get ").unwrap_or(&native);
    let class = match this {
        Val::Obj(_) => class_name(this),
        v => v.type_of().to_string(),
    };
    it.type_error(&format!("RegExp.prototype.{name} getter called on non-RegExp object: {class}"))
}

fn regexp_flag(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let native = it.native.clone();
    let name = native.strip_prefix("get ").unwrap_or(&native);
    let flag = REGEXP_FLAGS.iter().find(|(n, _)| **n == *name).map_or('g', |(_, c)| *c);
    Ok(match regexp_this(it, this)? {
        Some((_, f)) => Val::Bool(f.contains(flag)),
        None => Val::Undef,
    })
}

fn regexp_source(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(match regexp_this(it, this)? {
        Some((s, _)) => Val::Str(s),
        None => Val::str("(?:)"),
    })
}

/// `flags`: the flags read through their accessors (ECMA-262 §22.2.6.4).
fn regexp_flags(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    if !matches!(this, Val::Obj(_)) {
        return it.type_error(&format!("{} is not a non-null object", this.type_of()));
    }
    let mut out = String::new();
    for (name, c) in REGEXP_FLAGS {
        if it.get(this, name)?.truthy() {
            out.push(c);
        }
    }
    it.string(out)
}

/// `toString`: `/source/flags`, read from any object (ECMA-262 §22.2.6.17).
pub(crate) fn regexp_to_string(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    if !matches!(this, Val::Obj(_)) {
        return it.type_error(&format!("{} is not a non-null object", this.type_of()));
    }
    let source = it.get(this, "source")?;
    let source = it.str_of(&source)?;
    let flags = it.get(this, "flags")?;
    let flags = it.str_of(&flags)?;
    it.string(format!("/{source}/{flags}"))
}

/// `compile(pattern, flags)` of Annex B: `this` takes the new pattern and `lastIndex` goes to 0.
fn regexp_compile(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = match this {
        Val::Obj(o) if matches!(o.borrow().kind, Kind::RegExp(..)) => o,
        _ => {
            let m = format!("compile method called on incompatible {}", class_name(this));
            return it.type_error(&m);
        }
    };
    let pattern = arg(a, 0);
    let flags = arg(a, 1);
    let from = match &pattern {
        Val::Obj(p) => match &p.borrow().kind {
            Kind::RegExp(s, f) => Some((s.clone(), f.clone())),
            _ => None,
        },
        _ => None,
    };
    let (source, flags) = match from {
        Some(parts) if matches!(flags, Val::Undef) => parts,
        Some(_) => {
            return it.type_error("can't supply flags when constructing one RegExp from another");
        }
        None => {
            let s = if matches!(pattern, Val::Undef) { "".into() } else { it.str_of(&pattern)? };
            let f = if matches!(flags, Val::Undef) { "".into() } else { it.str_of(&flags)? };
            (s, f)
        }
    };
    crate::regex::check(it, &source, &flags)?;
    o.borrow_mut().kind = Kind::RegExp(escape_pattern(source), flags);
    it.set(this, "lastIndex", Val::Num(0.0))?;
    Ok(this.clone())
}

/// `RegExp.escape(text)` (ECMA-262 §22.2.5.1): a pattern that matches `text` literally.
fn regexp_escape(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let Val::Str(s) = arg(a, 0) else {
        let v = arg(a, 0);
        return it.type_error(&format!("{} is not a string", it.show(&v)));
    };
    let mut out = String::new();
    for (i, u) in crate::strings::units(&s).into_iter().enumerate() {
        let c = char::from_u32(u32::from(u));
        match c {
            Some(c) if i == 0 && c.is_ascii_alphanumeric() => out.push_str(&format!("\\x{u:02x}")),
            Some(c) if "^$\\.*+?()[]{}|/".contains(c) => {
                out.push('\\');
                out.push(c);
            }
            Some(c) if ",-=<>#&!%:;@~'`\"".contains(c) => out.push_str(&format!("\\x{u:02x}")),
            Some('\t') => out.push_str("\\t"),
            Some('\n') => out.push_str("\\n"),
            Some('\u{b}') => out.push_str("\\v"),
            Some('\u{c}') => out.push_str("\\f"),
            Some('\r') => out.push_str("\\r"),
            Some(' ') => out.push_str("\\x20"),
            Some(c) if c.is_whitespace() || c == '\u{feff}' => {
                out.push_str(&format!("\\u{u:04x}"));
            }
            Some(c) => out.push(c),
            None => out.push_str(&format!("\\u{u:04x}")),
        }
    }
    it.string(crate::text::encode(&out).into_owned())
}

pub fn make_regexp(it: &mut Interp, source: Name, flags: Name) -> R {
    crate::regex::check(it, &source, &flags)?;
    let source = escape_pattern(source);
    let o = it.alloc(Some(it.realm.regexp.clone()), Kind::RegExp(source, flags))?;
    it.define(&o, "lastIndex", Prop::hidden(Val::Num(0.0)))?;
    Ok(Val::Obj(o))
}

/// The pattern as `source` gives it (ECMA-262 §22.2.6.13.1): `(?:)` for an empty one, `/` and
/// line terminators escaped.
fn escape_pattern(source: Name) -> Name {
    if source.is_empty() {
        return "(?:)".into();
    }
    if !source.contains(['/', '\n', '\r', '\u{2028}', '\u{2029}']) {
        return source;
    }
    let mut out = String::with_capacity(source.len() + 2);
    let (mut escaped, mut class) = (false, false);
    for c in source.chars() {
        match c {
            '/' if !escaped && !class => out.push_str("\\/"),
            '\n' => out.push_str(if escaped { "n" } else { "\\n" }),
            '\r' => out.push_str(if escaped { "r" } else { "\\r" }),
            '\u{2028}' => out.push_str(if escaped { "u2028" } else { "\\u2028" }),
            '\u{2029}' => out.push_str(if escaped { "u2029" } else { "\\u2029" }),
            c => out.push(c),
        }
        if !escaped {
            match c {
                '[' => class = true,
                ']' => class = false,
                _ => {}
            }
        }
        escaped = !escaped && c == '\\';
    }
    out.into()
}

fn regexp_ctor(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let pattern = arg(a, 0);
    let parts = match &pattern {
        Val::Obj(o) => match &o.borrow().kind {
            Kind::RegExp(s, f) => Some((s.clone(), f.clone())),
            _ => None,
        },
        _ => None,
    };
    let (src, flags) = match (parts, pattern) {
        (Some(p), _) => p,
        (None, Val::Undef) => ("(?:)".into(), "".into()),
        (None, v) => (it.str_of(&v)?, "".into()),
    };
    let flags = if matches!(arg(a, 1), Val::Undef) { flags } else { it.str_of(&arg(a, 1))? };
    make_regexp(it, src, flags)
}

/// `__defineGetter__(key, f)` and `__defineSetter__` of Annex B: an enumerable, configurable
/// accessor (`invalid getter usage` when `f` is not a function, measured on a `refrouter` 9.0.4).
fn define_legacy_accessor(it: &mut Interp, this: &Val, a: &[Val], getter: bool) -> R {
    let o = it.object_of(this)?;
    let f = arg(a, 1);
    let Val::Obj(fo) = &f else {
        return it.type_error(if getter { "invalid getter usage" } else { "invalid setter usage" });
    };
    if !f.is_callable() {
        return it.type_error(if getter { "invalid getter usage" } else { "invalid setter usage" });
    }
    let key = it.key_of(&arg(a, 0))?;
    let d = crate::props::Desc {
        get: getter.then(|| Some(fo.clone())),
        set: (!getter).then(|| Some(fo.clone())),
        enumerable: Some(true),
        configurable: Some(true),
        ..crate::props::Desc::default()
    };
    crate::props::define_own(it, &o, &key, &d)?;
    Ok(Val::Undef)
}

/// `__lookupGetter__(key)` and `__lookupSetter__`: the accessor of `key` on the object or the
/// nearest prototype that has the property.
fn lookup_legacy_accessor(it: &mut Interp, this: &Val, a: &[Val], getter: bool) -> R {
    let mut o = Some(it.object_of(this)?);
    let key = it.key_of(&arg(a, 0))?;
    while let Some(cur) = o {
        if let Some(p) = crate::props::own_prop(&cur, &key) {
            let f = if getter { p.getter } else { p.setter };
            return Ok(f.map_or(Val::Undef, Val::Obj));
        }
        o = cur.borrow().proto.clone();
    }
    Ok(Val::Undef)
}

/// The error a native function gives for `this` of the wrong kind.
pub fn incompatible(it: &Interp, what: &str) -> Ctrl {
    it.error("TypeError", &format!("{what} called on incompatible object"))
}
