//! `pop`, `shift`, `unshift`, `reverse`, `splice` and `sort` of `Array.prototype` on a `this`
//! that is not an array (an object with a `length`, a proxy, a string), as ECMA-262 §23.1.3
//! writes them: through `length`, `get`, `set`, `has` and `delete`, which throw when they fail.
//! Measured on a `refrouter` 9.0.4 on 03-11: `Array.prototype.pop.call({length: 2, 0: 'a', 1: 'b'})`
//! is `'b'` and leaves `{0: 'a', length: 1}`.

use crate::arrays::{key, move_element, read_at, relative, sorted, species, species_result};
use crate::interp::{Interp, R};
use crate::value::*;

/// The longest length (ECMA-262 §7.1.20 ToLength).
const MAX_LENGTH: f64 = 9_007_199_254_740_991.0;

fn arg(args: &[Val], i: usize) -> Val {
    args.get(i).cloned().unwrap_or(Val::Undef)
}

/// `this` as an object and its `length`, when it is not an array.
pub fn of(it: &mut Interp, this: &Val) -> R<Option<(ObjRef, usize)>> {
    if matches!(this, Val::Obj(o) if matches!(o.borrow().kind, Kind::Array(_))) {
        return Ok(None);
    }
    let o = it.object_of(this)?;
    let n = it.get(&Val::Obj(o.clone()), "length")?;
    let n = it.number_of(&n)?;
    // NaN is not above 0.
    Ok(Some((o, if n > 0.0 { n.min(MAX_LENGTH) as usize } else { 0 })))
}

/// Runs `f` with `set` and `delete` throwing when they fail, as the methods call them.
fn throwing<T>(it: &mut Interp, f: impl FnOnce(&mut Interp) -> R<T>) -> R<T> {
    let outer = std::mem::replace(&mut it.strict, true);
    let r = f(it);
    it.strict = outer;
    r
}

fn set_length(it: &mut Interp, o: &ObjRef, n: usize) -> R<()> {
    it.set(&Val::Obj(o.clone()), "length", Val::Num(n as f64))
}

/// `pop` (`first` false) or `shift` (`first` true).
pub fn remove(it: &mut Interp, o: &ObjRef, len: usize, first: bool) -> R {
    throwing(it, |it| {
        if len == 0 {
            set_length(it, o, 0)?;
            return Ok(Val::Undef);
        }
        let out = it.get(&Val::Obj(o.clone()), &key(if first { 0 } else { len - 1 }))?;
        if first {
            for k in 1..len {
                move_element(it, o, k, k - 1)?;
            }
        }
        it.delete_key(o, &key(len - 1))?;
        set_length(it, o, len - 1)?;
        Ok(out)
    })
}

/// Fails when a length would pass the longest one.
fn check_grow(it: &mut Interp, len: usize, more: usize) -> R<()> {
    if (len + more) as f64 > MAX_LENGTH {
        return it.type_error("Too long array");
    }
    Ok(())
}

pub fn unshift(it: &mut Interp, o: &ObjRef, len: usize, items: &[Val]) -> R {
    throwing(it, |it| {
        if !items.is_empty() {
            check_grow(it, len, items.len())?;
            for k in (0..len).rev() {
                move_element(it, o, k, k + items.len())?;
            }
            for (j, x) in items.iter().enumerate() {
                it.set(&Val::Obj(o.clone()), &key(j), x.clone())?;
            }
        }
        set_length(it, o, len + items.len())?;
        Ok(Val::Num((len + items.len()) as f64))
    })
}

/// `push`: the items after the `length` elements.
pub fn push(it: &mut Interp, o: &ObjRef, len: usize, items: &[Val]) -> R {
    throwing(it, |it| {
        check_grow(it, len, items.len())?;
        for (j, x) in items.iter().enumerate() {
            it.set(&Val::Obj(o.clone()), &key(len + j), x.clone())?;
        }
        set_length(it, o, len + items.len())?;
        Ok(Val::Num((len + items.len()) as f64))
    })
}

pub fn reverse(it: &mut Interp, o: &ObjRef, len: usize) -> R {
    throwing(it, |it| {
        for lower in 0..len / 2 {
            let upper = len - 1 - lower;
            let (a, b) = (read_at(it, o, lower)?, read_at(it, o, upper)?);
            crate::arrays::write_back_at(it, o, lower, b)?;
            crate::arrays::write_back_at(it, o, upper, a)?;
        }
        Ok(Val::Obj(o.clone()))
    })
}

pub fn splice(it: &mut Interp, this: &Val, o: &ObjRef, len: usize, a: &[Val]) -> R {
    let start = relative(it, &arg(a, 0), len, 0)?;
    let count = match a.len() {
        0 => 0,
        1 => len - start,
        _ => {
            let c = it.number_of(&a[1])?;
            if c > 0.0 { (c as usize).min(len - start) } else { 0 }
        }
    };
    let items = a.get(2..).unwrap_or(&[]);
    check_grow(it, len - count, items.len())?;
    let target = species(it, this, count)?;
    let mut removed = Vec::with_capacity(count);
    for k in 0..count {
        removed.push(read_at(it, o, start + k)?);
    }
    let out = species_result(it, target, removed, true)?;
    throwing(it, |it| {
        if items.len() < count {
            for k in start..len - count {
                move_element(it, o, k + count, k + items.len())?;
            }
            for k in (len - count + items.len()..len).rev() {
                it.delete_key(o, &key(k))?;
            }
        } else {
            for k in (start..len - count).rev() {
                move_element(it, o, k + count, k + items.len())?;
            }
        }
        for (j, x) in items.iter().enumerate() {
            it.set(&Val::Obj(o.clone()), &key(start + j), x.clone())?;
        }
        set_length(it, o, len - count + items.len())
    })?;
    Ok(out)
}

/// `sort` with `cmp` checked: the elements it has, sorted, then the holes deleted.
pub fn sort(it: &mut Interp, o: &ObjRef, len: usize, cmp: &Val) -> R {
    let mut v = Vec::with_capacity(len);
    for k in 0..len {
        v.push(read_at(it, o, k)?);
    }
    let s = sorted(it, v, cmp)?;
    throwing(it, |it| crate::arrays::write_back(it, o, s))?;
    Ok(Val::Obj(o.clone()))
}
