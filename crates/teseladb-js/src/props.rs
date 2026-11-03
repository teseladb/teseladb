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
    pub(crate) fn is_accessor(&self) -> bool {
        self.get.is_some() || self.set.is_some()
    }

    pub(crate) fn is_data(&self) -> bool {
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
    touch(o, key);
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
                && !matches!(x, Val::Hole)
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
        Kind::Arguments(Some(m)) => {
            // An element tied to its parameter has the parameter's value.
            let current = m.name(key).and_then(|n| m.value(&n));
            return b.get_own(key).cloned().map(|p| match current {
                Some(value) => Prop { value, ..p },
                None => p,
            });
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

pub(crate) fn same_value(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::Num(x), Val::Num(y)) => {
            (x.is_nan() && y.is_nan()) || (x == y && x.is_sign_negative() == y.is_sign_negative())
        }
        _ => strict_eq(a, b),
    }
}

pub(crate) fn same_fn(a: &Option<ObjRef>, b: &Option<ObjRef>) -> bool {
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

/// `[[DefineOwnProperty]]` of an element of a typed array (ECMA-262 §10.4.5.3), with the
/// messages of a `refrouter` 9.0.4 (measured on 02-11): an element is writable, enumerable and
/// configurable, and nothing else can be made of it.
fn define_element(
    it: &mut Interp,
    t: &crate::typed::View,
    i: Option<usize>,
    key: &str,
    d: &Desc,
) -> R<()> {
    let Some(i) = i.filter(|i| *i < t.length()) else {
        return it.type_error("can't define element for invalid or out-of-range index");
    };
    if d.configurable == Some(false)
        || d.enumerable == Some(false)
        || d.is_accessor()
        || d.writable == Some(false)
    {
        let k = shown_key(it, key);
        return it.type_error(&format!("can't redefine non-configurable property {k}"));
    }
    if let Some(v) = &d.value {
        let x = crate::typed::convert(it, t.ty, v)?;
        crate::typed::write(t, i, &x);
    }
    Ok(())
}

/// ValidateAndApplyPropertyDescriptor for `o[key]`, throwing as `Object.defineProperty` does.
pub fn define_own(it: &mut Interp, o: &ObjRef, key: &str, d: &Desc) -> R<()> {
    let is_array = matches!(o.borrow().kind, Kind::Array(_));
    let typed = match &o.borrow().kind {
        Kind::Typed(t) => crate::typed::numeric_key(key).map(|i| (t.clone(), i)),
        _ => None,
    };
    if let Some((t, i)) = typed {
        return define_element(it, &t, i, key, d);
    }
    let cur = own_prop(o, key);
    let shown = shown_key(it, key);
    match &cur {
        None if is_array && array_index(key).is_some() && o.borrow().locks & LENGTH_FIXED != 0 => {
            return Err(crate::arrays::cannot_grow(it, true, array_index(key).unwrap_or(0)));
        }
        None if !o.borrow().extensible => {
            let class = crate::builtins::object_class(it, o);
            return it
                .type_error(&format!("can't define property {shown}: {class} is not extensible"));
        }
        // A change of the value or writability of a fixed `length`; the attributes of a
        // non-configurable property give the general message (measured on 03-11).
        Some(c)
            if forbidden(c, d)
                && is_array
                && key == "length"
                && d.configurable != Some(true)
                && d.writable != Some(true)
                && d.enumerable.is_none_or(|e| e == c.enumerable)
                && !d.is_accessor() =>
        {
            return it.type_error("can't redefine array length");
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
    let Some((m, n)) = mapped_element(o, key) else { return it.define(o, key, p) };
    // An element of a mapped `arguments` that stays tied to its parameter (§10.4.4.2).
    if d.is_data() && d.value.is_none() && d.writable == Some(false) {
        p.value = m.value(&n).unwrap_or(Val::Undef);
    }
    it.define(o, key, p)?;
    if let Some(v) = &d.value {
        m.set(&n, v.clone());
    }
    if d.is_accessor() || d.writable == Some(false) {
        m.unmap(key);
    }
    Ok(())
}

/// The mapping and parameter of element `key` of a mapped `arguments` object.
fn mapped_element(o: &ObjRef, key: &str) -> Option<(Rc<crate::interp::Mapped>, Name)> {
    let b = o.borrow();
    let Kind::Arguments(Some(m)) = &b.kind else { return None };
    let n = m.name(key)?;
    Some((m.clone(), n))
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
        it.grow_elements(o, i + 1 - it.array_len(o))?;
    }
    if let Kind::Array(items) = &mut o.borrow_mut().kind {
        if i >= items.len() {
            items.resize(i + 1, Val::Hole);
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

/// A key as the engine shows it in a message: `"b"`, `0` for an index, and a symbol as it is
/// written: `Symbol.iterator`, `Symbol.for("r")`, `Symbol("k")` or `Symbol()` (measured on a
/// `refrouter` 9.0.4 on 02-11).
pub fn shown_key(it: &Interp, key: &str) -> String {
    if let Val::Sym(sym) = symbol_of_key(key)
        && key.starts_with("@@")
    {
        let (desc, id) = (&sym.0, sym.1);
        return if id == 0 {
            desc.to_string()
        } else if it.registry.iter().any(|(_, s)| matches!(s, Val::Sym(r) if r.1 == id)) {
            format!("Symbol.for(\"{desc}\")")
        } else if it.undescribed.contains(&id) {
            "Symbol()".into()
        } else {
            format!("Symbol(\"{desc}\")")
        };
    }
    if array_index(key).is_some() { key.to_string() } else { format!("\"{key}\"") }
}

fn get_own_property_descriptor(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let o = it.object_of(&arg(a, 0))?;
    let k = it.key_of(&arg(a, 1))?;
    let p = if crate::proxy::is_proxy(&o) {
        crate::proxy::own_property(it, &o, &k)?
    } else {
        own_prop(&o, &k)
    };
    match p {
        Some(p) => descriptor_object(it, &p),
        None => Ok(Val::Undef),
    }
}

/// Every own key of `o`: the string keys in order, then the symbol keys.
fn all_keys(it: &mut Interp, o: &ObjRef) -> R<Vec<Name>> {
    let mut keys = it.keys(o, false)?;
    if matches!(o.borrow().kind, Kind::Array(_) | Kind::Str(_)) {
        // After the indices and before the other keys, which come after it (ECMA-262 §10.1.11.1).
        let at = keys.iter().take_while(|k| array_index(k).is_some()).count();
        keys.insert(at, "length".into());
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

pub(crate) fn symbol_keys(o: &ObjRef) -> Vec<Name> {
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
    let keys = if crate::proxy::is_proxy(&o) {
        crate::proxy::own_keys(it, &o)?.into_iter().filter(|k| is_symbol_key(k)).collect()
    } else {
        symbol_keys(&o)
    };
    let syms = keys.iter().map(|k| symbol_of_key(k)).collect();
    it.array(syms)
}

fn define_property(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let Val::Obj(o) = arg(a, 0) else {
        return it.type_error(&format!("{} is not a non-null object", it.show(&arg(a, 0))));
    };
    let k = it.key_of(&arg(a, 1))?;
    if crate::proxy::is_proxy(&o) {
        if !crate::proxy::define(it, &o, &k, arg(a, 2))? {
            let k = shown_key(it, &k);
            return it.type_error(&format!(
                "proxy defineProperty handler returned false for property '{k}'"
            ));
        }
        return Ok(Val::Obj(o));
    }
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
        return Err(it.operand_error(&arg(a, 0), |t| format!("{t} is not an object")));
    };
    define_all(it, &o, &arg(a, 1))?;
    Ok(Val::Obj(o))
}

fn is_extensible(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    Ok(Val::Bool(match arg(a, 0) {
        Val::Obj(o) if crate::proxy::is_proxy(&o) => crate::proxy::is_extensible(it, &o)?,
        Val::Obj(o) => o.borrow().extensible,
        _ => false,
    }))
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

fn is_sealed(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    Ok(Val::Bool(match arg(a, 0) {
        Val::Obj(o) if crate::proxy::is_proxy(&o) => crate::proxy::is_locked(it, &o, false)?,
        Val::Obj(o) => is_locked(&o, false),
        _ => true,
    }))
}

fn is_frozen(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    Ok(Val::Bool(match arg(a, 0) {
        Val::Obj(o) if crate::proxy::is_proxy(&o) => crate::proxy::is_locked(it, &o, true)?,
        Val::Obj(o) => is_locked(&o, true),
        _ => true,
    }))
}

/// `Object.seal` (`frozen` false) or `Object.freeze` of `o`.
pub fn lock(o: &ObjRef, frozen: bool) {
    let mut b = o.borrow_mut();
    b.extensible = false;
    b.locks &= !LAZY;
    b.locks |=
        if frozen { LENGTH_FIXED | ELEMENTS_SEALED | ELEMENTS_FROZEN } else { ELEMENTS_SEALED };
    for (_, p) in &mut b.props {
        p.configurable = false;
        if frozen && !p.is_accessor() {
            p.writable = false;
        }
    }
}

fn freeze(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    match arg(a, 0) {
        Val::Obj(o) if crate::proxy::is_proxy(&o) => crate::proxy::lock(it, &o, true)?,
        Val::Obj(o) => lock(&o, true),
        _ => {}
    }
    Ok(arg(a, 0))
}

fn seal(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    match arg(a, 0) {
        Val::Obj(o) if crate::proxy::is_proxy(&o) => crate::proxy::lock(it, &o, false)?,
        Val::Obj(o) => lock(&o, false),
        _ => {}
    }
    Ok(arg(a, 0))
}

fn prevent_extensions(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    match arg(a, 0) {
        Val::Obj(o) if crate::proxy::is_proxy(&o) => {
            if !crate::proxy::prevent_extensions(it, &o)? {
                return it.type_error("proxy preventExtensions handler returned false");
            }
        }
        Val::Obj(o) => o.borrow_mut().extensible = false,
        _ => {}
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

/// CreateDataPropertyOrThrow (ECMA-262 §7.3.7): the errors of `defineProperty` when `o` refuses
/// the property.
pub fn create_data_property_or_throw(it: &mut Interp, o: &ObjRef, key: &str, v: Val) -> R<()> {
    if crate::proxy::is_proxy(o) {
        create_data_property(it, o, key, v)?;
        return Ok(());
    }
    let d = Desc {
        value: Some(v),
        writable: Some(true),
        enumerable: Some(true),
        configurable: Some(true),
        ..Desc::default()
    };
    define_own(it, o, key, &d)
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
