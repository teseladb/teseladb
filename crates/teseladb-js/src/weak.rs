//! `WeakMap`, `WeakSet`, `WeakRef` and `FinalizationRegistry`. An invocation frees its objects
//! when it ends, so a weak reference keeps its target as long as the call: nothing can observe
//! the difference inside one call. Building a `FinalizationRegistry` stops the engine of
//! RefDb 9.0 with a trap (measured on a `refrouter` 9.0.4 on 02-11), and so it does here.

use crate::builtins::{arg, ctor, method, new_target};
use crate::interp::{Ctrl, Fatal, Interp, R};
use crate::value::*;

fn require_new(it: &mut Interp, this: &Val, name: &str) -> R<()> {
    if new_target(this).is_none() {
        return it
            .type_error(&format!("calling a builtin {name} constructor without new is forbidden"));
    }
    Ok(())
}

fn key(it: &mut Interp, v: &Val, what: &str) -> R<ObjRef> {
    match v {
        Val::Obj(o) => Ok(o.clone()),
        _ => {
            let shown = it.show(v);
            it.type_error(&format!("{what} {shown} must be an object"))
        }
    }
}

fn this_kind(it: &mut Interp, this: &Val, name: &str, ok: fn(&Kind) -> bool) -> R<ObjRef> {
    match this {
        Val::Obj(o) if ok(&o.borrow().kind) => Ok(o.clone()),
        _ => it.type_error(&format!(
            "{name} method called on incompatible {}",
            crate::builtins::class_name(this)
        )),
    }
}

fn weak_map(it: &mut Interp, this: &Val, m: &str) -> R<ObjRef> {
    this_kind(it, this, m, |k| matches!(k, Kind::WeakMap(_)))
}

fn weak_set(it: &mut Interp, this: &Val, m: &str) -> R<ObjRef> {
    this_kind(it, this, m, |k| matches!(k, Kind::WeakSet(_)))
}

fn wm_ctor(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    require_new(it, this, "WeakMap")?;
    let m = Val::Obj(it.alloc(Some(it.realm.proto("WeakMap")), Kind::WeakMap(Vec::new()))?);
    crate::builtins::add_from(it, &m, &arg(a, 0), wm_set, Some("WeakMap"))?;
    Ok(m)
}

fn wm_get(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let m = weak_map(it, this, "get")?;
    let Val::Obj(k) = arg(a, 0) else { return Ok(Val::Undef) };
    Ok(match &m.borrow().kind {
        Kind::WeakMap(e) => {
            e.iter().find(|(x, _)| x.ptr_eq(&k)).map_or(Val::Undef, |(_, v)| v.clone())
        }
        _ => Val::Undef,
    })
}

fn wm_has(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let m = weak_map(it, this, "has")?;
    let Val::Obj(k) = arg(a, 0) else { return Ok(Val::Bool(false)) };
    Ok(Val::Bool(
        matches!(&m.borrow().kind, Kind::WeakMap(e) if e.iter().any(|(x, _)| x.ptr_eq(&k))),
    ))
}

fn wm_set(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let m = weak_map(it, this, "set")?;
    let k = key(it, &arg(a, 0), "WeakMap key")?;
    let present =
        matches!(&m.borrow().kind, Kind::WeakMap(e) if e.iter().any(|(x, _)| x.ptr_eq(&k)));
    if !present {
        it.grow(&m, 2)?;
    }
    if let Kind::WeakMap(e) = &mut m.borrow_mut().kind {
        match e.iter_mut().find(|(x, _)| x.ptr_eq(&k)) {
            Some(slot) => slot.1 = arg(a, 1),
            None => e.push((k, arg(a, 1))),
        }
    }
    Ok(this.clone())
}

fn wm_delete(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let m = weak_map(it, this, "delete")?;
    let Val::Obj(k) = arg(a, 0) else { return Ok(Val::Bool(false)) };
    let mut b = m.borrow_mut();
    let Kind::WeakMap(e) = &mut b.kind else { return Ok(Val::Bool(false)) };
    let before = e.len();
    e.retain(|(x, _)| !x.ptr_eq(&k));
    Ok(Val::Bool(e.len() != before))
}

fn ws_ctor(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    require_new(it, this, "WeakSet")?;
    let s = Val::Obj(it.alloc(Some(it.realm.proto("WeakSet")), Kind::WeakSet(Vec::new()))?);
    crate::builtins::add_from(it, &s, &arg(a, 0), ws_add, None)?;
    Ok(s)
}

fn ws_add(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let s = weak_set(it, this, "add")?;
    let k = key(it, &arg(a, 0), "WeakSet value")?;
    let present = matches!(&s.borrow().kind, Kind::WeakSet(e) if e.iter().any(|x| x.ptr_eq(&k)));
    if !present {
        it.grow(&s, 1)?;
        if let Kind::WeakSet(e) = &mut s.borrow_mut().kind {
            e.push(k);
        }
    }
    Ok(this.clone())
}

fn ws_has(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let s = weak_set(it, this, "has")?;
    let Val::Obj(k) = arg(a, 0) else { return Ok(Val::Bool(false)) };
    Ok(Val::Bool(matches!(&s.borrow().kind, Kind::WeakSet(e) if e.iter().any(|x| x.ptr_eq(&k)))))
}

fn ws_delete(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let s = weak_set(it, this, "delete")?;
    let Val::Obj(k) = arg(a, 0) else { return Ok(Val::Bool(false)) };
    let mut b = s.borrow_mut();
    let Kind::WeakSet(e) = &mut b.kind else { return Ok(Val::Bool(false)) };
    let before = e.len();
    e.retain(|x| !x.ptr_eq(&k));
    Ok(Val::Bool(e.len() != before))
}

fn wr_ctor(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    require_new(it, this, "WeakRef")?;
    let Val::Obj(t) = arg(a, 0) else {
        let shown = it.show(&arg(a, 0));
        return it.type_error(&format!("{shown} is not a non-null object"));
    };
    Ok(Val::Obj(it.alloc(Some(it.realm.proto("WeakRef")), Kind::WeakRef(t))?))
}

fn wr_deref(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let r = this_kind(it, this, "deref", |k| matches!(k, Kind::WeakRef(_)))?;
    Ok(match &r.borrow().kind {
        Kind::WeakRef(t) => Val::Obj(t.clone()),
        _ => Val::Undef,
    })
}

fn fr_ctor(_: &mut Interp, _: &Val, _: &[Val]) -> R {
    Err(Ctrl::Fatal(Fatal::Trap))
}

pub fn install(it: &mut Interp, g: &ObjRef) -> R<()> {
    type Methods<'a> = &'a [(&'static str, NativeFn, usize)];
    let kinds: [(&'static str, NativeFn, Methods); 4] = [
        (
            "WeakMap",
            wm_ctor,
            &[("get", wm_get, 1), ("set", wm_set, 2), ("has", wm_has, 1), ("delete", wm_delete, 1)],
        ),
        ("WeakSet", ws_ctor, &[("add", ws_add, 1), ("has", ws_has, 1), ("delete", ws_delete, 1)]),
        ("WeakRef", wr_ctor, &[("deref", wr_deref, 0)]),
        ("FinalizationRegistry", fr_ctor, &[]),
    ];
    for (name, c, methods) in kinds {
        let proto = it.alloc(Some(it.realm.object.clone()), Kind::Ordinary)?;
        ctor(it, g, name, c, if name == "WeakSet" || name == "WeakMap" { 0 } else { 1 }, &proto)?;
        for (m, f, n) in methods {
            method(it, &proto, m, *f, *n)?;
        }
        crate::builtins::to_string_tag(it, &proto, name)?;
        it.realm.protos.push((name, proto));
    }
    Ok(())
}
