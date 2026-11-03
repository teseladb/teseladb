//! `Symbol` (ECMA-262 §20.4): the constructor, the registry of `Symbol.for`, the well-known
//! symbols and the prototype, with the messages of the engine of a `refrouter` 9.0.4 (measured on
//! 02-11). A symbol is its description and an identity; the well-known ones have identity 0.

use std::rc::Rc;

use crate::builtins::{arg, class_name, ctor, method, new_target, to_string_tag, well_known};
use crate::interp::{Interp, R};
use crate::value::*;

/// The well-known symbols, `Symbol.<name>`.
const WELL_KNOWN: [&str; 13] = [
    "asyncIterator",
    "hasInstance",
    "isConcatSpreadable",
    "iterator",
    "match",
    "matchAll",
    "replace",
    "search",
    "species",
    "split",
    "toPrimitive",
    "toStringTag",
    "unscopables",
];

pub fn install(it: &mut Interp, g: &ObjRef) -> R<()> {
    let proto = it.realm.symbol.clone();
    let c = ctor(it, g, "Symbol", symbol_ctor, 0, &proto)?;
    for name in WELL_KNOWN {
        let sym = Val::Sym(Rc::new((format!("Symbol.{name}").into(), 0)));
        let fixed = Prop { writable: false, configurable: false, ..Prop::hidden(sym) };
        it.define(&c, name, fixed)?;
    }
    method(it, &c, "for", symbol_for, 1)?;
    method(it, &c, "keyFor", key_for, 1)?;
    method(it, &proto, "toString", to_string, 0)?;
    method(it, &proto, "valueOf", value_of, 0)?;
    let getter = it.native("get description", description, 0)?;
    it.define(&proto, "description", Prop { getter: Some(getter), ..Prop::hidden(Val::Undef) })?;
    let f = it.native("[Symbol.toPrimitive]", to_primitive, 1)?;
    let fixed = Prop { writable: false, ..Prop::hidden(Val::Obj(f)) };
    it.define(&proto, &well_known("toPrimitive"), fixed)?;
    to_string_tag(it, &proto, "Symbol")
}

fn symbol_ctor(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    if new_target(this).is_some() {
        return it.type_error("Symbol is not a constructor");
    }
    let described = !matches!(arg(a, 0), Val::Undef);
    let d = if described { it.str_of(&arg(a, 0))? } else { "".into() };
    Ok(new_symbol(it, d, described))
}

fn new_symbol(it: &mut Interp, d: Name, described: bool) -> Val {
    it.symbol_ids += 1;
    if !described {
        it.undescribed.push(it.symbol_ids);
    }
    Val::Sym(Rc::new((d, it.symbol_ids)))
}

/// `Symbol.for(key)`: the symbol of the registry for `key`, made the first time.
fn symbol_for(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let key = it.str_of(&arg(a, 0))?;
    if let Some((_, s)) = it.registry.iter().find(|(k, _)| *k == key) {
        return Ok(s.clone());
    }
    let s = new_symbol(it, key.clone(), true);
    it.registry.push((key, s.clone()));
    Ok(s)
}

fn key_for(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let s = arg(a, 0);
    if !matches!(s, Val::Sym(_)) {
        return it.type_error(&format!("{} is not a symbol", it.show(&s)));
    }
    Ok(it
        .registry
        .iter()
        .find(|(_, x)| strict_eq(x, &s))
        .map_or(Val::Undef, |(k, _)| Val::Str(k.clone())))
}

/// The symbol `this` is or wraps, or the error of `method` on anything else.
fn this_symbol(it: &Interp, this: &Val, method: &str) -> R<Rc<(Name, u64)>> {
    match this {
        Val::Sym(s) => return Ok(s.clone()),
        Val::Obj(o) => {
            if let Kind::Symbol(Val::Sym(s)) = &o.borrow().kind {
                return Ok(s.clone());
            }
        }
        _ => {}
    }
    it.type_error(&format!("{method} method called on incompatible {}", class_name(this)))
}

fn to_string(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let s = this_symbol(it, this, "toString")?;
    it.string(format!("Symbol({})", s.0))
}

fn value_of(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(Val::Sym(this_symbol(it, this, "valueOf")?))
}

fn to_primitive(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(Val::Sym(this_symbol(it, this, "[Symbol.toPrimitive]")?))
}

fn description(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let s = this_symbol(it, this, "get description")?;
    if s.1 != 0 && it.undescribed.contains(&s.1) {
        return Ok(Val::Undef);
    }
    Ok(Val::Str(s.0.clone()))
}

/// `get [Symbol.species]` of the constructors that make new instances of their kind: `this`.
pub fn install_species(it: &mut Interp, g: &ObjRef) -> R<()> {
    for name in ["Array", "Map", "Set", "RegExp", "Promise", "ArrayBuffer"] {
        let c = crate::builtins::installed(g, name);
        let getter = it.native("get [Symbol.species]", species, 0)?;
        let p = Prop { getter: Some(getter), ..Prop::hidden(Val::Undef) };
        it.define(&c, &well_known("species"), p)?;
    }
    Ok(())
}

fn species(_: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(this.clone())
}
