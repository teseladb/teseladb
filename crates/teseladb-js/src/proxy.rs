//! `Proxy`: an object whose operations go to the traps of its handler, or to its target when
//! the handler has no trap for them, with SpiderMonkey's messages (measured on a `refrouter`
//! 9.0.4 on 02-11). A revoked proxy fails every operation.

use crate::builtins::{arg, ctor, method, new_target};
use crate::interp::{Interp, R};
use crate::value::*;

const REVOKED: &str = "illegal operation attempted on a revoked proxy";

/// The target and handler of `o` when it is a proxy; `Some(None)` when it was revoked.
pub fn parts(o: &ObjRef) -> Option<Option<(ObjRef, ObjRef)>> {
    match &o.borrow().kind {
        Kind::Proxy(p, ..) => Some(p.clone()),
        _ => None,
    }
}

pub fn is_proxy(o: &ObjRef) -> bool {
    matches!(o.borrow().kind, Kind::Proxy(..))
}

fn live(it: &mut Interp, o: &ObjRef) -> R<(ObjRef, ObjRef)> {
    match parts(o) {
        Some(Some(p)) => Ok(p),
        _ => it.type_error(REVOKED),
    }
}

/// The trap `name` of `handler`, `None` when it is undefined or null.
fn trap(it: &mut Interp, handler: &ObjRef, name: &str) -> R<Option<Val>> {
    let t = it.get(&Val::Obj(handler.clone()), name)?;
    if t.is_nullish() {
        return Ok(None);
    }
    if !t.is_callable() {
        return it.type_error(&format!(
            "proxy handler's {name} trap wasn't undefined, null, or callable"
        ));
    }
    Ok(Some(t))
}

/// The key a trap receives: a symbol for a symbol key, a string otherwise.
fn same_proto(a: &Option<ObjRef>, b: &Option<ObjRef>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x.ptr_eq(y),
        (None, None) => true,
        _ => false,
    }
}

/// `[[GetPrototypeOf]]` of a proxy: its `getPrototypeOf` trap, or its target's (ECMA-262
/// §10.5.1; messages measured on a `refrouter` 9.0.4 on 02-11).
pub fn get_proto(it: &mut Interp, o: &ObjRef) -> R<Option<ObjRef>> {
    let (target, handler) = live(it, o)?;
    let Some(t) = trap(it, &handler, "getPrototypeOf")? else { return it.proto_of(&target) };
    let p = match it.call_back(
        crate::recursion::PROXY_TRAP,
        &t,
        &Val::Obj(handler),
        &[Val::Obj(target.clone())],
    )? {
        Val::Obj(p) => Some(p),
        Val::Null => None,
        _ => {
            return it
                .type_error("proxy getPrototypeOf handler returned a non-object, non-null value");
        }
    };
    if !target.borrow().extensible && !same_proto(&it.proto_of(&target)?, &p) {
        return it.type_error(
            "proxy getPrototypeOf handler didn't return the target object's prototype",
        );
    }
    Ok(p)
}

/// `[[SetPrototypeOf]]` of a proxy: its `setPrototypeOf` trap, or its target's (ECMA-262
/// §10.5.2).
pub fn set_proto(it: &mut Interp, o: &ObjRef, p: Option<ObjRef>) -> R<()> {
    let (target, handler) = live(it, o)?;
    let Some(t) = trap(it, &handler, "setPrototypeOf")? else {
        return crate::builtins::set_proto_of(it, &target, p);
    };
    let arg = p.clone().map_or(Val::Null, Val::Obj);
    if !it
        .call_back(
            crate::recursion::PROXY_TRAP,
            &t,
            &Val::Obj(handler),
            &[Val::Obj(target.clone()), arg],
        )?
        .truthy()
    {
        return it.type_error("proxy setPrototypeOf handler returned false");
    }
    if !target.borrow().extensible && !same_proto(&it.proto_of(&target)?, &p) {
        return it.type_error("proxy setPrototypeOf handler returned true, even though the target's prototype is immutable because the target is non-extensible");
    }
    Ok(())
}

fn key_val(key: &str) -> Val {
    if crate::value::is_symbol_key(key) { crate::props::symbol_of_key(key) } else { Val::str(key) }
}

pub fn get(it: &mut Interp, o: &ObjRef, key: &str, receiver: &Val) -> R {
    let (target, handler) = live(it, o)?;
    match trap(it, &handler, "get")? {
        Some(t) => it.call_back(
            crate::recursion::PROXY_TRAP,
            &t,
            &Val::Obj(handler),
            &[Val::Obj(target), key_val(key), receiver.clone()],
        ),
        None => it.get(&Val::Obj(target), key),
    }
}

pub fn set(it: &mut Interp, o: &ObjRef, key: &str, value: Val, receiver: &Val) -> R<()> {
    let (target, handler) = live(it, o)?;
    match trap(it, &handler, "set")? {
        Some(t) => {
            let args = [Val::Obj(target), key_val(key), value, receiver.clone()];
            let ok =
                it.call_back(crate::recursion::PROXY_TRAP, &t, &Val::Obj(handler), &args)?.truthy();
            if !ok && it.strict {
                return it.type_error(&format!(
                    "proxy set handler returned false for property '\"{key}\"'"
                ));
            }
            Ok(())
        }
        None => it.set(&Val::Obj(target), key, value),
    }
}

pub fn has(it: &mut Interp, o: &ObjRef, key: &str) -> R<bool> {
    let (target, handler) = live(it, o)?;
    match trap(it, &handler, "has")? {
        Some(t) => Ok(it
            .call_back(
                crate::recursion::PROXY_TRAP,
                &t,
                &Val::Obj(handler),
                &[Val::Obj(target), key_val(key)],
            )?
            .truthy()),
        None => it.has(&target, key),
    }
}

/// `[[DefineOwnProperty]]` through the `defineProperty` trap, or on the target.
pub fn define(it: &mut Interp, o: &ObjRef, key: &str, desc: Val) -> R<bool> {
    let (target, handler) = live(it, o)?;
    match trap(it, &handler, "defineProperty")? {
        Some(t) => {
            let args = [Val::Obj(target), key_val(key), desc];
            Ok(it.call_back(crate::recursion::PROXY_TRAP, &t, &Val::Obj(handler), &args)?.truthy())
        }
        None => {
            let d = crate::props::to_desc(it, &desc)?;
            crate::props::define_own(it, &target, key, &d)?;
            Ok(true)
        }
    }
}

pub fn delete(it: &mut Interp, o: &ObjRef, key: &str) -> R<bool> {
    let (target, handler) = live(it, o)?;
    match trap(it, &handler, "deleteProperty")? {
        Some(t) => {
            let ok = it
                .call_back(
                    crate::recursion::PROXY_TRAP,
                    &t,
                    &Val::Obj(handler),
                    &[Val::Obj(target), key_val(key)],
                )?
                .truthy();
            if !ok && it.strict {
                return it.type_error(&format!(
                    "can't delete property '\"{key}\"': proxy deleteProperty handler returned false"
                ));
            }
            Ok(ok)
        }
        None => it.delete_key(&target, key),
    }
}

/// The own keys, through `ownKeys`, and their enumerability through `getOwnPropertyDescriptor`.
pub fn keys(it: &mut Interp, o: &ObjRef, only_enumerable: bool) -> R<Vec<Name>> {
    let (target, handler) = live(it, o)?;
    let all = match trap(it, &handler, "ownKeys")? {
        Some(t) => {
            let list = it.call_back(
                crate::recursion::PROXY_TRAP,
                &t,
                &Val::Obj(handler.clone()),
                &[Val::Obj(target.clone())],
            )?;
            if !matches!(list, Val::Obj(_)) {
                let m = format!("ownKeys trap must be an object, got {}", it.show(&list));
                return it.type_error(&m);
            }
            let mut out = Vec::new();
            for k in it.iterate(&list)? {
                match k {
                    Val::Str(s) => out.push(s),
                    Val::Sym(_) => {}
                    _ => {
                        return it.type_error(
                            "proxy [[OwnPropertyKeys]] must return an array with only string and symbol elements",
                        );
                    }
                }
            }
            out
        }
        None => return it.keys(&target, only_enumerable),
    };
    if !only_enumerable {
        return Ok(all);
    }
    let mut out = Vec::new();
    for k in all {
        if let Some(d) = descriptor(it, &target, &handler, &k)?
            && it.get(&d, "enumerable")?.truthy()
        {
            out.push(k);
        }
    }
    Ok(out)
}

/// The descriptor `getOwnPropertyDescriptor` gives for `key`, or the target's.
fn descriptor(it: &mut Interp, target: &ObjRef, handler: &ObjRef, key: &str) -> R<Option<Val>> {
    let d = match trap(it, handler, "getOwnPropertyDescriptor")? {
        Some(t) => it.call_back(
            crate::recursion::PROXY_TRAP,
            &t,
            &Val::Obj(handler.clone()),
            &[Val::Obj(target.clone()), key_val(key)],
        )?,
        None => {
            if !it.has_own_property(target, key)? {
                return Ok(None);
            }
            let enumerable = it.keys(target, true)?.iter().any(|k| &**k == key);
            let o = it.object()?;
            it.define(&o, "enumerable", Prop::data(Val::Bool(enumerable)))?;
            Val::Obj(o)
        }
    };
    match d {
        Val::Undef => Ok(None),
        Val::Obj(_) => Ok(Some(d)),
        _ => it.type_error(
            "proxy getOwnPropertyDescriptor handler returned neither object nor undefined",
        ),
    }
}

pub fn has_own(it: &mut Interp, o: &ObjRef, key: &str) -> R<bool> {
    let (target, handler) = live(it, o)?;
    Ok(descriptor(it, &target, &handler, key)?.is_some())
}

pub fn apply(it: &mut Interp, o: &ObjRef, this: &Val, args: &[Val]) -> R {
    let (target, handler) = live(it, o)?;
    match trap(it, &handler, "apply")? {
        Some(t) => {
            let list = it.array(args.to_vec())?;
            it.call_back(
                crate::recursion::PROXY_TRAP,
                &t,
                &Val::Obj(handler),
                &[Val::Obj(target), this.clone(), list],
            )
        }
        None => it.call(&Val::Obj(target), this, args),
    }
}

pub fn construct(it: &mut Interp, o: &ObjRef, args: &[Val], new_target: &Val) -> R {
    let (target, handler) = live(it, o)?;
    let nt = if matches!(new_target, Val::Obj(n) if n.ptr_eq(o)) {
        Val::Obj(target.clone())
    } else {
        new_target.clone()
    };
    match trap(it, &handler, "construct")? {
        Some(t) => {
            let list = it.array(args.to_vec())?;
            let r = it.call_back(
                crate::recursion::PROXY_TRAP,
                &t,
                &Val::Obj(handler),
                &[Val::Obj(target), list, new_target.clone()],
            )?;
            if !matches!(r, Val::Obj(_)) {
                return it.type_error("proxy [[Construct]] must return an object");
            }
            Ok(r)
        }
        None => it.construct(&Val::Obj(target), args, &nt),
    }
}

/// Whether `v` is an array, looking through proxies (`Array.isArray`).
pub fn is_array(it: &mut Interp, v: &Val) -> R<bool> {
    let Val::Obj(o) = v else { return Ok(false) };
    match parts(o) {
        None => Ok(matches!(o.borrow().kind, Kind::Array(_))),
        Some(None) => it.type_error(REVOKED),
        Some(Some((t, _))) => is_array(it, &Val::Obj(t)),
    }
}

/// How SpiderMonkey shows a value that should have been an object: `the number 1`,
/// `the string "s"`, `the BigInt 1n`, `true`, `null`, `Symbol("q")`.
fn got(v: &Val) -> String {
    match v {
        Val::Num(n) => format!("the number {}", crate::number::to_string(*n)),
        Val::Str(s) => format!("the string \"{s}\""),
        Val::BigInt(b) => format!("the BigInt {b}n"),
        Val::Sym(s) => format!("Symbol(\"{}\")", s.0),
        Val::Bool(b) => b.to_string(),
        Val::Null => "null".into(),
        Val::Undef | Val::Hole => "undefined".into(),
        Val::Obj(_) => "an object".into(),
    }
}

fn make(it: &mut Interp, a: &[Val]) -> R<ObjRef> {
    let (target, handler) = match (arg(a, 0), arg(a, 1)) {
        (Val::Obj(t), Val::Obj(h)) => (t, h),
        (Val::Obj(_), h) => {
            let m = format!("`handler` argument of Proxy must be an object, got {}", got(&h));
            return it.type_error(&m);
        }
        (t, _) => {
            let m = format!("`target` argument of Proxy must be an object, got {}", got(&t));
            return it.type_error(&m);
        }
    };
    let callable = target.borrow().is_callable();
    let constructor = crate::interp::is_constructor(&Val::Obj(target.clone()));
    let proto = target.borrow().proto.clone();
    it.alloc(proto, Kind::Proxy(Some((target, handler)), callable, constructor))
}

fn proxy_ctor(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    if new_target(this).is_none() {
        return it.type_error("calling a builtin Proxy constructor without new is forbidden");
    }
    Ok(Val::Obj(make(it, a)?))
}

fn revoke(_: &mut Interp, this: &Val, _: &[Val]) -> R {
    if let Val::Obj(o) = this
        && let Kind::Proxy(p, ..) = &mut o.borrow_mut().kind
    {
        *p = None;
    }
    Ok(Val::Undef)
}

fn revocable(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let p = make(it, a)?;
    let target = it.native("", revoke, 0)?;
    let r = it.alloc(
        Some(it.realm.function.clone()),
        Kind::Func(Func::Bound(target, Val::Obj(p.clone()), Vec::new())),
    )?;
    let o = it.object()?;
    it.define(&o, "proxy", Prop::data(Val::Obj(p)))?;
    it.define(&o, "revoke", Prop::data(Val::Obj(r)))?;
    Ok(Val::Obj(o))
}

pub fn install(it: &mut Interp, g: &ObjRef) -> R<()> {
    let proto = it.alloc(Some(it.realm.object.clone()), Kind::Ordinary)?;
    let c = ctor(it, g, "Proxy", proxy_ctor, 2, &proto)?;
    // `Proxy` has no `prototype`.
    c.borrow_mut().remove("prototype");
    method(it, &c, "revocable", revocable, 2)
}
