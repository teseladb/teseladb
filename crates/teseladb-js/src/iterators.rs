//! The `Iterator` global and the iterator helpers of ECMA-262 §27.1 (`map`, `filter`, `take`,
//! `drop`, `flatMap`, `reduce`, `toArray`, `forEach`, `some`, `every`, `find`), which the
//! engine of RefDb 9.0 has.

use std::cell::RefCell;
use std::rc::Rc;

use crate::builtins::{arg, method, new_target};
use crate::interp::{Ctrl, Interp, IterRec, R};
use crate::value::*;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Op {
    Map,
    Filter,
    Take,
    Drop,
    FlatMap,
}

/// The state of a lazy helper.
pub struct Helper {
    op: Op,
    src: Option<IterRec>,
    f: Val,
    /// What `take` may still give, or what `drop` still skips.
    count: f64,
    counter: usize,
    inner: Option<IterRec>,
}

impl std::fmt::Debug for Helper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Helper").field("op", &self.op).finish_non_exhaustive()
    }
}

/// The iterator of `this` and its `next` (`GetIteratorDirect`).
fn direct(it: &mut Interp, this: &Val) -> R<IterRec> {
    if !matches!(this, Val::Obj(_)) {
        return it.type_error(&format!("{} is not an object", it.show(this)));
    }
    let next = it.get(this, "next")?;
    Ok(IterRec::Proto { it: this.clone(), next })
}

fn callable(it: &mut Interp, f: &Val) -> R<Val> {
    if !f.is_callable() {
        return it.type_error(&format!("{} is not a function", it.show(f)));
    }
    Ok(f.clone())
}

fn count_of(it: &mut Interp, v: &Val) -> R<f64> {
    let n = it.number_of(v)?;
    if n.is_nan() || n.trunc() < 0.0 {
        return Err(it.error("RangeError", "Iterator limits cannot be negative"));
    }
    Ok(n.trunc())
}

fn helper(it: &mut Interp, this: &Val, op: Op, f: Val, count: f64) -> R {
    let src = direct(it, this)?;
    let h = Helper { op, src: Some(src), f, count, counter: 0, inner: None };
    let o =
        it.alloc(Some(it.realm.proto("IteratorHelper")), Kind::Helper(Rc::new(RefCell::new(h))))?;
    Ok(Val::Obj(o))
}

fn map(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let f = callable(it, &arg(a, 0))?;
    helper(it, this, Op::Map, f, 0.0)
}

fn filter(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let f = callable(it, &arg(a, 0))?;
    helper(it, this, Op::Filter, f, 0.0)
}

fn take(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let n = count_of(it, &arg(a, 0))?;
    helper(it, this, Op::Take, Val::Undef, n)
}

fn drop(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let n = count_of(it, &arg(a, 0))?;
    helper(it, this, Op::Drop, Val::Undef, n)
}

fn flat_map(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let f = callable(it, &arg(a, 0))?;
    helper(it, this, Op::FlatMap, f, 0.0)
}

fn result(it: &mut Interp, value: Val, done: bool) -> R {
    let o = it.object()?;
    it.define(&o, "value", Prop::data(value))?;
    it.define(&o, "done", Prop::data(Val::Bool(done)))?;
    Ok(Val::Obj(o))
}

fn state(it: &mut Interp, this: &Val) -> R<Rc<RefCell<Helper>>> {
    if let Val::Obj(o) = this
        && let Kind::Helper(h) = &o.borrow().kind
    {
        return Ok(h.clone());
    }
    it.type_error("Iterator Helper method called on incompatible value")
}

/// The next value of the helper, `None` at its end.
fn step(it: &mut Interp, h: &Rc<RefCell<Helper>>) -> R<Option<Val>> {
    loop {
        let op = h.borrow().op;
        if op == Op::FlatMap {
            let inner = h.borrow_mut().inner.take();
            if let Some(mut inner) = inner
                && let Some(v) = it.iter_step(&mut inner)?
            {
                h.borrow_mut().inner = Some(inner);
                return Ok(Some(v));
            }
        }
        if op == Op::Take {
            let left = h.borrow().count;
            if left <= 0.0 {
                let src = h.borrow_mut().src.take();
                if let Some(src) = src {
                    it.iter_close(&src)?;
                }
                return Ok(None);
            }
            h.borrow_mut().count = left - 1.0;
        }
        let Some(mut src) = h.borrow_mut().src.take() else { return Ok(None) };
        let next = it.iter_step(&mut src);
        let Some(v) = next? else { return Ok(None) };
        h.borrow_mut().src = Some(src);
        let (f, i) = {
            let mut b = h.borrow_mut();
            b.counter += 1;
            (b.f.clone(), b.counter - 1)
        };
        let idx = Val::Num(i as f64);
        let r = match op {
            Op::Map => it.call(&f, &Val::Undef, &[v, idx]).map(Some),
            Op::Filter => {
                it.call(&f, &Val::Undef, &[v.clone(), idx]).map(|k| k.truthy().then_some(v))
            }
            Op::Take => Ok(Some(v)),
            Op::Drop => {
                let left = h.borrow().count;
                if left > 0.0 {
                    h.borrow_mut().count = left - 1.0;
                    Ok(None)
                } else {
                    Ok(Some(v))
                }
            }
            Op::FlatMap => it.call(&f, &Val::Undef, &[v, idx]).and_then(|m| {
                if !matches!(m, Val::Obj(_) | Val::Str(_)) {
                    return it.type_error(&format!("{} is not iterable", it.show(&m)));
                }
                let rec = it.open_iter(&m)?;
                h.borrow_mut().inner = Some(rec);
                Ok(None)
            }),
        };
        match r {
            Ok(Some(v)) => return Ok(Some(v)),
            Ok(None) => {}
            Err(e) => {
                if let Some(src) = h.borrow_mut().src.take() {
                    let _ = it.iter_close(&src);
                }
                return Err(e);
            }
        }
    }
}

fn helper_next(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let h = state(it, this)?;
    match step(it, &h)? {
        Some(v) => result(it, v, false),
        None => {
            h.borrow_mut().src = None;
            result(it, Val::Undef, true)
        }
    }
}

fn helper_return(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let h = state(it, this)?;
    let src = h.borrow_mut().src.take();
    if let Some(src) = src {
        it.iter_close(&src)?;
    }
    result(it, Val::Undef, true)
}

/// Runs `f` on each value until it asks to stop; closes the iterator when it stops early.
fn each(
    it: &mut Interp,
    this: &Val,
    mut f: impl FnMut(&mut Interp, Val, usize) -> R<bool>,
) -> R<()> {
    let mut rec = direct(it, this)?;
    let mut i = 0;
    while let Some(v) = it.iter_step(&mut rec)? {
        match f(it, v, i) {
            Ok(true) => return it.iter_close(&rec),
            Ok(false) => {}
            Err(e) => {
                if matches!(e, Ctrl::Throw(_)) {
                    let _ = it.iter_close(&rec);
                }
                return Err(e);
            }
        }
        i += 1;
    }
    Ok(())
}

fn to_array(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let mut out = Vec::new();
    each(it, this, |_, v, _| {
        out.push(v);
        Ok(false)
    })?;
    it.array(out)
}

fn for_each(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let f = callable(it, &arg(a, 0))?;
    each(it, this, |it, v, i| it.call(&f, &Val::Undef, &[v, Val::Num(i as f64)]).map(|_| false))?;
    Ok(Val::Undef)
}

fn some(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let f = callable(it, &arg(a, 0))?;
    let mut found = false;
    each(it, this, |it, v, i| {
        found = it.call(&f, &Val::Undef, &[v, Val::Num(i as f64)])?.truthy();
        Ok(found)
    })?;
    Ok(Val::Bool(found))
}

fn every(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let f = callable(it, &arg(a, 0))?;
    let mut all = true;
    each(it, this, |it, v, i| {
        all = it.call(&f, &Val::Undef, &[v, Val::Num(i as f64)])?.truthy();
        Ok(!all)
    })?;
    Ok(Val::Bool(all))
}

fn find(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let f = callable(it, &arg(a, 0))?;
    let mut out = Val::Undef;
    each(it, this, |it, v, i| {
        let hit = it.call(&f, &Val::Undef, &[v.clone(), Val::Num(i as f64)])?.truthy();
        if hit {
            out = v;
        }
        Ok(hit)
    })?;
    Ok(out)
}

fn reduce(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let f = callable(it, &arg(a, 0))?;
    let mut acc = if a.len() >= 2 { Some(a[1].clone()) } else { None };
    each(it, this, |it, v, i| {
        acc = Some(match acc.take() {
            None => v,
            Some(x) => it.call(&f, &Val::Undef, &[x, v, Val::Num(i as f64)])?,
        });
        Ok(false)
    })?;
    match acc {
        Some(v) => Ok(v),
        None => it.type_error("reduce of empty iterator with no initial value"),
    }
}

fn iterator_ctor(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    match new_target(this) {
        None => it.type_error("calling a builtin Iterator constructor without new is forbidden"),
        Some(_) => it.type_error("Iterator constructor can't be used directly"),
    }
}

/// `Iterator.from(o)`: its iterator, or `o` itself when it has a `next`.
fn from(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let o = arg(a, 0);
    let f = if o.is_nullish() { Val::Undef } else { it.get(&o, "@@Symbol.iterator:0")? };
    if f.is_callable() {
        return it.call(&f, &o, &[]);
    }
    if matches!(o, Val::Obj(_)) {
        // An iterator that does not inherit the helpers is wrapped in one that does.
        let w = it.alloc(Some(it.realm.iter.clone()), Kind::Ordinary)?;
        for m in ["next", "return"] {
            let f = it.get(&o, m)?;
            if let Val::Obj(f) = f {
                let b = it.alloc(
                    Some(it.realm.function.clone()),
                    Kind::Func(Func::Bound(f, o.clone(), Vec::new())),
                )?;
                it.define(&w, m, Prop::hidden(Val::Obj(b)))?;
            }
        }
        return Ok(Val::Obj(w));
    }
    it.type_error(&format!("{} is not iterable", it.show(&o)))
}

pub fn install(it: &mut Interp, g: &ObjRef) -> R<()> {
    let proto = it.realm.iter.clone();
    let c = crate::builtins::ctor(it, g, "Iterator", iterator_ctor, 0, &proto)?;
    method(it, &c, "from", from, 1)?;
    for (name, f, n) in [
        ("map", map as NativeFn, 1),
        ("filter", filter, 1),
        ("take", take, 1),
        ("drop", drop, 1),
        ("flatMap", flat_map, 1),
        ("reduce", reduce, 1),
        ("toArray", to_array, 0),
        ("forEach", for_each, 1),
        ("some", some, 1),
        ("every", every, 1),
        ("find", find, 1),
    ] {
        method(it, &proto, name, f, n)?;
    }
    let hp = it.alloc(Some(proto), Kind::Ordinary)?;
    method(it, &hp, "next", helper_next, 0)?;
    method(it, &hp, "return", helper_return, 0)?;
    crate::builtins::to_string_tag(it, &hp, "Iterator Helper")?;
    it.realm.protos.push(("IteratorHelper", hp));
    Ok(())
}
