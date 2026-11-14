//! The set methods of `Set.prototype` (`union`, `intersection`…) and `Map.groupBy`, which the
//! engine of a reference router 9.0.4 has (measured on 02-11), as ECMA-262 §24.2.4 and §24.1.2.1
//! write them: the other set is read through its `size`, `has` and `keys`.

use crate::builtins::method;
use crate::interp::{Interp, R};
use crate::keyed::Table;
use crate::value::*;

fn arg(a: &[Val], i: usize) -> Val {
    a.get(i).cloned().unwrap_or(Val::Undef)
}

/// GetSetRecord: the other set's size and its `has` and `keys` methods.
struct Other {
    obj: Val,
    size: f64,
    has: Val,
    keys: Val,
}

fn other_set(it: &mut Interp, v: &Val) -> R<Other> {
    if !matches!(v, Val::Obj(_)) {
        return it.type_error(&crate::builtins::non_object(v));
    }
    let raw = it.get(v, "size")?;
    let size = it.number_of(&raw)?;
    if size.is_nan() {
        let shown = if matches!(raw, Val::Undef) { "undefined" } else { "NaN" };
        return it.type_error(&format!("size is {shown}"));
    }
    let size = size.trunc();
    if size < 0.0 {
        return Err(it.error("RangeError", "Set size must be non-negative"));
    }
    let has = it.get(v, "has")?;
    if !has.is_callable() {
        return it.type_error("has property is not callable");
    }
    let keys = it.get(v, "keys")?;
    if !keys.is_callable() {
        return it.type_error("keys property is not callable");
    }
    Ok(Other { obj: v.clone(), size, has, keys })
}

impl Other {
    fn has(&self, it: &mut Interp, v: &Val) -> R<bool> {
        let r = it.call(&self.has, &self.obj, std::slice::from_ref(v))?;
        Ok(r.truthy())
    }

    /// The values of `keys()`, one at a time, until `stop` says so.
    fn each(&self, it: &mut Interp, mut f: impl FnMut(&mut Interp, Val) -> R<bool>) -> R<()> {
        let iter = it.call(&self.keys, &self.obj, &[])?;
        if !matches!(iter, Val::Obj(_)) {
            return it.type_error(&crate::builtins::non_object(&iter));
        }
        let next = it.get(&iter, "next")?;
        loop {
            let step = it.call(&next, &iter, &[])?;
            if !matches!(step, Val::Obj(_)) {
                return it.type_error("iterator.next() returned a non-object value");
            }
            if it.get(&step, "done")?.truthy() {
                return Ok(());
            }
            let v = it.get(&step, "value")?;
            let v = if matches!(v, Val::Num(n) if n == 0.0) { Val::Num(0.0) } else { v };
            if !f(it, v)? {
                return Ok(());
            }
        }
    }
}

/// `this`, a `Set` (`union method called on incompatible Map` otherwise).
fn this_set(it: &mut Interp, this: &Val) -> R<ObjRef> {
    if let Val::Obj(o) = this
        && let Kind::Set(_) = &o.borrow().kind
    {
        return Ok(o.clone());
    }
    let name = it.native.clone();
    let class = crate::builtins::class_name(this);
    it.type_error(&format!("{name} method called on incompatible {class}"))
}

/// The values of the `Set` `o` now.
fn values(o: &ObjRef) -> Table<Val> {
    match &o.borrow().kind {
        Kind::Set(e) => e.clone(),
        _ => Table::default(),
    }
}

/// How many values the `Set` `o` has now.
fn size(o: &ObjRef) -> f64 {
    match &o.borrow().kind {
        Kind::Set(e) => e.len() as f64,
        _ => 0.0,
    }
}

/// Whether the `Set` `o` has `v` now.
fn has(o: &ObjRef, v: &Val) -> bool {
    matches!(&o.borrow().kind, Kind::Set(e) if e.contains(v))
}

/// Calls `f` with each value of the `Set` `o` as it goes: values added meanwhile are visited and
/// deleted ones are not (ECMA-262 §24.2.4: `thisSize` is read again after each call), until `f`
/// says to stop.
fn each_live(it: &mut Interp, o: &ObjRef, mut f: impl FnMut(&mut Interp, Val) -> R<bool>) -> R<()> {
    let cursor = crate::builtins::coll_cursor(o);
    while let Some((v, _)) = crate::builtins::coll_next(o, &cursor) {
        if !f(it, v)? {
            break;
        }
    }
    Ok(())
}

fn new_set(it: &mut Interp, items: Table<Val>) -> R {
    let o = it.alloc(Some(it.realm.set.clone()), Kind::Ordinary)?;
    it.grow_elements(&o, items.len())?;
    o.borrow_mut().kind = Kind::Set(items);
    Ok(Val::Obj(o))
}

fn union(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = this_set(it, this)?;
    let other = other_set(it, &arg(a, 0))?;
    let mut out = values(&o);
    other.each(it, |_, v| {
        if !out.contains(&v) {
            out.push(v);
        }
        Ok(true)
    })?;
    new_set(it, out)
}

fn intersection(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = this_set(it, this)?;
    let other = other_set(it, &arg(a, 0))?;
    let mut out = Table::default();
    if size(&o) <= other.size {
        each_live(it, &o, |it, v| {
            if other.has(it, &v)? && !out.contains(&v) {
                out.push(v);
            }
            Ok(true)
        })?;
    } else {
        other.each(it, |_, v| {
            if has(&o, &v) && !out.contains(&v) {
                out.push(v);
            }
            Ok(true)
        })?;
    }
    new_set(it, out)
}

fn difference(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = this_set(it, this)?;
    let other = other_set(it, &arg(a, 0))?;
    let mut out = values(&o);
    if out.len() as f64 <= other.size {
        // The values of the copy, which `has` cannot change.
        for v in out.iter().cloned().collect::<Vec<_>>() {
            if other.has(it, &v)? {
                out.remove(&v);
            }
        }
    } else {
        other.each(it, |_, v| {
            out.remove(&v);
            Ok(true)
        })?;
    }
    new_set(it, out)
}

fn symmetric_difference(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = this_set(it, this)?;
    let other = other_set(it, &arg(a, 0))?;
    let mut out = values(&o);
    other.each(it, |_, v| {
        if has(&o, &v) {
            out.remove(&v);
        } else if !out.contains(&v) {
            out.push(v);
        }
        Ok(true)
    })?;
    new_set(it, out)
}

fn is_subset_of(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = this_set(it, this)?;
    let other = other_set(it, &arg(a, 0))?;
    if size(&o) > other.size {
        return Ok(Val::Bool(false));
    }
    let mut all = true;
    each_live(it, &o, |it, v| {
        all = other.has(it, &v)?;
        Ok(all)
    })?;
    Ok(Val::Bool(all))
}

fn is_superset_of(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = this_set(it, this)?;
    let other = other_set(it, &arg(a, 0))?;
    if size(&o) < other.size {
        return Ok(Val::Bool(false));
    }
    let mut all = true;
    other.each(it, |_, v| {
        all = has(&o, &v);
        Ok(all)
    })?;
    Ok(Val::Bool(all))
}

fn is_disjoint_from(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = this_set(it, this)?;
    let other = other_set(it, &arg(a, 0))?;
    let mut disjoint = true;
    if size(&o) <= other.size {
        each_live(it, &o, |it, v| {
            disjoint = !other.has(it, &v)?;
            Ok(disjoint)
        })?;
    } else {
        other.each(it, |_, v| {
            disjoint = !has(&o, &v);
            Ok(disjoint)
        })?;
    }
    Ok(Val::Bool(disjoint))
}

/// `Map.groupBy(items, callback)`: a `Map` from each key the callback gives to its items.
fn group_by(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let f = arg(a, 1);
    if !f.is_callable() {
        return it.type_error(&format!("{} is not a function", it.show(&f)));
    }
    let items = it.iterate(&arg(a, 0))?;
    let mut groups: Table<(Val, Vec<Val>)> = Table::default();
    for (i, v) in items.into_iter().enumerate() {
        let k = it.call(&f, &Val::Undef, &[v.clone(), Val::Num(i as f64)])?;
        let k = if matches!(k, Val::Num(n) if n == 0.0) { Val::Num(0.0) } else { k };
        match groups.get_mut(&k) {
            Some((_, list)) => list.push(v),
            None => groups.push((k, vec![v])),
        }
    }
    let mut entries = Table::default();
    for (k, list) in groups.iter().cloned() {
        entries.push((k, it.array(list)?));
    }
    let o = it.alloc(Some(it.realm.map.clone()), Kind::Ordinary)?;
    it.grow_elements(&o, entries.len() * 2)?;
    o.borrow_mut().kind = Kind::Map(entries);
    Ok(Val::Obj(o))
}

/// Installs the set methods on `Set.prototype` and `Map.groupBy` on `Map`.
pub fn install(it: &mut Interp, g: &ObjRef) -> R<()> {
    let sp = it.realm.set.clone();
    for (name, f) in [
        ("union", union as NativeFn),
        ("intersection", intersection),
        ("difference", difference),
        ("symmetricDifference", symmetric_difference),
        ("isSubsetOf", is_subset_of),
        ("isSupersetOf", is_superset_of),
        ("isDisjointFrom", is_disjoint_from),
    ] {
        method(it, &sp, name, f, 1)?;
    }
    method(it, &crate::builtins::installed(g, "Map"), "groupBy", group_by, 2)?;
    Ok(())
}
