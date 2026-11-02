//! `Array.prototype` (ECMA-262 §23.1.3).

use crate::builtins::{make_iter, method};
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
        ("flat", flat, 0),
        ("flatMap", flat_map, 1),
        ("fill", fill, 1),
        ("at", at, 1),
        ("keys", keys, 0),
        ("values", values, 0),
        ("entries", entries, 0),
    ] {
        method(it, proto, name, f, n)?;
    }
    Ok(())
}

/// The elements of `this` (an array, or an array-like object).
fn items(it: &mut Interp, this: &Val) -> R<Vec<Val>> {
    if let Val::Obj(o) = this
        && let Kind::Array(v) = &o.borrow().kind
    {
        return Ok(v.clone());
    }
    let o = Val::Obj(it.object_of(this)?);
    let n = it.get(&o, "length")?;
    let n = it.number_of(&n)?;
    let n = if n.is_finite() && n > 0.0 { (n as usize).min(1 << 24) } else { 0 };
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(it.get(&o, &i.to_string())?);
    }
    Ok(out)
}

fn array_obj(it: &Interp, this: &Val) -> R<ObjRef> {
    match this {
        Val::Obj(o) if matches!(o.borrow().kind, Kind::Array(_)) => Ok(o.clone()),
        _ => Err(crate::builtins::incompatible(it, "Array.prototype method")),
    }
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
    it.grow(&o, a.len())?;
    let n = with_vec(&o, |v| {
        v.extend_from_slice(a);
        v.len()
    });
    Ok(Val::Num(n.unwrap_or(0) as f64))
}

fn pop(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let o = array_obj(it, this)?;
    Ok(with_vec(&o, Vec::pop).flatten().unwrap_or(Val::Undef))
}

fn shift(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let o = array_obj(it, this)?;
    Ok(with_vec(&o, |v| if v.is_empty() { None } else { Some(v.remove(0)) })
        .flatten()
        .unwrap_or(Val::Undef))
}

fn unshift(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = array_obj(it, this)?;
    it.grow(&o, a.len())?;
    let n = with_vec(&o, |v| {
        v.splice(0..0, a.iter().cloned());
        v.len()
    });
    Ok(Val::Num(n.unwrap_or(0) as f64))
}

fn slice(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let v = items(it, this)?;
    let start = relative(it, &arg(a, 0), v.len(), 0)?;
    let end = relative(it, &arg(a, 1), v.len(), v.len())?;
    let out = if start < end { v[start..end].to_vec() } else { Vec::new() };
    it.array(out)
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
    it.grow(&o, insert.len())?;
    let removed = with_vec(&o, |v| v.splice(start..start + count, insert).collect::<Vec<_>>())
        .unwrap_or_default();
    it.array(removed)
}

fn concat(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mut out = items(it, this)?;
    for x in a {
        match x {
            Val::Obj(o) if matches!(o.borrow().kind, Kind::Array(_)) => out.extend(items(it, x)?),
            _ => out.push(x.clone()),
        }
    }
    it.charge(out.len() * PROP_BYTES)?;
    it.array(out)
}

fn join(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let sep = if matches!(arg(a, 0), Val::Undef) { ",".into() } else { it.str_of(&arg(a, 0))? };
    let v = items(it, this)?;
    let mut out = String::new();
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            out.push_str(&sep);
        }
        if !x.is_nullish() {
            out.push_str(&it.str_of(x)?);
        }
        it.charge(0)?;
    }
    it.string(out)
}

fn to_string(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    join(it, this, &[])
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
            let s = it.call(&f, x, &[])?;
            out.push_str(&it.str_of(&s)?);
        }
        it.charge(0)?;
    }
    it.string(out)
}

fn reverse(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let o = array_obj(it, this)?;
    with_vec(&o, |v| v.reverse());
    Ok(this.clone())
}

fn index_of(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let v = items(it, this)?;
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
    let v = items(it, this)?;
    let x = arg(a, 0);
    Ok(Val::Num(v.iter().rposition(|y| strict_eq(y, &x)).map_or(-1.0, |i| i as f64)))
}

fn includes(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let v = items(it, this)?;
    let from = relative(it, &arg(a, 1), v.len(), 0)?;
    let x = arg(a, 0);
    Ok(Val::Bool(v.iter().skip(from).any(|y| same_value_zero(y, &x))))
}

/// Calls `f(x, i, this)` on each element in order, stopping when `stop` says so.
fn each(
    it: &mut Interp,
    this: &Val,
    a: &[Val],
    rev: bool,
    mut on: impl FnMut(&Val, usize, &Val) -> bool,
) -> R<()> {
    let f = arg(a, 0);
    if !f.is_callable() {
        return it.type_error(&format!("{} is not a function", it.show(&f)));
    }
    let v = items(it, this)?;
    let order: Vec<usize> = if rev { (0..v.len()).rev().collect() } else { (0..v.len()).collect() };
    for i in order {
        let r = it.call(&f, &arg(a, 1), &[v[i].clone(), Val::Num(i as f64), this.clone()])?;
        if on(&v[i], i, &r) {
            break;
        }
    }
    Ok(())
}

fn find(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mut out = Val::Undef;
    each(it, this, a, false, |x, _, r| {
        r.truthy() && {
            out = x.clone();
            true
        }
    })?;
    Ok(out)
}

fn find_index(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mut out = -1.0;
    each(it, this, a, false, |_, i, r| {
        r.truthy() && {
            out = i as f64;
            true
        }
    })?;
    Ok(Val::Num(out))
}

fn find_last(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mut out = Val::Undef;
    each(it, this, a, true, |x, _, r| {
        r.truthy() && {
            out = x.clone();
            true
        }
    })?;
    Ok(out)
}

fn find_last_index(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mut out = -1.0;
    each(it, this, a, true, |_, i, r| {
        r.truthy() && {
            out = i as f64;
            true
        }
    })?;
    Ok(Val::Num(out))
}

fn filter(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mut out = Vec::new();
    each(it, this, a, false, |x, _, r| {
        if r.truthy() {
            out.push(x.clone());
        }
        false
    })?;
    it.charge(out.len() * PROP_BYTES)?;
    it.array(out)
}

fn map(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mut out = Vec::new();
    each(it, this, a, false, |_, _, r| {
        out.push(r.clone());
        false
    })?;
    it.charge(out.len() * PROP_BYTES)?;
    it.array(out)
}

fn for_each(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    each(it, this, a, false, |_, _, _| false)?;
    Ok(Val::Undef)
}

fn some(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mut out = false;
    each(it, this, a, false, |_, _, r| {
        out = r.truthy();
        out
    })?;
    Ok(Val::Bool(out))
}

fn every(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mut out = true;
    each(it, this, a, false, |_, _, r| {
        out = r.truthy();
        !out
    })?;
    Ok(Val::Bool(out))
}

fn reduce_in(it: &mut Interp, this: &Val, a: &[Val], rev: bool) -> R {
    let f = arg(a, 0);
    if !f.is_callable() {
        return it.type_error(&format!("{} is not a function", it.show(&f)));
    }
    let v = items(it, this)?;
    let mut order: Vec<usize> =
        if rev { (0..v.len()).rev().collect() } else { (0..v.len()).collect() };
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
    v.retain(|x| !matches!(x, Val::Undef));
    let mut keys = Vec::new();
    if !cmp.is_callable() {
        for x in &v {
            keys.push(it.str_of(x)?);
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
                    keys[idx[i]].encode_utf16().cmp(keys[idx[j]].encode_utf16())
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
    Ok(out)
}

fn sort(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = array_obj(it, this)?;
    let cmp = arg(a, 0);
    if !matches!(cmp, Val::Undef) && !cmp.is_callable() {
        return it.type_error("invalid Array.prototype.sort argument");
    }
    let v = items(it, this)?;
    let s = sorted(it, v, &cmp)?;
    with_vec(&o, |v| *v = s);
    Ok(this.clone())
}

fn to_sorted(it: &mut Interp, this: &Val, a: &[Val]) -> R {
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

fn flatten(it: &mut Interp, v: Vec<Val>, depth: f64, out: &mut Vec<Val>) -> R<()> {
    for x in v {
        let nested = matches!(&x, Val::Obj(o) if matches!(o.borrow().kind, Kind::Array(_)));
        if nested && depth >= 1.0 {
            let inner = items(it, &x)?;
            flatten(it, inner, depth - 1.0, out)?;
        } else {
            it.charge(PROP_BYTES)?;
            out.push(x);
        }
    }
    Ok(())
}

fn flat(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let depth = if matches!(arg(a, 0), Val::Undef) { 1.0 } else { it.number_of(&arg(a, 0))? };
    let v = items(it, this)?;
    let mut out = Vec::new();
    flatten(it, v, depth, &mut out)?;
    it.array(out)
}

fn flat_map(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let m = map(it, this, a)?;
    let v = items(it, &m)?;
    let mut out = Vec::new();
    flatten(it, v, 1.0, &mut out)?;
    it.array(out)
}

fn fill(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let o = array_obj(it, this)?;
    let len = it.array_len(&o);
    let start = relative(it, &arg(a, 1), len, 0)?;
    let end = relative(it, &arg(a, 2), len, len)?;
    let x = arg(a, 0);
    with_vec(&o, |v| {
        for slot in v.iter_mut().take(end).skip(start) {
            *slot = x.clone();
        }
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
    let n = items(it, this)?.len();
    make_iter(it, (0..n).map(|i| Val::Num(i as f64)).collect())
}

fn values(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let v = items(it, this)?;
    make_iter(it, v)
}

fn entries(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let v = items(it, this)?;
    let mut out = Vec::new();
    for (i, x) in v.into_iter().enumerate() {
        out.push(it.array(vec![Val::Num(i as f64), x])?);
    }
    make_iter(it, out)
}
