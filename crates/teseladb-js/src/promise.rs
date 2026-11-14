//! Promises and async functions as the engine of the reference server 9.0 runs them: a promise can
//! be made, resolved and rejected, but anything that queues a job (`then`, `catch`, `finally`,
//! `await`, resolving with a thenable, `Promise.all` and its kin over a value, `Array.fromAsync`)
//! stops the call with the trap of its WebAssembly build (measured on a reference router 9.0.4 on
//! 02-11). An async function runs until its first `await`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::builtins::{arg, ctor, method, new_target};
use crate::interp::{Ctrl, Fatal, Interp, R};
use crate::value::*;

/// Where a promise is.
#[derive(Debug)]
pub struct PromiseState {
    pub value: Option<Result<Val, Val>>,
    /// Whether a resolving function already ran.
    pub resolved: bool,
}

fn trap<T>() -> R<T> {
    Err(Ctrl::Fatal(Fatal::Trap))
}

fn new_promise(it: &mut Interp) -> R<(ObjRef, Rc<RefCell<PromiseState>>)> {
    let st = Rc::new(RefCell::new(PromiseState { value: None, resolved: false }));
    let o = it.alloc(Some(it.realm.proto("Promise")), Kind::Promise(st.clone()))?;
    Ok((o, st))
}

fn state_of(v: &Val) -> Option<Rc<RefCell<PromiseState>>> {
    match v {
        Val::Obj(o) => match &o.borrow().kind {
            Kind::Promise(st) => Some(st.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// Resolves `p` with `v`: a thenable would queue a job.
fn resolve_with(it: &mut Interp, p: &Val, v: Val) -> R<()> {
    let Some(st) = state_of(p) else { return Ok(()) };
    if std::mem::replace(&mut st.borrow_mut().resolved, true) {
        return Ok(());
    }
    if let Val::Obj(o) = &v {
        if p.as_obj().is_some_and(|q| q.ptr_eq(o)) {
            let e = it.make_error("TypeError", "A promise cannot be resolved with itself.")?;
            st.borrow_mut().value = Some(Err(e));
            return Ok(());
        }
        match it.get(&v, "then") {
            Ok(t) if t.is_callable() => return trap(),
            Ok(_) => {}
            Err(Ctrl::Throw(e)) => {
                st.borrow_mut().value = Some(Err(e));
                return Ok(());
            }
            Err(e) => return Err(e),
        }
    }
    st.borrow_mut().value = Some(Ok(v));
    Ok(())
}

fn reject_with(p: &Val, v: Val) {
    if let Some(st) = state_of(p)
        && !std::mem::replace(&mut st.borrow_mut().resolved, true)
    {
        st.borrow_mut().value = Some(Err(v));
    }
}

fn resolve_fn(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    resolve_with(it, this, arg(a, 0))?;
    Ok(Val::Undef)
}

fn reject_fn(_: &mut Interp, this: &Val, a: &[Val]) -> R {
    reject_with(this, arg(a, 0));
    Ok(Val::Undef)
}

/// The resolving functions of `p`: functions bound to it.
fn resolvers(it: &mut Interp, p: &ObjRef) -> R<(Val, Val)> {
    let mut out = Vec::new();
    for (name, f) in [("resolve", resolve_fn as NativeFn), ("reject", reject_fn)] {
        let target = it.native(name, f, 1)?;
        let b = it.alloc(
            Some(it.realm.function.clone()),
            Kind::Func(Func::Bound(target, Val::Obj(p.clone()), Vec::new())),
        )?;
        out.push(Val::Obj(b));
    }
    let reject = out.pop().unwrap_or(Val::Undef);
    let resolve = out.pop().unwrap_or(Val::Undef);
    Ok((resolve, reject))
}

fn promise_ctor(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    if new_target(this).is_none() {
        return it.type_error("calling a builtin Promise constructor without new is forbidden");
    }
    let exec = arg(a, 0);
    if !exec.is_callable() {
        return it.type_error(&format!("{} is not a function", it.show(&exec)));
    }
    let (p, _) = new_promise(it)?;
    let (resolve, reject) = resolvers(it, &p)?;
    match it.call(&exec, &Val::Undef, &[resolve, reject]) {
        Ok(_) => {}
        Err(Ctrl::Throw(e)) => reject_with(&Val::Obj(p.clone()), e),
        Err(e) => return Err(e),
    }
    Ok(Val::Obj(p))
}

/// `Promise.resolve` and `reject` need an object as `this` (a constructor).
fn receiver(it: &mut Interp, this: &Val) -> R<()> {
    if matches!(this, Val::Obj(_)) {
        return Ok(());
    }
    let m = format!("Receiver of Promise.{} call is not a non-null object", it.native);
    it.type_error(&m)
}

fn promise_resolve(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    receiver(it, this)?;
    let v = arg(a, 0);
    if state_of(&v).is_some() {
        return Ok(v);
    }
    let (p, _) = new_promise(it)?;
    let p = Val::Obj(p);
    resolve_with(it, &p, v)?;
    Ok(p)
}

fn promise_reject(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    receiver(it, this)?;
    let (p, _) = new_promise(it)?;
    let p = Val::Obj(p);
    reject_with(&p, arg(a, 0));
    Ok(p)
}

/// `Promise.try(f, ...args)`: a promise of what `f` gives or throws.
fn promise_try(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let f = arg(a, 0);
    let r = it.call(&f, &Val::Undef, a.get(1..).unwrap_or(&[]));
    let (p, _) = new_promise(it)?;
    let p = Val::Obj(p);
    match r {
        Ok(v) => resolve_with(it, &p, v)?,
        Err(Ctrl::Throw(e)) => reject_with(&p, e),
        Err(e) => return Err(e),
    }
    Ok(p)
}

fn with_resolvers(it: &mut Interp, _: &Val, _: &[Val]) -> R {
    let (p, _) = new_promise(it)?;
    let (resolve, reject) = resolvers(it, &p)?;
    let o = it.object()?;
    for (k, v) in [("promise", Val::Obj(p)), ("resolve", resolve), ("reject", reject)] {
        it.define(&o, k, Prop::data(v))?;
    }
    Ok(Val::Obj(o))
}

/// `Promise.all`, `allSettled`, `any` and `race`: over no value they settle at once; a value
/// would be followed with `then`.
fn combinator(it: &mut Interp, a: &[Val], which: u8) -> R {
    let items = match it.iterate(&arg(a, 0)) {
        Err(Ctrl::Throw(e)) => {
            // What cannot be iterated rejects the promise.
            let (p, _) = new_promise(it)?;
            let p = Val::Obj(p);
            reject_with(&p, e);
            return Ok(p);
        }
        r => r?,
    };
    if !items.is_empty() {
        return trap();
    }
    let (p, st) = new_promise(it)?;
    let empty = it.array(Vec::new())?;
    st.borrow_mut().resolved = true;
    st.borrow_mut().value = match which {
        0 | 1 => Some(Ok(empty)),
        2 => {
            let e = it.make_error("AggregateError", "No Promise in Promise.any was resolved")?;
            Some(Err(e))
        }
        _ => None,
    };
    Ok(Val::Obj(p))
}

fn all(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    combinator(it, a, 0)
}

fn all_settled(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    combinator(it, a, 1)
}

fn any(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    combinator(it, a, 2)
}

fn race(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    combinator(it, a, 3)
}

fn then(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    if !matches!(this, Val::Obj(_)) {
        return it.type_error("Receiver of Promise.prototype.then call is not a non-null object");
    }
    if state_of(this).is_none() {
        let class = crate::builtins::class_name(this);
        return it.type_error(&format!("Promise.prototype.then called on incompatible {class}"));
    }
    trap()
}

fn queues_a_job(_: &mut Interp, _: &Val, _: &[Val]) -> R {
    trap()
}

/// The promise an async function's body gives, from how the body ended.
pub fn settle_async(it: &mut Interp, r: R<()>) -> R {
    let (p, _) = new_promise(it)?;
    let p = Val::Obj(p);
    match r {
        Ok(()) | Err(Ctrl::Break(_) | Ctrl::Continue(_)) => resolve_with(it, &p, Val::Undef)?,
        Err(Ctrl::Return(v)) => resolve_with(it, &p, v)?,
        Err(Ctrl::Throw(e)) => reject_with(&p, e),
        Err(e) => return Err(e),
    }
    Ok(p)
}

pub fn install(it: &mut Interp, g: &ObjRef) -> R<()> {
    let proto = it.alloc(Some(it.realm.object.clone()), Kind::Ordinary)?;
    let c = ctor(it, g, "Promise", promise_ctor, 1, &proto)?;
    for (name, f, n) in [
        ("resolve", promise_resolve as NativeFn, 1),
        ("reject", promise_reject, 1),
        ("withResolvers", with_resolvers, 0),
        ("try", promise_try, 1),
        ("all", all, 1),
        ("allSettled", all_settled, 1),
        ("any", any, 1),
        ("race", race, 1),
    ] {
        method(it, &c, name, f, n)?;
    }
    for (name, n) in [("then", 2), ("catch", 1), ("finally", 1)] {
        method(it, &proto, name, if name == "then" { then } else { queues_a_job }, n)?;
    }
    crate::builtins::to_string_tag(it, &proto, "Promise")?;
    it.realm.protos.push(("Promise", proto));
    method(it, &crate::builtins::installed(g, "Array"), "fromAsync", queues_a_job, 1)?;
    let af = it.alloc(Some(it.realm.function.clone()), Kind::Ordinary)?;
    let afc = it.native("AsyncFunction", crate::builtins::function_ctor, 1)?;
    it.define(&afc, "prototype", Prop { writable: false, ..Prop::hidden(Val::Obj(af.clone())) })?;
    it.define(&af, "constructor", Prop::hidden(Val::Obj(afc)))?;
    crate::builtins::to_string_tag(it, &af, "AsyncFunction")?;
    it.realm.protos.push(("AsyncFunction", af));
    crate::generators::install(it)
}
