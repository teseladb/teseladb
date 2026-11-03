//! `Array.prototype` (ECMA-262 §23.1.3).

use std::cell::RefCell;
use std::rc::Rc;

use crate::builtins::method;
use crate::interp::{Interp, PROP_BYTES, R};
use crate::value::*;

fn arg(args: &[Val], i: usize) -> Val {
    args.get(i).cloned().unwrap_or(Val::Undef)
}

pub fn install(it: &mut Interp, proto: &ObjRef) -> R<()> {
    for (name, f, n) in [
        ("push", push as NativeFn, 1),
        ("pop", pop, 0),
        ("shift", shift, 0),
        ("unshift", unshift, 1),
        ("slice", slice, 2),
        ("splice", splice, 2),
        ("concat", concat, 1),
        ("join", join, 1),
        ("toString", to_string, 0),
        ("toLocaleString", to_locale_string, 0),
        ("reverse", reverse, 0),
        ("indexOf", index_of, 1),
        ("lastIndexOf", last_index_of, 1),
        ("includes", includes, 1),
        ("find", find, 1),
        ("findIndex", find_index, 1),
        ("findLast", find_last, 1),
        ("findLastIndex", find_last_index, 1),
        ("filter", filter, 1),
        ("map", map, 1),
        ("forEach", for_each, 1),
        ("some", some, 1),
        ("every", every, 1),
        ("reduce", reduce, 1),
        ("reduceRight", reduce_right, 1),
        ("sort", sort, 1),
        ("toSorted", to_sorted, 1),
        ("toReversed", to_reversed, 0),
        ("toSpliced", to_spliced, 2),
        ("with", with, 2),
        ("flat", flat, 0),
        ("flatMap", flat_map, 1),
        ("fill", fill, 1),
        ("copyWithin", copy_within, 2),
        ("at", at, 1),
        ("keys", keys, 0),
        ("values", values, 0),
        ("entries", entries, 0),
    ] {
        method(it, proto, name, f, n)?;
    }
    Ok(())
}

/// The elements of `this` (an array, or an array-like object), a hole read from the prototypes.
fn items(it: &mut Interp, this: &Val) -> R<Vec<Val>> {
    if let Some(mut v) = own_elements(this) {
        for (i, x) in v.iter_mut().enumerate() {
            if matches!(x, Val::Hole) {
                *x = it.get(this, &key(i))?;
            }
        }
        return Ok(v);
    }
    generic_elements(it, this, false)
}

/// The elements of `this` with its holes ([`Val::Hole`]): the indices an array-like object does
/// not have are holes too (ECMA-262 §23.1.3: `forEach`, `map`, `filter`, `reduce`, `indexOf`…
/// skip them; measured on a `refrouter` 9.0.4 on 02-11).
fn elements(it: &mut Interp, this: &Val) -> R<Vec<Val>> {
    match own_elements(this) {
        Some(v) => Ok(v),
        None => generic_elements(it, this, true),
    }
}

/// The elements of an array whose elements have no attributes of their own.
fn own_elements(this: &Val) -> Option<Vec<Val>> {
    match this {
        Val::Obj(o)
            if !has_own_elements(o)
                && let Kind::Array(v) = &o.borrow().kind =>
        {
            Some(v.clone())
        }
        _ => None,
    }
}

/// The elements of `this` through `length` and `get` (and `has` when `holes`).
fn generic_elements(it: &mut Interp, this: &Val, holes: bool) -> R<Vec<Val>> {
    let obj = it.object_of(this)?;
    let o = Val::Obj(obj.clone());
    let n = it.get(&o, "length")?;
    let n = it.number_of(&n)?;
    let n = if n.is_finite() && n > 0.0 { (n as usize).min(1 << 24) } else { 0 };
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let k = key(i);
        out.push(if holes && !it.has(&obj, &k)? { Val::Hole } else { it.get(&o, &k)? });
    }
    Ok(out)
}

fn is_hole(v: &Val) -> bool {
    matches!(v, Val::Hole)
}

/// Whether some element of the array `o` is an own property with attributes of its own (an
/// accessor, a read-only value): the methods then go through `get` and `set`, which call its
/// getter and setter (ECMA-262 §23.1.3).
fn has_own_elements(o: &ObjRef) -> bool {
    o.borrow().locks & ELEMENTS_OWN != 0
}

fn key(i: usize) -> String {
    i.to_string()
}

/// Writes `values` to the first elements of `o` with `set`, deleting where one is a hole.
fn write_back(it: &mut Interp, o: &ObjRef, values: Vec<Val>) -> R<()> {
    for (i, x) in values.into_iter().enumerate() {
        write_back_at(it, o, i, x)?;
    }
    Ok(())
}

/// `pop` and `shift` through `get`, `set` and `delete`.
fn remove_generic(it: &mut Interp, o: &ObjRef, first: bool) -> R {
    let target = Val::Obj(o.clone());
    let len = it.array_len(o);
    if len == 0 {
        return Ok(Val::Undef);
    }
    let out = it.get(&target, &key(if first { 0 } else { len - 1 }))?;
    if first {
        for k in 1..len {
            move_element(it, o, k, k - 1)?;
        }
    }
    it.delete_key(o, &key(len - 1))?;
    it.set(&target, "length", Val::Num((len - 1) as f64))?;
    Ok(out)
}

/// Element `i` of `o` with `get`, or a hole when `o` does not have it.
fn read_at(it: &mut Interp, o: &ObjRef, i: usize) -> R {
    if it.has(o, &key(i))? { it.get(&Val::Obj(o.clone()), &key(i)) } else { Ok(Val::Hole) }
}

/// Element `from` of `o` written to `to` (deleted there when `from` is a hole).
fn move_element(it: &mut Interp, o: &ObjRef, from: usize, to: usize) -> R<()> {
    let x = read_at(it, o, from)?;
    write_back_at(it, o, to, x)
}

/// `reverse` through `get`, `set` and `delete`, pair by pair from the ends.
fn reverse_generic(it: &mut Interp, o: &ObjRef) -> R<()> {
    let len = it.array_len(o);
    for lower in 0..len / 2 {
        let upper = len - 1 - lower;
        let (a, b) = (read_at(it, o, lower)?, read_at(it, o, upper)?);
        write_back_at(it, o, lower, b)?;
        write_back_at(it, o, upper, a)?;
    }
    Ok(())
}

/// Writes `x` to element `i` of `o` with `set`, or deletes it when `x` is a hole.
fn write_back_at(it: &mut Interp, o: &ObjRef, i: usize, x: Val) -> R<()> {
    if is_hole(&x) {
        it.delete_key(o, &key(i))?;
        return Ok(());
    }
    it.set(&Val::Obj(o.clone()), &key(i), x)
}

fn array_obj(it: &Interp, this: &Val) -> R<ObjRef> {
    match this {
        Val::Obj(o) if matches!(o.borrow().kind, Kind::Array(_)) => Ok(o.clone()),
        _ => Err(crate::builtins::incompatible(it, "Array.prototype method")),
    }
}

/// How an array method changes the array it works on.
#[derive(Clone, Copy, PartialEq)]
enum Change {
    /// Elements are written in place (`sort`, `reverse`, `fill`).
    Write,
    /// Elements are added at the end (`push`, `unshift`).
    Grow,
    /// The last element is removed (`pop`, `splice`).
    Shrink,
    /// The first element is removed and the rest move (`shift`).
    Shift,
    /// Only `length` is written again (`push()`, `pop` of an empty array).
    Length,
}

/// The error of adding element `i` to an array whose length is read-only (`fixed`) or that is not
/// extensible.
pub fn cannot_grow(it: &Interp, fixed: bool, i: usize) -> crate::interp::Ctrl {
    let m = if fixed {
        "can't define array index property past the end of an array with non-writable length"
            .to_string()
    } else {
        format!("can't define property {i}: Array is not extensible")
    };
    it.error("TypeError", &m)
}

/// Refuses `change` to `o` when `Object.seal`, `Object.freeze` or `Object.preventExtensions` forbid
/// it, with the error of the first step the engine would take.
fn check(it: &Interp, o: &ObjRef, change: Change) -> R<()> {
    let (locks, extensible, len) = {
        let b = o.borrow();
        let len = match &b.kind {
            Kind::Array(v) => v.len(),
            _ => 0,
        };
        (b.locks, b.extensible, len)
    };
    let frozen = locks & ELEMENTS_FROZEN != 0;
    let sealed = locks & ELEMENTS_SEALED != 0;
    let read_only = || Err(it.error("TypeError", "0 is read-only"));
    let undeletable = |i: usize| {
        Err(it
            .error("TypeError", &format!("property {i} is non-configurable and can't be deleted")))
    };
    match change {
        Change::Grow if locks & LENGTH_FIXED != 0 || !extensible => {
            Err(cannot_grow(it, locks & LENGTH_FIXED != 0, len))
        }
        Change::Write if frozen && len > 0 => read_only(),
        Change::Shift if frozen && len > 1 => read_only(),
        Change::Shrink | Change::Shift if sealed && len > 0 => undeletable(len - 1),
        Change::Length if locks & LENGTH_FIXED != 0 => {
            Err(it.error("TypeError", "\"length\" is read-only"))
        }
        _ => Ok(()),
    }
}

/// After the last element of `o` is removed: when its `length` cannot be written, the length
/// stays with a hole at the end and the write of `length` fails (ECMA-262 §23.1.3.22).
fn keep_length(it: &mut Interp, o: &ObjRef) -> R<()> {
    let r = check(it, o, Change::Length);
    if r.is_err() {
        with_vec(o, |v| v.push(Val::Hole));
    }
    r
}

fn with_vec<T>(o: &ObjRef, f: impl FnOnce(&mut Vec<Val>) -> T) -> Option<T> {
    match &mut o.borrow_mut().kind {
        Kind::Array(v) => Some(f(v)),
        _ => None,
    }
}

/// A relative index (`-1` is the last) clamped to `[0, len]`.
fn relative(it: &mut Interp, v: &Val, len: usize, default: usize) -> R<usize> {
    if matches!(v, Val::Undef) {
        return Ok(default);
    }
    let n = it.number_of(v)?;
    let n = if n.is_nan() { 0.0 } else { n.trunc() };
    Ok(if n < 0.0 { (len as f64 + n).max(0.0) as usize } else { (n as usize).min(len) })
}

/// `push` on an object that is not an array (a proxy of one): through `length` and `set`.
fn push_generic(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = Val::Obj(it.object_of(this)?);
    let n = it.get(&o, "length")?;
    let mut n = it.number_of(&n)?;
    n = if n.is_finite() && n > 0.0 { n.trunc() } else { 0.0 };
    for x in a {
        it.set(&o, &crate::number::to_string(n), x.clone())?;
        n += 1.0;
    }
    it.set(&o, "length", Val::Num(n))?;
    Ok(Val::Num(n))
}

fn push(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    if !matches!(this, Val::Obj(o) if matches!(o.borrow().kind, Kind::Array(_))) {
        return push_generic(it, this, a);
    }
    let o = array_obj(it, this)?;
    check(it, &o, if a.is_empty() { Change::Length } else { Change::Grow })?;
    it.grow(&o, a.len())?;
    let n = with_vec(&o, |v| {
        v.extend_from_slice(a);
        v.len()
    });
    Ok(Val::Num(n.unwrap_or(0) as f64))
}

fn pop(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let o = array_obj(it, this)?;
    check(it, &o, Change::Shrink)?;
    if has_own_elements(&o) {
        return remove_generic(it, &o, false);
    }
    let len = it.array_len(&o);
    let out = with_vec(&o, Vec::pop).flatten().unwrap_or(Val::Undef);
    if len > 0 {
        keep_length(it, &o)?
    } else {
        check(it, &o, Change::Length)?
    }
    // A hole reads from the prototypes, where the index is now looked up anyway.
    if is_hole(&out) { it.get(this, &key(len - 1)) } else { Ok(out) }
}

fn shift(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let o = array_obj(it, this)?;
    check(it, &o, Change::Shift)?;
    if has_own_elements(&o) {
        return remove_generic(it, &o, true);
    }
    let first = with_vec(&o, |v| v.first().cloned()).flatten();
    let out = match first {
        Some(Val::Hole) => it.get(this, "0")?,
        Some(x) => x,
        None => {
            check(it, &o, Change::Length)?;
            return Ok(Val::Undef);
        }
    };
    with_vec(&o, |v| v.remove(0));
    keep_length(it, &o)?;
    Ok(out)
}

fn unshift(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = array_obj(it, this)?;
    check(it, &o, if a.is_empty() { Change::Length } else { Change::Grow })?;
    it.grow(&o, a.len())?;
    let n = with_vec(&o, |v| {
        v.splice(0..0, a.iter().cloned());
        v.len()
    });
    Ok(Val::Num(n.unwrap_or(0) as f64))
}

fn slice(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let v = elements(it, this)?;
    let start = relative(it, &arg(a, 0), v.len(), 0)?;
    let end = relative(it, &arg(a, 1), v.len(), v.len())?;
    let out = if start < end { v[start..end].to_vec() } else { Vec::new() };
    let target = species(it, this, out.len())?;
    species_result(it, target, out, true)
}

/// ArraySpeciesCreate (ECMA-262 §10.4.2.3): what `constructor[Symbol.species]` of an array
/// makes with `len`, or `None` for a plain array (a value that is not an array, a constructor
/// without species, or `Array` itself). Measured on a `refrouter` 9.0.4 on 02-11: `map`, `filter`,
/// `slice`, `splice`, `concat`, `flat` and `flatMap` read `constructor`, and a subclass of `Array`
/// gets its own instances.
fn species(it: &mut Interp, this: &Val, len: usize) -> R<Option<ObjRef>> {
    if !crate::proxy::is_array(it, this)? {
        return Ok(None);
    }
    let mut c = it.get(this, "constructor")?;
    if let Val::Obj(_) = c {
        c = match it.get(&c, "@@Symbol.species:0")? {
            Val::Null => Val::Undef,
            v => v,
        };
    }
    let array = it.realm.array.borrow().get_own("constructor").map(|p| p.value.clone());
    match &c {
        Val::Undef => return Ok(None),
        Val::Obj(o) if array.as_ref().is_some_and(|a| matches!(a, Val::Obj(x) if x.ptr_eq(o))) => {
            return Ok(None);
        }
        _ => {}
    }
    if !crate::interp::is_constructor(&c) {
        return it.type_error("constructor property is not a constructor");
    }
    match it.construct(&c, &[Val::Num(len as f64)], &c)? {
        Val::Obj(o) => Ok(Some(o)),
        _ => Ok(None),
    }
}

/// What `Array.from` and `Array.of` make when `this` is a constructor other than `Array` (a
/// subclass): `this` constructed with `args`; `None` for a plain array.
pub(crate) fn made_by(it: &mut Interp, this: &Val, args: &[Val]) -> R<Option<ObjRef>> {
    let Val::Obj(c) = this else { return Ok(None) };
    let is_array = it
        .realm
        .array
        .borrow()
        .get_own("constructor")
        .is_some_and(|p| matches!(&p.value, Val::Obj(a) if a.ptr_eq(c)));
    if is_array || !crate::interp::is_constructor(this) {
        return Ok(None);
    }
    match it.construct(this, args, this)? {
        Val::Obj(o) => Ok(Some(o)),
        _ => Ok(None),
    }
}

/// The result of a method that creates through the species: `items` in a plain array, or
/// defined one by one on what the species made, then its `length` when `with_length`.
pub(crate) fn species_result(
    it: &mut Interp,
    target: Option<ObjRef>,
    items: Vec<Val>,
    with_length: bool,
) -> R {
    let Some(t) = target else {
        return it.array(items);
    };
    let n = items.len();
    for (i, x) in items.into_iter().enumerate().filter(|(_, x)| !is_hole(x)) {
        crate::props::create_data_property_or_throw(it, &t, &key(i), x)?;
    }
    if with_length {
        it.set(&Val::Obj(t.clone()), "length", Val::Num(n as f64))?;
    }
    Ok(Val::Obj(t))
}

fn splice(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = array_obj(it, this)?;
    let len = it.array_len(&o);
    let start = relative(it, &arg(a, 0), len, 0)?;
    let count = match a.len() {
        0 => 0,
        1 => len - start,
        _ => {
            let c = it.number_of(&a[1])?;
            if c.is_nan() || c < 0.0 { 0 } else { (c as usize).min(len - start) }
        }
    };
    let insert = a.get(2..).unwrap_or(&[]).to_vec();
    if count > 0 || !insert.is_empty() {
        let change = if insert.len() > count { Change::Grow } else { Change::Shrink };
        if o.borrow().locks & ELEMENTS_FROZEN != 0 {
            check(it, &o, Change::Write)?;
        }
        check(it, &o, change)?;
    } else {
        check(it, &o, Change::Length)?;
    }
    it.grow(&o, insert.len())?;
    let target = species(it, this, count)?;
    let removed = with_vec(&o, |v| v.splice(start..start + count, insert).collect::<Vec<_>>())
        .unwrap_or_default();
    species_result(it, target, removed, true)
}

fn concat(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let target = species(it, this, 0)?;
    // `this` is the first item: spread when it is an array, else as an object.
    let mut out = if crate::proxy::is_array(it, this)? {
        elements(it, this)?
    } else {
        vec![Val::Obj(it.object_of(this)?)]
    };
    for x in a {
        match x {
            Val::Obj(o) if matches!(o.borrow().kind, Kind::Array(_)) => {
                out.extend(elements(it, x)?);
            }
            _ => out.push(x.clone()),
        }
    }
    it.charge(out.len() * PROP_BYTES)?;
    species_result(it, target, out, true)
}

fn join(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let sep = if matches!(arg(a, 0), Val::Undef) { ",".into() } else { it.str_of(&arg(a, 0))? };
    let v = items(it, this);
    // SpiderMonkey writes a `length` it cannot convert without the parentheses (`{}`).
    let v = it.rename_operand(v, |_, _, t| {
        t.strip_prefix('(').and_then(|t| t.strip_suffix(')')).unwrap_or(t).to_string()
    })?;
    let mut out = String::new();
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            out.push_str(&sep);
        }
        if !x.is_nullish() {
            let r = it.str_of(x);
            out.push_str(&it.operand_as_class(r)?);
        }
        it.charge(0)?;
    }
    it.string(out)
}

/// `join()`, or `Object.prototype.toString` when `join` is not a function (ECMA-262 §23.1.3.36).
fn to_string(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let o = Val::Obj(it.object_of(this)?);
    let f = it.get(&o, "join")?;
    if f.is_callable() {
        return it.call(&f, &o, &[]);
    }
    crate::builtins::obj_to_string(it, &o, &[])
}

/// Each element's `toLocaleString()`, joined with commas.
fn to_locale_string(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let v = items(it, this)?;
    let mut out = String::new();
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        if !x.is_nullish() {
            let f = it.get(x, "toLocaleString")?;
            if !f.is_callable() {
                // The names of SpiderMonkey's self-hosted code (measured on a `refrouter` 9.0.4).
                let which = if i == 0 { "firstElement" } else { "nextElement" };
                return it.type_error(&format!("{which}.toLocaleString is not a function"));
            }
            let s = it.call(&f, x, &[])?;
            out.push_str(&it.str_of(&s)?);
        }
        it.charge(0)?;
    }
    it.string(out)
}

fn reverse(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let o = array_obj(it, this)?;
    check(it, &o, Change::Write)?;
    if has_own_elements(&o) {
        reverse_generic(it, &o)?;
        return Ok(this.clone());
    }
    with_vec(&o, |v| v.reverse());
    Ok(this.clone())
}

fn index_of(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    // A hole is never strictly equal to anything.
    let v = elements(it, this)?;
    let from = relative(it, &arg(a, 1), v.len(), 0)?;
    let x = arg(a, 0);
    Ok(Val::Num(
        v.iter()
            .enumerate()
            .skip(from)
            .find(|(_, y)| strict_eq(y, &x))
            .map_or(-1.0, |(i, _)| i as f64),
    ))
}

fn last_index_of(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let v = elements(it, this)?;
    if v.is_empty() {
        return Ok(Val::Num(-1.0));
    }
    // From `fromIndex` down (ECMA-262 §23.1.3.20): the last element by default.
    let last = match a.get(1) {
        None => v.len() - 1,
        Some(n) => {
            let n = it.number_of(n)?;
            let n = if n.is_nan() { 0.0 } else { n.trunc() };
            let n = if n < 0.0 { v.len() as f64 + n } else { n.min((v.len() - 1) as f64) };
            if n < 0.0 {
                return Ok(Val::Num(-1.0));
            }
            n as usize
        }
    };
    let x = arg(a, 0);
    Ok(Val::Num(v[..=last].iter().rposition(|y| strict_eq(y, &x)).map_or(-1.0, |i| i as f64)))
}

fn includes(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let v = items(it, this)?;
    let from = relative(it, &arg(a, 1), v.len(), 0)?;
    let x = arg(a, 0);
    Ok(Val::Bool(v.iter().skip(from).any(|y| same_value_zero(y, &x))))
}

/// Calls `f(x, i, this)` on each element in order, stopping when `stop` says so; with `skip`
/// the holes are left out (`forEach`, `map`…), otherwise they are read as `undefined` (`find`…).
/// Gives the number of elements.
fn each(
    it: &mut Interp,
    this: &Val,
    a: &[Val],
    (rev, skip): (bool, bool),
    mut on: impl FnMut(&Val, usize, &Val) -> bool,
) -> R<usize> {
    let f = arg(a, 0);
    if !f.is_callable() {
        return Err(not_a_function(it, &f));
    }
    let v = if skip { elements(it, this)? } else { items(it, this)? };
    let order: Vec<usize> = if rev { (0..v.len()).rev().collect() } else { (0..v.len()).collect() };
    for i in order.into_iter().filter(|i| !is_hole(&v[*i])) {
        let r = it.call(&f, &arg(a, 1), &[v[i].clone(), Val::Num(i as f64), this.clone()])?;
        if on(&v[i], i, &r) {
            break;
        }
    }
    Ok(v.len())
}

/// «f is not a function» for a callback, which the call names by its expression.
fn not_a_function(it: &mut Interp, f: &Val) -> crate::interp::Ctrl {
    it.operand_error(f, |t| format!("{t} is not a function"))
}

fn find(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mut out = Val::Undef;
    each(it, this, a, (false, false), |x, _, r| {
        r.truthy() && {
            out = x.clone();
            true
        }
    })?;
    Ok(out)
}

fn find_index(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mut out = -1.0;
    each(it, this, a, (false, false), |_, i, r| {
        r.truthy() && {
            out = i as f64;
            true
        }
    })?;
    Ok(Val::Num(out))
}

fn find_last(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mut out = Val::Undef;
    each(it, this, a, (true, false), |x, _, r| {
        r.truthy() && {
            out = x.clone();
            true
        }
    })?;
    Ok(out)
}

fn find_last_index(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mut out = -1.0;
    each(it, this, a, (true, false), |_, i, r| {
        r.truthy() && {
            out = i as f64;
            true
        }
    })?;
    Ok(Val::Num(out))
}

fn filter(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let target = species(it, this, 0)?;
    let mut out = Vec::new();
    each(it, this, a, (false, true), |x, _, r| {
        if r.truthy() {
            out.push(x.clone());
        }
        false
    })?;
    it.charge(out.len() * PROP_BYTES)?;
    species_result(it, target, out, false)
}

fn map(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let len = match this {
        Val::Obj(o) if let Kind::Array(v) = &o.borrow().kind => v.len(),
        _ => 0,
    };
    let target = species(it, this, len)?;
    let out = mapped(it, this, a)?;
    species_result(it, target, out, false)
}

/// What the callback gives for each element, a hole where the element is one.
fn mapped(it: &mut Interp, this: &Val, a: &[Val]) -> R<Vec<Val>> {
    let mut out = Vec::new();
    let n = each(it, this, a, (false, true), |_, i, r| {
        out.resize(i, Val::Hole);
        out.push(r.clone());
        false
    })?;
    out.resize(n, Val::Hole);
    it.charge(out.len() * PROP_BYTES)?;
    Ok(out)
}

fn for_each(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    each(it, this, a, (false, true), |_, _, _| false)?;
    Ok(Val::Undef)
}

fn some(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mut out = false;
    each(it, this, a, (false, true), |_, _, r| {
        out = r.truthy();
        out
    })?;
    Ok(Val::Bool(out))
}

fn every(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mut out = true;
    each(it, this, a, (false, true), |_, _, r| {
        out = r.truthy();
        !out
    })?;
    Ok(Val::Bool(out))
}

fn reduce_in(it: &mut Interp, this: &Val, a: &[Val], rev: bool) -> R {
    let f = arg(a, 0);
    if !f.is_callable() {
        return Err(not_a_function(it, &f));
    }
    let v = elements(it, this)?;
    let mut order: Vec<usize> =
        if rev { (0..v.len()).rev().collect() } else { (0..v.len()).collect() };
    order.retain(|i| !is_hole(&v[*i]));
    let mut acc = if a.len() >= 2 {
        a[1].clone()
    } else if order.is_empty() {
        return it.type_error("reduce of empty array with no initial value");
    } else {
        v[order.remove(0)].clone()
    };
    for i in order {
        acc = it.call(&f, &Val::Undef, &[acc, v[i].clone(), Val::Num(i as f64), this.clone()])?;
    }
    Ok(acc)
}

fn reduce(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    reduce_in(it, this, a, false)
}

fn reduce_right(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    reduce_in(it, this, a, true)
}

/// A stable merge sort with the comparator (or the string order), undefined last.
fn sorted(it: &mut Interp, mut v: Vec<Val>, cmp: &Val) -> R<Vec<Val>> {
    let undef = v.iter().filter(|x| matches!(x, Val::Undef)).count();
    let holes = v.iter().filter(|x| is_hole(x)).count();
    v.retain(|x| !matches!(x, Val::Undef | Val::Hole));
    let mut keys = Vec::new();
    if !cmp.is_callable() && v.len() > 1 {
        for x in &v {
            let r = it.str_of(x);
            keys.push(it.operand_as_class(r)?);
        }
    }
    let mut idx: Vec<usize> = (0..v.len()).collect();
    // Insertion into runs, merge by halves: the comparator may throw, so no `sort_by`.
    let mut width = 1;
    let n = idx.len();
    let mut buf = idx.clone();
    while width < n {
        let mut start = 0;
        while start < n {
            let mid = (start + width).min(n);
            let end = (start + 2 * width).min(n);
            let (mut i, mut j, mut k) = (start, mid, start);
            while i < mid && j < end {
                let le = if cmp.is_callable() {
                    let r = it.call(cmp, &Val::Undef, &[v[idx[i]].clone(), v[idx[j]].clone()])?;
                    it.number_of(&r)?.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater)
                } else {
                    crate::strings::units(&keys[idx[i]]).cmp(&crate::strings::units(&keys[idx[j]]))
                        != std::cmp::Ordering::Greater
                };
                // `le`: element i goes first (stable).
                if le {
                    buf[k] = idx[i];
                    i += 1;
                } else {
                    buf[k] = idx[j];
                    j += 1;
                }
                k += 1;
            }
            while i < mid {
                buf[k] = idx[i];
                i += 1;
                k += 1;
            }
            while j < end {
                buf[k] = idx[j];
                j += 1;
                k += 1;
            }
            start = end;
        }
        std::mem::swap(&mut idx, &mut buf);
        width *= 2;
    }
    let mut out: Vec<Val> = idx.into_iter().map(|i| v[i].clone()).collect();
    out.extend(std::iter::repeat_n(Val::Undef, undef));
    out.extend(std::iter::repeat_n(Val::Hole, holes));
    Ok(out)
}

fn sort(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = array_obj(it, this)?;
    check(it, &o, Change::Write)?;
    let cmp = arg(a, 0);
    if !matches!(cmp, Val::Undef) && !cmp.is_callable() {
        return it.type_error("invalid Array.prototype.sort argument");
    }
    let v = elements(it, this)?;
    let s = sorted(it, v, &cmp)?;
    if has_own_elements(&o) {
        write_back(it, &o, s)?;
        return Ok(this.clone());
    }
    with_vec(&o, |v| *v = s);
    Ok(this.clone())
}

fn to_sorted(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let cmp = arg(a, 0);
    if !matches!(cmp, Val::Undef) && !cmp.is_callable() {
        return it.type_error("non-function passed to Array.prototype.toSorted");
    }
    let v = items(it, this)?;
    let s = sorted(it, v, &arg(a, 0))?;
    it.charge(s.len() * PROP_BYTES)?;
    it.array(s)
}

fn to_reversed(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let mut v = items(it, this)?;
    v.reverse();
    it.charge(v.len() * PROP_BYTES)?;
    it.array(v)
}

/// `toSpliced(start, skipCount, ...items)`: a copy with `splice` applied.
fn to_spliced(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mut v = items(it, this)?;
    let len = v.len();
    let start = relative(it, &arg(a, 0), len, 0)?;
    let skip = match a.len() {
        0 => 0,
        1 => len - start,
        _ => {
            let c = it.number_of(&a[1])?;
            if c.is_nan() || c < 0.0 { 0 } else { (c as usize).min(len - start) }
        }
    };
    let insert = a.get(2..).unwrap_or(&[]).to_vec();
    v.splice(start..start + skip, insert);
    it.charge(v.len() * PROP_BYTES)?;
    it.array(v)
}

/// `with(index, value)`: a copy with one element replaced.
fn with(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mut v = items(it, this)?;
    let n = it.number_of(&arg(a, 0))?;
    let n = if n.is_nan() { 0.0 } else { n.trunc() };
    let i = if n < 0.0 { v.len() as f64 + n } else { n };
    if i < 0.0 || i >= v.len() as f64 {
        return Err(it.error("RangeError", "invalid or out-of-range index"));
    }
    v[i as usize] = arg(a, 1);
    it.charge(v.len() * PROP_BYTES)?;
    it.array(v)
}

fn flatten(it: &mut Interp, v: Vec<Val>, depth: f64, out: &mut Vec<Val>) -> R<()> {
    for x in v.into_iter().filter(|x| !is_hole(x)) {
        let nested = matches!(&x, Val::Obj(o) if matches!(o.borrow().kind, Kind::Array(_)));
        if nested && depth >= 1.0 {
            let inner = elements(it, &x)?;
            flatten(it, inner, depth - 1.0, out)?;
        } else {
            it.charge(PROP_BYTES)?;
            out.push(x);
        }
    }
    Ok(())
}

fn flat(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let depth = match arg(a, 0) {
        Val::Undef => 1.0,
        d => {
            let r = it.number_of(&d);
            it.operand_as_source(r)?
        }
    };
    let v = elements(it, this)?;
    let target = species(it, this, 0)?;
    let mut out = Vec::new();
    flatten(it, v, depth, &mut out)?;
    species_result(it, target, out, false)
}

fn flat_map(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let target = species(it, this, 0)?;
    let v = mapped(it, this, a)?;
    let mut out = Vec::new();
    flatten(it, v, 1.0, &mut out)?;
    species_result(it, target, out, false)
}

/// A relative index argument, named as SpiderMonkey's self-hosted code names it when it cannot
/// be converted (`can't convert start to number`; measured on a `refrouter` 9.0.4 on 02-11).
fn named_relative(it: &mut Interp, v: &Val, len: usize, default: usize, name: &str) -> R<usize> {
    let r = relative(it, v, len, default);
    it.operand_named(r, name)
}

/// The `length` of an array-like `this` for `fill` and `copyWithin` (`O.length` in their
/// messages).
fn generic_len(it: &mut Interp, o: &Val) -> R<usize> {
    let n = it.get(o, "length")?;
    let n = it.number_of(&n);
    let n = it.operand_named(n, "O.length")?;
    Ok(if n.is_finite() && n > 0.0 { (n as usize).min(1 << 24) } else { 0 })
}

fn fill(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let x = arg(a, 0);
    if !matches!(this, Val::Obj(o) if matches!(o.borrow().kind, Kind::Array(_))) {
        let o = Val::Obj(it.object_of(this)?);
        let len = generic_len(it, &o)?;
        let start = named_relative(it, &arg(a, 1), len, 0, "start")?;
        let end = named_relative(it, &arg(a, 2), len, len, "end")?;
        for i in start..end {
            it.set(&o, &key(i), x.clone())?;
        }
        return Ok(o);
    }
    let o = array_obj(it, this)?;
    check(it, &o, Change::Write)?;
    let len = it.array_len(&o);
    let start = named_relative(it, &arg(a, 1), len, 0, "start")?;
    let end = named_relative(it, &arg(a, 2), len, len, "end")?;
    if has_own_elements(&o) {
        let target = Val::Obj(o.clone());
        for i in start..end {
            it.set(&target, &key(i), x.clone())?;
        }
        return Ok(this.clone());
    }
    with_vec(&o, |v| {
        for slot in v.iter_mut().take(end).skip(start) {
            *slot = x.clone();
        }
    });
    Ok(this.clone())
}

fn copy_within(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let generic = !matches!(this, Val::Obj(o) if matches!(o.borrow().kind, Kind::Array(_)));
    let o = it.object_of(this)?;
    let len = if generic {
        generic_len(it, &Val::Obj(o.clone()))?
    } else {
        check(it, &o, Change::Write)?;
        it.array_len(&o)
    };
    let target = named_relative(it, &arg(a, 0), len, 0, "target")?;
    let start = named_relative(it, &arg(a, 1), len, 0, "start")?;
    let end = named_relative(it, &arg(a, 2), len, len, "end")?;
    let count = end.saturating_sub(start).min(len - target);
    if generic {
        // Backwards when the ranges overlap that way (ECMA-262 §23.1.3.4).
        let order: Vec<usize> = if start < target && target < start + count {
            (0..count).rev().collect()
        } else {
            (0..count).collect()
        };
        for k in order {
            move_element(it, &o, start + k, target + k)?;
        }
        return Ok(Val::Obj(o));
    }
    with_vec(&o, |v| {
        let part: Vec<Val> = v[start..start + count].to_vec();
        v[target..target + count].clone_from_slice(&part);
    });
    Ok(this.clone())
}

fn at(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let v = items(it, this)?;
    let n = it.number_of(&arg(a, 0))?;
    let n = if n.is_nan() { 0.0 } else { n.trunc() };
    let i = if n < 0.0 { v.len() as f64 + n } else { n };
    if i < 0.0 || i >= v.len() as f64 {
        return Ok(Val::Undef);
    }
    Ok(v[i as usize].clone())
}

fn keys(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    array_iter(it, this, ArrayIterKind::Keys)
}

fn values(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    array_iter(it, this, ArrayIterKind::Values)
}

fn entries(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    array_iter(it, this, ArrayIterKind::Entries)
}

/// An iterator over `this`, which reads it at each step.
fn array_iter(it: &mut Interp, this: &Val, kind: ArrayIterKind) -> R {
    let source = Some(Val::Obj(it.object_of(this)?));
    let state = Rc::new(RefCell::new(ArrayIter { source, pos: 0, kind }));
    let proto = Some(it.realm.proto(crate::builtins::ARRAY_ITERATOR));
    Ok(Val::Obj(it.alloc(proto, Kind::ArrayIter(state))?))
}

/// The next step of an array iterator: its value, or `None` once the index reaches `length`.
pub(crate) fn array_iter_next(it: &mut Interp, state: &Rc<RefCell<ArrayIter>>) -> R<Option<Val>> {
    let (source, pos, kind) = {
        let s = state.borrow();
        let Some(source) = s.source.clone() else { return Ok(None) };
        (source, s.pos, s.kind)
    };
    let len = match &source {
        Val::Obj(o) if let Kind::Array(v) = &o.borrow().kind => v.len() as f64,
        _ => {
            let n = it.get(&source, "length")?;
            it.number_of(&n)?
        }
    };
    if len.is_nan() || pos as f64 >= len {
        state.borrow_mut().source = None;
        return Ok(None);
    }
    state.borrow_mut().pos = pos + 1;
    let index = Val::Num(pos as f64);
    Ok(Some(match kind {
        ArrayIterKind::Keys => index,
        ArrayIterKind::Values => it.get(&source, &key(pos))?,
        ArrayIterKind::Entries => {
            let x = it.get(&source, &key(pos))?;
            it.array(vec![index, x])?
        }
    }))
}
