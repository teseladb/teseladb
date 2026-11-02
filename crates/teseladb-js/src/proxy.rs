//! `Proxy`: an object whose operations go to the traps of its handler, or to its target when
//! the handler has no trap for them, with SpiderMonkey's messages (measured on a `refrouter`
//! 9.0.4 on 02-11). A revoked proxy fails every operation.

use crate::builtins::{arg, ctor, method, new_target};
use crate::interp::{Interp, R};
use crate::props::{Desc, own_prop, same_fn, same_value};
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
    let Some(t) = trap(it, &handler, "get")? else { return it.get(&Val::Obj(target), key) };
    let v = it.call_back(
        crate::recursion::PROXY_TRAP,
        &t,
        &Val::Obj(handler),
        &[Val::Obj(target.clone()), key_val(key), receiver.clone()],
    )?;
    // A non-configurable property of the target fixes what the trap may give (§10.5.8).
    if let Some(p) = target_prop(it, &target, key)?
        && !p.configurable
        && if p.is_accessor() {
            p.getter.is_none() && !matches!(v, Val::Undef)
        } else {
            !p.writable && !same_value(&v, &p.value)
        }
    {
        let k = shown(it, key);
        return it.type_error(&format!(
            "proxy must report the same value for the non-writable, non-configurable property {k}"
        ));
    }
    Ok(v)
}

/// `[[GetOwnProperty]]` of the target of a proxy, which may be a proxy itself.
fn target_prop(it: &mut Interp, target: &ObjRef, key: &str) -> R<Option<Prop>> {
    if is_proxy(target) { own_property(it, target, key) } else { Ok(own_prop(target, key)) }
}

/// A key as the messages of a proxy show it: `'"a"'`.
fn shown(it: &Interp, key: &str) -> String {
    format!("'{}'", crate::props::shown_key(it, key))
}

/// Whether the target of a proxy may grow: its own `[[IsExtensible]]`.
fn extensible(it: &mut Interp, target: &ObjRef) -> R<bool> {
    if is_proxy(target) { is_extensible(it, target) } else { Ok(target.borrow().extensible) }
}

/// `[[IsExtensible]]` of a proxy: its `isExtensible` trap, which must agree with its target
/// (§10.5.3).
pub fn is_extensible(it: &mut Interp, o: &ObjRef) -> R<bool> {
    let (target, handler) = live(it, o)?;
    let Some(t) = trap(it, &handler, "isExtensible")? else { return extensible(it, &target) };
    let r = it
        .call_back(
            crate::recursion::PROXY_TRAP,
            &t,
            &Val::Obj(handler),
            &[Val::Obj(target.clone())],
        )?
        .truthy();
    if r != extensible(it, &target)? {
        return it.type_error("proxy must report same extensiblitity as target");
    }
    Ok(r)
}

/// `[[PreventExtensions]]` of a proxy: its `preventExtensions` trap, which may only succeed when
/// its target no longer grows (§10.5.4).
pub fn prevent_extensions(it: &mut Interp, o: &ObjRef) -> R<bool> {
    let (target, handler) = live(it, o)?;
    let Some(t) = trap(it, &handler, "preventExtensions")? else {
        if is_proxy(&target) {
            return prevent_extensions(it, &target);
        }
        target.borrow_mut().extensible = false;
        return Ok(true);
    };
    let r = it
        .call_back(
            crate::recursion::PROXY_TRAP,
            &t,
            &Val::Obj(handler),
            &[Val::Obj(target.clone())],
        )?
        .truthy();
    if r && extensible(it, &target)? {
        return it.type_error("proxy can't report an extensible object as non-extensible");
    }
    Ok(r)
}

/// Why `desc` may not stand for a property whose target has `current` (IsCompatiblePropertyDescriptor,
/// ECMA-262 §10.1.6.2), in SpiderMonkey's words.
fn incompatible(extensible: bool, desc: &Desc, current: Option<&Prop>) -> Option<&'static str> {
    let Some(c) = current else {
        return (!extensible)
            .then_some("proxy can't report an extensible object as non-extensible");
    };
    if c.configurable {
        return None;
    }
    if desc.configurable == Some(true) {
        return Some("proxy can't report an existing non-configurable property as configurable");
    }
    if desc.enumerable.is_some_and(|e| e != c.enumerable) {
        return Some(
            "proxy can't report a different 'enumerable' from target when target is not configurable",
        );
    }
    if !desc.is_data() && !desc.is_accessor() {
        return None;
    }
    if desc.is_accessor() != c.is_accessor() {
        return Some(
            "proxy can't report a different descriptor type when target is not configurable",
        );
    }
    if c.is_accessor() {
        if desc.get.as_ref().is_some_and(|g| !same_fn(g, &c.getter)) {
            return Some(
                "proxy can't report different getters for a currently non-configurable property",
            );
        }
        if desc.set.as_ref().is_some_and(|f| !same_fn(f, &c.setter)) {
            return Some(
                "proxy can't report different setters for a currently non-configurable property",
            );
        }
        return None;
    }
    if !c.writable {
        if desc.writable == Some(true) {
            return Some(
                "proxy can't report a non-configurable, non-writable property as writable",
            );
        }
        if desc.value.as_ref().is_some_and(|v| !same_value(v, &c.value)) {
            return Some(
                "proxy must report the same value for the non-writable, non-configurable property",
            );
        }
    }
    None
}

/// `[[GetOwnProperty]]` of a proxy: what its `getOwnPropertyDescriptor` trap gives, checked
/// against its target (§10.5.5), or its target's property.
pub fn own_property(it: &mut Interp, o: &ObjRef, key: &str) -> R<Option<Prop>> {
    let (target, handler) = live(it, o)?;
    let Some(t) = trap(it, &handler, "getOwnPropertyDescriptor")? else {
        return target_prop(it, &target, key);
    };
    let r = it.call_back(
        crate::recursion::PROXY_TRAP,
        &t,
        &Val::Obj(handler),
        &[Val::Obj(target.clone()), key_val(key)],
    )?;
    let current = target_prop(it, &target, key)?;
    let k = shown(it, key);
    if matches!(r, Val::Undef) {
        let Some(c) = current else { return Ok(None) };
        if !c.configurable {
            return it.type_error(&format!(
                "proxy can't report a non-configurable own property {k} as non-existent"
            ));
        }
        if !extensible(it, &target)? {
            return it.type_error(&format!(
                "proxy can't report an existing own property {k} as non-existent on a non-extensible object"
            ));
        }
        return Ok(None);
    }
    if !matches!(r, Val::Obj(_)) {
        return it.type_error(&format!(
            "proxy [[GetOwnProperty]] must return an object or undefined for property {k}"
        ));
    }
    let d = crate::props::to_desc(it, &r)?;
    let grows = extensible(it, &target)?;
    if let Some(why) = incompatible(grows, &d, current.as_ref()) {
        return it.type_error(&format!(
            "proxy can't report an incompatible property descriptor ({k}, {why})"
        ));
    }
    if d.configurable != Some(true) {
        match &current {
            None => {
                return it.type_error(&format!(
                    "proxy can't report a non-existent property {k} as non-configurable"
                ));
            }
            Some(c) if c.configurable => {
                return it.type_error(&format!(
                    "proxy can't report existing configurable property {k} as non-configurable"
                ));
            }
            Some(c) if d.writable == Some(false) && !c.is_accessor() && c.writable => {
                return it.type_error(&format!(
                    "proxy can't report existing writable property {k} as non-writable"
                ));
            }
            Some(_) => {}
        }
    }
    let accessor = d.is_accessor();
    Ok(Some(Prop {
        value: d.value.unwrap_or(Val::Undef),
        getter: d.get.flatten(),
        setter: d.set.flatten(),
        enumerable: d.enumerable.unwrap_or(false),
        writable: !accessor && d.writable.unwrap_or(false),
        configurable: d.configurable.unwrap_or(false),
    }))
}

pub fn set(it: &mut Interp, o: &ObjRef, key: &str, value: Val, receiver: &Val) -> R<()> {
    let (target, handler) = live(it, o)?;
    match trap(it, &handler, "set")? {
        Some(t) => {
            let args = [Val::Obj(target.clone()), key_val(key), value.clone(), receiver.clone()];
            let ok =
                it.call_back(crate::recursion::PROXY_TRAP, &t, &Val::Obj(handler), &args)?.truthy();
            if !ok {
                if it.strict {
                    let k = shown(it, key);
                    return it
                        .type_error(&format!("proxy set handler returned false for property {k}"));
                }
                return Ok(());
            }
            // A non-configurable property of the target fixes what may be set (§10.5.9).
            if let Some(p) = target_prop(it, &target, key)?
                && !p.configurable
            {
                let k = shown(it, key);
                if p.is_accessor() && p.setter.is_none() {
                    return it.type_error(&format!(
                        "proxy can't succesfully set an accessor property {k} without a setter"
                    ));
                }
                if !p.is_accessor() && !p.writable && !same_value(&value, &p.value) {
                    return it.type_error(&format!(
                        "proxy can't successfully set a non-writable, non-configurable property {k}"
                    ));
                }
            }
            Ok(())
        }
        None => it.set(&Val::Obj(target), key, value),
    }
}

pub fn has(it: &mut Interp, o: &ObjRef, key: &str) -> R<bool> {
    let (target, handler) = live(it, o)?;
    let Some(t) = trap(it, &handler, "has")? else { return it.has(&target, key) };
    let found = it
        .call_back(
            crate::recursion::PROXY_TRAP,
            &t,
            &Val::Obj(handler),
            &[Val::Obj(target.clone()), key_val(key)],
        )?
        .truthy();
    if !found && let Some(p) = target_prop(it, &target, key)? {
        let k = shown(it, key);
        if !p.configurable {
            return it.type_error(&format!(
                "proxy can't report a non-configurable own property {k} as non-existent"
            ));
        }
        if !extensible(it, &target)? {
            return it.type_error(&format!(
                "proxy can't report an existing own property {k} as non-existent on a non-extensible object"
            ));
        }
    }
    Ok(found)
}

/// `[[DefineOwnProperty]]` through the `defineProperty` trap, or on the target.
pub fn define(it: &mut Interp, o: &ObjRef, key: &str, desc: Val) -> R<bool> {
    let (target, handler) = live(it, o)?;
    match trap(it, &handler, "defineProperty")? {
        Some(t) => {
            let d = crate::props::to_desc(it, &desc)?;
            let args = [Val::Obj(target.clone()), key_val(key), desc];
            if !it.call_back(crate::recursion::PROXY_TRAP, &t, &Val::Obj(handler), &args)?.truthy()
            {
                return Ok(false);
            }
            check_defined(it, &target, key, &d)?;
            Ok(true)
        }
        None => {
            let d = crate::props::to_desc(it, &desc)?;
            crate::props::define_own(it, &target, key, &d)?;
            Ok(true)
        }
    }
}

/// What a `defineProperty` trap that succeeded may have done to the target (§10.5.6).
fn check_defined(it: &mut Interp, target: &ObjRef, key: &str, d: &Desc) -> R<()> {
    let current = target_prop(it, target, key)?;
    let k = shown(it, key);
    let fixing = d.configurable == Some(false);
    let Some(c) = current else {
        if !extensible(it, target)? {
            return it.type_error(&format!(
                "proxy can't define a new property {k} on a non-extensible object"
            ));
        }
        if fixing {
            return it.type_error(&format!(
                "proxy can't define a non-existent {k} property as non-configurable"
            ));
        }
        return Ok(());
    };
    let why = if let Some(why) = incompatible(true, d, Some(&c)) {
        Some(why)
    } else if fixing && c.configurable {
        Some("proxy can't define an existing configurable property as non-configurable")
    } else if !c.is_accessor() && !c.configurable && c.writable && d.writable == Some(false) {
        Some("proxy can't define an existing non-configurable writable property as non-writable")
    } else {
        None
    };
    match why {
        Some(why) => it.type_error(&format!(
            "proxy can't define an incompatible property descriptor ({k}, {why})"
        )),
        None => Ok(()),
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
                    &[Val::Obj(target.clone()), key_val(key)],
                )?
                .truthy();
            if !ok && it.strict {
                return it.type_error(&format!(
                    "can't delete property '\"{key}\"': proxy deleteProperty handler returned false"
                ));
            }
            if ok && let Some(p) = target_prop(it, &target, key)? {
                if !p.configurable {
                    let k = crate::props::shown_key(it, key);
                    return it.type_error(&format!(
                        "property {k} is non-configurable and can't be deleted"
                    ));
                }
                if !extensible(it, &target)? {
                    let k = shown(it, key);
                    return it.type_error(&format!(
                        "proxy can't delete property {k} on a non-extensible object"
                    ));
                }
            }
            Ok(ok)
        }
        None => it.delete_key(&target, key),
    }
}

/// `[[OwnPropertyKeys]]` of a proxy: what its `ownKeys` trap gives, checked against its target
/// (§10.5.11), or its target's keys; symbols as their keys.
pub fn own_keys(it: &mut Interp, o: &ObjRef) -> R<Vec<Name>> {
    let (target, handler) = live(it, o)?;
    let Some(t) = trap(it, &handler, "ownKeys")? else {
        if is_proxy(&target) {
            return own_keys(it, &target);
        }
        let mut all = it.keys(&target, false)?;
        all.extend(crate::props::symbol_keys(&target));
        return Ok(all);
    };
    let list = it.call_back(
        crate::recursion::PROXY_TRAP,
        &t,
        &Val::Obj(handler),
        &[Val::Obj(target.clone())],
    )?;
    if !matches!(list, Val::Obj(_)) {
        let m = format!("ownKeys trap must be an object, got {}", it.show(&list));
        return it.type_error(&m);
    }
    let mut seen: Vec<Name> = Vec::new();
    for k in it.iterate(&list)? {
        if !matches!(k, Val::Str(_) | Val::Sym(_)) {
            return it.type_error(
                "proxy [[OwnPropertyKeys]] must return an array with only string and symbol elements",
            );
        }
        let key = it.key_of(&k)?;
        if seen.contains(&key) {
            let k = shown(it, &key);
            return it.type_error(&format!(
                "proxy [[OwnPropertyKeys]] can't report property {k} more than once"
            ));
        }
        seen.push(key);
    }
    check_keys(it, &target, &seen)?;
    Ok(seen)
}

/// The string keys of a proxy, the enumerable ones through `[[GetOwnProperty]]`.
pub fn keys(it: &mut Interp, o: &ObjRef, only_enumerable: bool) -> R<Vec<Name>> {
    let all: Vec<Name> = own_keys(it, o)?.into_iter().filter(|k| !is_symbol_key(k)).collect();
    if !only_enumerable {
        return Ok(all);
    }
    let mut out = Vec::new();
    for k in all {
        if own_property(it, o, &k)?.is_some_and(|p| p.enumerable) {
            out.push(k);
        }
    }
    Ok(out)
}

/// SetIntegrityLevel of a proxy (`Object.seal`, `Object.freeze`): no more growth, and every
/// property fixed through its traps.
pub fn lock(it: &mut Interp, o: &ObjRef, frozen: bool) -> R<()> {
    if !prevent_extensions(it, o)? {
        return it.type_error("proxy preventExtensions handler returned false");
    }
    for k in own_keys(it, o)? {
        let d = it.object()?;
        it.define(&d, "configurable", Prop::data(Val::Bool(false)))?;
        if frozen && own_property(it, o, &k)?.is_some_and(|p| !p.is_accessor()) {
            it.define(&d, "writable", Prop::data(Val::Bool(false)))?;
        }
        if !define(it, o, &k, Val::Obj(d))? {
            let k = shown(it, &k);
            return it.type_error(&format!(
                "proxy defineProperty handler returned false for property {k}"
            ));
        }
    }
    Ok(())
}

/// TestIntegrityLevel of a proxy (`Object.isSealed`, `Object.isFrozen`).
pub fn is_locked(it: &mut Interp, o: &ObjRef, frozen: bool) -> R<bool> {
    if is_extensible(it, o)? {
        return Ok(false);
    }
    for k in own_keys(it, o)? {
        if let Some(p) = own_property(it, o, &k)?
            && (p.configurable || frozen && !p.is_accessor() && p.writable)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The keys an `ownKeys` trap gave, against the target's (§10.5.11): every non-configurable key,
/// and for a target that does not grow exactly its keys.
fn check_keys(it: &mut Interp, target: &ObjRef, keys: &[Name]) -> R<()> {
    let grows = extensible(it, target)?;
    let mut own = it.keys(target, false)?;
    if matches!(target.borrow().kind, Kind::Array(_)) {
        own.push("length".into());
    }
    own.extend(crate::props::symbol_keys(target));
    for k in &own {
        if keys.contains(k) {
            continue;
        }
        let fixed = target_prop(it, target, k)?.is_some_and(|p| !p.configurable);
        let shown_k = shown(it, k);
        if fixed {
            return it
                .type_error(&format!("proxy can't skip a non-configurable property {shown_k}"));
        }
        if !grows {
            return it.type_error(&format!(
                "proxy can't report an existing own property {shown_k} as non-existent on a non-extensible object"
            ));
        }
    }
    if !grows && let Some(k) = keys.iter().find(|k| !own.contains(k)) {
        let k = shown(it, k);
        return it.type_error(&format!(
            "proxy can't report a new property {k} on a non-extensible object"
        ));
    }
    Ok(())
}

pub fn has_own(it: &mut Interp, o: &ObjRef, key: &str) -> R<bool> {
    Ok(own_property(it, o, key)?.is_some())
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
