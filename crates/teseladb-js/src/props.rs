//! Property attributes (ECMA-262 §6.2.6, §10.1.6, §20.1.2): `Object.defineProperty` and its
//! checks, the descriptors `Object.getOwnPropertyDescriptor` gives, `freeze`, `seal` and
//! `preventExtensions`, and the symbol keys of an object. Messages are those of the engine of a
//! `refrouter` 9.0.4 (measured on 02-11).

use std::rc::Rc;

use crate::builtins::{arg, method};
use crate::interp::{Interp, R};
use crate::value::*;

/// A property descriptor as `Object.defineProperty` reads it: the fields it has.
#[derive(Default)]
pub struct Desc {
    pub value: Option<Val>,
    pub writable: Option<bool>,
    pub get: Option<Option<ObjRef>>,
    pub set: Option<Option<ObjRef>>,
    pub enumerable: Option<bool>,
    pub configurable: Option<bool>,
}

impl Desc {
    fn is_accessor(&self) -> bool {
        self.get.is_some() || self.set.is_some()
    }

    fn is_data(&self) -> bool {
        self.value.is_some() || self.writable.is_some()
    }
}

pub fn install(it: &mut Interp, object: &ObjRef) -> R<()> {
    for (name, f, n) in [
        ("getOwnPropertyDescriptor", get_own_property_descriptor as NativeFn, 2),
        ("getOwnPropertyDescriptors", get_own_property_descriptors, 1),
        ("getOwnPropertySymbols", get_own_property_symbols, 1),
        ("defineProperty", define_property, 3),
        ("defineProperties", define_properties, 2),
        ("isExtensible", is_extensible, 1),
        ("isSealed", is_sealed, 1),
        ("isFrozen", is_frozen, 1),
        ("freeze", freeze, 1),
        ("seal", seal, 1),
        ("preventExtensions", prevent_extensions, 1),
    ] {
        method(it, object, name, f, n)?;
    }
    let proto = it.realm.object.clone();
    method(it, &proto, "propertyIsEnumerable", property_is_enumerable, 1)?;
    Ok(())
}

/// The symbol a symbol key stands for.
pub fn symbol_of_key(key: &str) -> Val {
    let (desc, id) = key
        .strip_prefix("@@")
        .and_then(|k| k.rsplit_once(':'))
        .map_or(("", 0), |(d, id)| (d, id.parse().unwrap_or(0)));
    Val::Sym(Rc::new((desc.into(), id)))
}

/// The own property `key` of `o` with its attributes, the elements an array or a string holds
/// included.
pub fn own_prop(o: &ObjRef, key: &str) -> Option<Prop> {
    let b = o.borrow();
    let fixed = |value: Val, writable: bool, enumerable: bool, configurable: bool| Prop {
        value,
        getter: None,
        setter: None,
        enumerable,
        writable,
        configurable,
    };
    match &b.kind {
        Kind::Array(items) => {
            if key == "length" {
                let ro = b.locks & LENGTH_FIXED != 0;
                return Some(fixed(Val::Num(items.len() as f64), !ro, false, false));
            }
            if let Some(i) = array_index(key)
                && let Some(x) = items.get(i)
            {
                if b.locks & ELEMENTS_OWN != 0
                    && let Some(p) = b.get_own(key)
                {
                    return Some(p.clone());
                }
                let frozen = b.locks & ELEMENTS_FROZEN != 0;
                let sealed = b.locks & ELEMENTS_SEALED != 0;
                return Some(fixed(x.clone(), !frozen, true, !sealed));
            }
        }
        Kind::Str(s) => {
            if key == "length" {
                return Some(fixed(
                    Val::Num(crate::strings::utf16_len(s) as f64),
                    false,
                    false,
                    false,
                ));
            }
            if let Some(i) = array_index(key)
                && let Some(c) = crate::interp::utf16_at(s, i)
            {
                let v = Val::Str(crate::strings::from_units(&[c]).into());
                return Some(fixed(v, false, true, false));
            }
        }
        Kind::Typed(t) => {
            if let Some(i) = crate::typed::numeric_key(key) {
                return match i {
                    Some(i) if i < t.length() => {
                        Some(fixed(crate::typed::read(t, i), true, true, true))
                    }
                    _ => None,
                };
            }
        }
        _ => {}
    }
    b.get_own(key).cloned()
}

/// The object `Object.getOwnPropertyDescriptor` gives for `p`.
pub fn descriptor_object(it: &mut Interp, p: &Prop) -> R {
    let o = it.object()?;
    if p.is_accessor() {
        let get = p.getter.clone().map_or(Val::Undef, Val::Obj);
        let set = p.setter.clone().map_or(Val::Undef, Val::Obj);
        it.define(&o, "get", Prop::data(get))?;
        it.define(&o, "set", Prop::data(set))?;
    } else {
        it.define(&o, "value", Prop::data(p.value.clone()))?;
        it.define(&o, "writable", Prop::data(Val::Bool(p.writable)))?;
    }
    it.define(&o, "enumerable", Prop::data(Val::Bool(p.enumerable)))?;
    it.define(&o, "configurable", Prop::data(Val::Bool(p.configurable)))?;
    Ok(Val::Obj(o))
}

/// ToPropertyDescriptor: the fields `d` has, checked.
pub fn to_desc(it: &mut Interp, d: &Val) -> R<Desc> {
    let Val::Obj(o) = d else {
        return it
            .type_error(&format!("Property descriptor must be an object, got {}", it.show(d)));
    };
    let mut out = Desc::default();
    let has = |it: &mut Interp, k: &str| it.has(o, k);
    if has(it, "enumerable")? {
        out.enumerable = Some(it.get(d, "enumerable")?.truthy());
    }
    if has(it, "configurable")? {
        out.configurable = Some(it.get(d, "configurable")?.truthy());
    }
    if has(it, "value")? {
        out.value = Some(it.get(d, "value")?);
    }
    if has(it, "writable")? {
        out.writable = Some(it.get(d, "writable")?.truthy());
    }
    for (field, slot) in [("get", &mut out.get), ("set", &mut out.set)] {
        if !has(it, field)? {
            continue;
        }
        *slot = Some(match it.get(d, field)? {
            Val::Undef => None,
            Val::Obj(f) if f.borrow().is_callable() => Some(f),
            _ => {
                return it.type_error(&format!(
                    "property descriptor's {field} field is neither undefined nor a function"
                ));
            }
        });
    }
    if out.is_accessor() && out.is_data() {
        return it.type_error(
            "property descriptors must not specify a value or be writable when a getter or \
             setter has been specified",
        );
    }
    Ok(out)
}

fn same_value(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::Num(x), Val::Num(y)) => {
            (x.is_nan() && y.is_nan()) || (x == y && x.is_sign_negative() == y.is_sign_negative())
        }
        _ => strict_eq(a, b),
    }
}

fn same_fn(a: &Option<ObjRef>, b: &Option<ObjRef>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x.ptr_eq(y),
        _ => false,
    }
}

/// Whether `d` changes a non-configurable `cur` in a way it may not.
fn forbidden(cur: &Prop, d: &Desc) -> bool {
    if cur.configurable {
        return false;
    }
    if d.configurable == Some(true) || d.enumerable.is_some_and(|e| e != cur.enumerable) {
        return true;
    }
    if cur.is_accessor() {
        return d.is_data()
            || d.get.as_ref().is_some_and(|g| !same_fn(g, &cur.getter))
            || d.set.as_ref().is_some_and(|s| !same_fn(s, &cur.setter));
    }
    if d.is_accessor() {
        return true;
    }
    !cur.writable
        && (d.writable == Some(true)
            || d.value.as_ref().is_some_and(|v| !same_value(v, &cur.value)))
}

/// ValidateAndApplyPropertyDescriptor for `o[key]`, throwing as `Object.defineProperty` does.
pub fn define_own(it: &mut Interp, o: &ObjRef, key: &str, d: &Desc) -> R<()> {
    let is_array = matches!(o.borrow().kind, Kind::Array(_));
    let cur = own_prop(o, key);
    let shown = shown_key(key);
    match &cur {
        None if !o.borrow().extensible => {
            return it
                .type_error(&format!("can't define property {shown}: Object is not extensible"));
        }
        Some(c) if forbidden(c, d) => {
            return it.type_error(&format!("can't redefine non-configurable property {shown}"));
        }
        _ => {}
    }
    if is_array && (key == "length" || array_index(key).is_some()) {
        return define_array(it, o, key, d);
    }
    let base = cur.unwrap_or(Prop {
        value: Val::Undef,
        getter: None,
        setter: None,
        enumerable: false,
        writable: false,
        configurable: false,
    });
    let mut p = base.clone();
    if d.is_accessor() && !base.is_accessor() {
        p.value = Val::Undef;
        p.writable = false;
    }
    if d.is_data() && base.is_accessor() {
        p.getter = None;
        p.setter = None;
    }
    if let Some(v) = &d.value {
        p.value = v.clone();
    }
    if let Some(w) = d.writable {
        p.writable = w;
    }
    if let Some(g) = &d.get {
        p.getter = g.clone();
    }
    if let Some(s) = &d.set {
        p.setter = s.clone();
    }
    if let Some(e) = d.enumerable {
        p.enumerable = e;
    }
    if let Some(c) = d.configurable {
        p.configurable = c;
    }
    it.define(o, key, p)
}

/// `Object.defineProperty` on an array's `length` or one of its elements.
fn define_array(it: &mut Interp, o: &ObjRef, key: &str, d: &Desc) -> R<()> {
    if key == "length" {
        if let Some(v) = &d.value {
            it.set(&Val::Obj(o.clone()), "length", v.clone())?;
        }
        if d.writable == Some(false) {
            o.borrow_mut().locks |= LENGTH_FIXED;
        }
        return Ok(());
    }
    let cur = own_prop(o, key);
    let has_own = o.borrow().locks & ELEMENTS_OWN != 0 && o.borrow().get_own(key).is_some();
    let plain = !d.is_accessor()
        && d.writable != Some(false)
        && d.enumerable != Some(false)
        && d.configurable != Some(false)
        && (cur.is_none()
            || d.writable.is_some() && d.enumerable.is_some() && d.configurable.is_some());
    if plain && !has_own {
        if let Some(v) = &d.value {
            let outer = std::mem::replace(&mut it.strict, true);
            let r = it.set(&Val::Obj(o.clone()), key, v.clone());
            it.strict = outer;
            r?;
        } else if cur.is_none() {
            it.set(&Val::Obj(o.clone()), key, Val::Undef)?;
        }
        return Ok(());
    }
    // An element with attributes of its own: kept in the properties, over the kind's value.
    let base = cur.unwrap_or(Prop {
        value: Val::Undef,
        getter: None,
        setter: None,
        enumerable: false,
        writable: false,
        configurable: false,
    });
    let p = merged(&base, d);
    let i = array_index(key).unwrap_or(0);
    if i >= it.array_len(o) {
        if o.borrow().locks & LENGTH_FIXED != 0 {
            return Err(crate::arrays::cannot_grow(it, true, i));
        }
        it.grow(o, i + 1 - it.array_len(o))?;
    }
    if let Kind::Array(items) = &mut o.borrow_mut().kind {
        if i >= items.len() {
            items.resize(i + 1, Val::Undef);
        }
        items[i] = if p.is_accessor() { Val::Undef } else { p.value.clone() };
    }
    o.borrow_mut().locks |= ELEMENTS_OWN;
    it.define(o, key, p)
}

/// `base` with the fields `d` has.
fn merged(base: &Prop, d: &Desc) -> Prop {
    let mut p = base.clone();
    if d.is_accessor() && !base.is_accessor() {
        p.value = Val::Undef;
        p.writable = false;
    }
    if d.is_data() && base.is_accessor() {
        p.getter = None;
        p.setter = None;
    }
    if let Some(v) = &d.value {
        p.value = v.clone();
    }
    if let Some(w) = d.writable {
        p.writable = w;
    }
    if let Some(g) = &d.get {
        p.getter = g.clone();
    }
    if let Some(s) = &d.set {
        p.setter = s.clone();
    }
    if let Some(e) = d.enumerable {
        p.enumerable = e;
    }
    if let Some(c) = d.configurable {
        p.configurable = c;
    }
    p
}

/// A key as the engine shows it in a message: `"b"`, or `0` for an index.
pub fn shown_key(key: &str) -> String {
    if array_index(key).is_some() { key.to_string() } else { format!("\"{key}\"") }
}

fn get_own_property_descriptor(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let o = it.object_of(&arg(a, 0))?;
    let k = it.key_of(&arg(a, 1))?;
    match own_prop(&o, &k) {
        Some(p) => descriptor_object(it, &p),
        None => Ok(Val::Undef),
    }
}

/// Every own key of `o`: the string keys in order, then the symbol keys.
fn all_keys(it: &mut Interp, o: &ObjRef) -> R<Vec<Name>> {
    let mut keys = it.keys(o, false)?;
    if matches!(o.borrow().kind, Kind::Array(_) | Kind::Str(_)) {
        keys.push("length".into());
    }
    keys.extend(symbol_keys(o));
    Ok(keys)
}

/// The enumerable symbol keys of `o` (object spread, `Object.assign`).
pub fn enumerable_symbol_keys(o: &ObjRef) -> Vec<Name> {
    let b = o.borrow();
    b.props
        .iter()
        .filter(|(k, p)| p.enumerable && is_symbol_key(k))
        .map(|(k, _)| k.clone())
        .collect()
}

fn symbol_keys(o: &ObjRef) -> Vec<Name> {
    o.borrow().props.iter().filter(|(k, _)| is_symbol_key(k)).map(|(k, _)| k.clone()).collect()
}

fn get_own_property_descriptors(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let o = it.object_of(&arg(a, 0))?;
    let out = it.object()?;
    for k in all_keys(it, &o)? {
        if let Some(p) = own_prop(&o, &k) {
            let d = descriptor_object(it, &p)?;
            it.define(&out, &k, Prop::data(d))?;
        }
    }
    Ok(Val::Obj(out))
}

fn get_own_property_symbols(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let o = it.object_of(&arg(a, 0))?;
    let syms = symbol_keys(&o).iter().map(|k| symbol_of_key(k)).collect();
    it.array(syms)
}

fn define_property(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let Val::Obj(o) = arg(a, 0) else {
        return it.type_error(&format!("{} is not a non-null object", it.show(&arg(a, 0))));
    };
    let k = it.key_of(&arg(a, 1))?;
    let d = to_desc(it, &arg(a, 2))?;
    define_own(it, &o, &k, &d)?;
    Ok(Val::Obj(o))
}

/// `Object.defineProperties(o, props)`, and `Object.create`'s second argument.
pub fn define_all(it: &mut Interp, o: &ObjRef, props: &Val) -> R<()> {
    let src = it.object_of(props)?;
    let mut descs = Vec::new();
    for k in all_keys(it, &src)? {
        if own_prop(&src, &k).is_some_and(|p| p.enumerable) {
            let d = it.get(&Val::Obj(src.clone()), &k)?;
            descs.push((k, to_desc(it, &d)?));
        }
    }
    for (k, d) in descs {
        define_own(it, o, &k, &d)?;
    }
    Ok(())
}

fn define_properties(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let Val::Obj(o) = arg(a, 0) else {
        return it.type_error(&format!("{} is not a non-null object", it.show(&arg(a, 0))));
    };
    define_all(it, &o, &arg(a, 1))?;
    Ok(Val::Obj(o))
}

fn is_extensible(_: &mut Interp, _: &Val, a: &[Val]) -> R {
    Ok(Val::Bool(matches!(arg(a, 0), Val::Obj(o) if o.borrow().extensible)))
}

/// Whether `o` cannot grow and none of its properties can be deleted (and, for `frozen`, none
/// can be written).
pub fn is_locked(o: &ObjRef, frozen: bool) -> bool {
    let b = o.borrow();
    if b.extensible {
        return false;
    }
    let elements = match &b.kind {
        Kind::Array(items) => {
            let need = if frozen {
                LENGTH_FIXED | ELEMENTS_SEALED | ELEMENTS_FROZEN
            } else {
                ELEMENTS_SEALED
            };
            items.is_empty() && (!frozen || b.locks & LENGTH_FIXED != 0) || b.locks & need == need
        }
        Kind::Typed(t) => t.length() == 0,
        _ => true,
    };
    elements
        && b.props
            .iter()
            .all(|(_, p)| !p.configurable && (!frozen || p.is_accessor() || !p.writable))
}

fn is_sealed(_: &mut Interp, _: &Val, a: &[Val]) -> R {
    Ok(Val::Bool(match arg(a, 0) {
        Val::Obj(o) => is_locked(&o, false),
        _ => true,
    }))
}

fn is_frozen(_: &mut Interp, _: &Val, a: &[Val]) -> R {
    Ok(Val::Bool(match arg(a, 0) {
        Val::Obj(o) => is_locked(&o, true),
        _ => true,
    }))
}

/// `Object.seal` (`frozen` false) or `Object.freeze` of `o`.
pub fn lock(o: &ObjRef, frozen: bool) {
    let mut b = o.borrow_mut();
    b.extensible = false;
    b.locks |=
        if frozen { LENGTH_FIXED | ELEMENTS_SEALED | ELEMENTS_FROZEN } else { ELEMENTS_SEALED };
    for (_, p) in &mut b.props {
        p.configurable = false;
        if frozen && !p.is_accessor() {
            p.writable = false;
        }
    }
}

fn freeze(_: &mut Interp, _: &Val, a: &[Val]) -> R {
    if let Val::Obj(o) = arg(a, 0) {
        lock(&o, true);
    }
    Ok(arg(a, 0))
}

fn seal(_: &mut Interp, _: &Val, a: &[Val]) -> R {
    if let Val::Obj(o) = arg(a, 0) {
        lock(&o, false);
    }
    Ok(arg(a, 0))
}

fn prevent_extensions(_: &mut Interp, _: &Val, a: &[Val]) -> R {
    if let Val::Obj(o) = arg(a, 0) {
        o.borrow_mut().extensible = false;
    }
    Ok(arg(a, 0))
}

fn property_is_enumerable(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let k = it.key_of(&arg(a, 0))?;
    let o = it.object_of(this)?;
    Ok(Val::Bool(own_prop(&o, &k).is_some_and(|p| p.enumerable)))
}

/// CreateDataProperty (ECMA-262 §7.3.5): `o[key] = v` as a new enumerable, writable and
/// configurable property; `false` when `o` refuses it.
pub fn create_data_property(it: &mut Interp, o: &ObjRef, key: &str, v: Val) -> R<bool> {
    let d = Desc {
        value: Some(v),
        writable: Some(true),
        enumerable: Some(true),
        configurable: Some(true),
        ..Desc::default()
    };
    if crate::proxy::is_proxy(o) {
        let obj = desc_object(it, &d)?;
        return crate::proxy::define(it, o, key, obj);
    }
    match own_prop(o, key) {
        Some(c) if !c.configurable => return Ok(false),
        None if !o.borrow().extensible => return Ok(false),
        _ => {}
    }
    define_own(it, o, key, &d)?;
    Ok(true)
}

/// The object form of `d`.
fn desc_object(it: &mut Interp, d: &Desc) -> R {
    let o = it.object()?;
    let fields: [(&str, Option<Val>); 4] = [
        ("value", d.value.clone()),
        ("writable", d.writable.map(Val::Bool)),
        ("enumerable", d.enumerable.map(Val::Bool)),
        ("configurable", d.configurable.map(Val::Bool)),
    ];
    for (k, v) in fields {
        if let Some(v) = v {
            it.define(&o, k, Prop::data(v))?;
        }
    }
    Ok(Val::Obj(o))
}
