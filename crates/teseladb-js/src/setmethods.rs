//! The set methods of `Set.prototype` (`union`, `intersection`…) and `Map.groupBy`, which the
//! engine of a `refrouter` 9.0.4 has (measured on 02-11), as ECMA-262 §24.2.4 and §24.1.2.1 write
//! them: the other set is read through its `size`, `has` and `keys`.

use crate::builtins::method;
use crate::interp::{Interp, R};
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
        return it.type_error(&format!("{} is not an object", it.show(v)));
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
            return it.type_error("keys method must return an object");
        }
        let next = it.get(&iter, "next")?;
        loop {
            let step = it.call(&next, &iter, &[])?;
            if !matches!(step, Val::Obj(_)) {
                return it.type_error(&format!("{} is not an object", it.show(&step)));
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

/// The elements of `this`, a `Set` (`union method called on incompatible Map` otherwise).
fn this_set(it: &mut Interp, this: &Val) -> R<Vec<Val>> {
    if let Val::Obj(o) = this
        && let Kind::Set(e) = &o.borrow().kind
    {
        return Ok(e.clone());
    }
    let name = it.native.clone();
    let class = crate::builtins::class_name(this);
    it.type_error(&format!("{name} method called on incompatible {class}"))
}

fn contains(items: &[Val], v: &Val) -> bool {
    items.iter().any(|x| same_value_zero(x, v))
}

fn new_set(it: &mut Interp, items: Vec<Val>) -> R {
    let o = it.alloc(Some(it.realm.set.clone()), Kind::Ordinary)?;
    it.grow(&o, items.len())?;
    o.borrow_mut().kind = Kind::Set(items);
    Ok(Val::Obj(o))
}

fn union(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mut out = this_set(it, this)?;
    let other = other_set(it, &arg(a, 0))?;
    other.each(it, |_, v| {
        if !contains(&out, &v) {
            out.push(v);
        }
        Ok(true)
    })?;
    new_set(it, out)
}

fn intersection(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mine = this_set(it, this)?;
    let other = other_set(it, &arg(a, 0))?;
    let mut out = Vec::new();
    if mine.len() as f64 <= other.size {
        for v in mine {
            if other.has(it, &v)? && !contains(&out, &v) {
                out.push(v);
            }
        }
    } else {
        other.each(it, |_, v| {
            if contains(&mine, &v) && !contains(&out, &v) {
                out.push(v);
            }
            Ok(true)
        })?;
    }
    new_set(it, out)
}

fn difference(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mine = this_set(it, this)?;
    let other = other_set(it, &arg(a, 0))?;
    let mut out = mine.clone();
    if mine.len() as f64 <= other.size {
        for v in mine {
            if other.has(it, &v)? {
                out.retain(|x| !same_value_zero(x, &v));
            }
        }
    } else {
        other.each(it, |_, v| {
            out.retain(|x| !same_value_zero(x, &v));
            Ok(true)
        })?;
    }
    new_set(it, out)
}

fn symmetric_difference(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mine = this_set(it, this)?;
    let other = other_set(it, &arg(a, 0))?;
    let mut out = mine.clone();
    other.each(it, |_, v| {
        if contains(&mine, &v) {
            out.retain(|x| !same_value_zero(x, &v));
        } else if !contains(&out, &v) {
            out.push(v);
        }
        Ok(true)
    })?;
    new_set(it, out)
}

fn is_subset_of(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mine = this_set(it, this)?;
    let other = other_set(it, &arg(a, 0))?;
    if mine.len() as f64 > other.size {
        return Ok(Val::Bool(false));
    }
    for v in mine {
        if !other.has(it, &v)? {
            return Ok(Val::Bool(false));
        }
    }
    Ok(Val::Bool(true))
}

fn is_superset_of(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mine = this_set(it, this)?;
    let other = other_set(it, &arg(a, 0))?;
    if (mine.len() as f64) < other.size {
        return Ok(Val::Bool(false));
    }
    let mut all = true;
    other.each(it, |_, v| {
        all = contains(&mine, &v);
        Ok(all)
    })?;
    Ok(Val::Bool(all))
}

fn is_disjoint_from(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mine = this_set(it, this)?;
    let other = other_set(it, &arg(a, 0))?;
    if mine.len() as f64 <= other.size {
        for v in mine {
            if other.has(it, &v)? {
                return Ok(Val::Bool(false));
            }
        }
        return Ok(Val::Bool(true));
    }
    let mut disjoint = true;
    other.each(it, |_, v| {
        disjoint = !contains(&mine, &v);
        Ok(disjoint)
    })?;
    Ok(Val::Bool(disjoint))
}

/// `Map.groupBy(items, callback)`: a `Map` from each key the callback gives to its items.
fn group_by(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let f = arg(a, 1);
    if !f.is_callable() {
        return it.type_error(&format!("{} is not a function", it.show(&f)));
    }
    let items = it.iterate(&arg(a, 0))?;
    let mut groups: Vec<(Val, Vec<Val>)> = Vec::new();
    for (i, v) in items.into_iter().enumerate() {
        let k = it.call(&f, &Val::Undef, &[v.clone(), Val::Num(i as f64)])?;
        let k = if matches!(k, Val::Num(n) if n == 0.0) { Val::Num(0.0) } else { k };
        match groups.iter_mut().find(|(x, _)| same_value_zero(x, &k)) {
            Some((_, list)) => list.push(v),
            None => groups.push((k, vec![v])),
        }
    }
    let mut entries = Vec::with_capacity(groups.len());
    for (k, list) in groups {
        entries.push((k, it.array(list)?));
    }
    let o = it.alloc(Some(it.realm.map.clone()), Kind::Ordinary)?;
    it.grow(&o, entries.len() * 2)?;
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
    if let Some(Val::Obj(map)) = g.borrow().get_own("Map").map(|p| p.value.clone()) {
        method(it, &map, "groupBy", group_by, 2)?;
    }
    Ok(())
}
