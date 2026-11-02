//! The text SpiderMonkey writes for an operand in a `TypeError` (`can't convert o to number`,
//! `({}) is not iterable`): the expression when it can decompile it (a name, a member, a call),
//! else the source of the value, as `uneval` gives it (`({a:1})`, `[1, 2]`).

use crate::ast::Expr;
use crate::interp::Interp;
use crate::value::*;

/// How deep `value_source` follows nested objects.
const MAX_DEPTH: usize = 4;

/// The text of operand `e`, whose value is `v`.
pub fn operand_text(it: &Interp, e: &Expr, v: &Val) -> String {
    match e {
        Expr::At(_, inner) => operand_text(it, inner, v),
        Expr::Ident(..) | Expr::Member { .. } | Expr::This | Expr::Call { .. } => {
            crate::interp::callee_text(e)
        }
        _ => value_source(it, v),
    }
}

/// The source of `v` (`uneval`).
pub fn value_source(it: &Interp, v: &Val) -> String {
    source(it, v, 0)
}

fn source(it: &Interp, v: &Val, depth: usize) -> String {
    let Val::Obj(o) = v else { return primitive(v) };
    let b = o.borrow();
    match &b.kind {
        Kind::Func(Func::Closure { def, .. }) => crate::text::to_utf8(&def.source).into_owned(),
        Kind::Func(Func::Native(_, n)) => format!("function {n}() {{\n    [native code]\n}}"),
        Kind::Func(_) => "function () {\n    [native code]\n}".into(),
        _ if depth >= MAX_DEPTH => "{}".into(),
        Kind::Array(items) => {
            let parts: Vec<String> = items.iter().map(|x| source(it, x, depth + 1)).collect();
            format!("[{}]", parts.join(", "))
        }
        _ => {
            let mut parts = Vec::new();
            for (k, p) in &b.props {
                if !p.enumerable || k.starts_with("@@") && !k.starts_with("@@Symbol.") {
                    continue;
                }
                parts.push(property(it, k, p, depth));
            }
            format!("({{{}}})", parts.join(", "))
        }
    }
}

fn key_text(k: &str) -> String {
    if let Some(rest) = k.strip_prefix("@@") {
        let name = rest.rsplit_once(':').map_or(rest, |(n, _)| n);
        return format!("[{name}]");
    }
    let plain = k.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$')
        && k.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$');
    if plain || array_index(k).is_some() { k.to_string() } else { crate::json::quote(k) }
}

fn property(it: &Interp, k: &str, p: &Prop, depth: usize) -> String {
    let accessor = |f: &Option<ObjRef>| match f {
        Some(f) => match &f.borrow().kind {
            Kind::Func(Func::Closure { def, .. }) => Some(def.source.to_string()),
            _ => None,
        },
        None => None,
    };
    if p.getter.is_some() || p.setter.is_some() {
        let parts: Vec<String> =
            [accessor(&p.getter), accessor(&p.setter)].into_iter().flatten().collect();
        return parts.join(", ");
    }
    if let Val::Obj(f) = &p.value
        && let Kind::Func(Func::Closure { def, .. }) = &f.borrow().kind
        && def.method
    {
        return def.source.to_string();
    }
    format!("{}:{}", key_text(k), source(it, &p.value, depth + 1))
}

fn primitive(v: &Val) -> String {
    match v {
        Val::Undef | Val::Hole => "(void 0)".into(),
        Val::Null => "null".into(),
        Val::Bool(b) => b.to_string(),
        Val::Num(n) if *n == 0.0 && n.is_sign_negative() => "-0".into(),
        Val::Num(n) => crate::number::to_string(*n),
        Val::BigInt(b) => format!("{b}n"),
        Val::Str(s) => crate::json::quote(s),
        Val::Sym(s) => format!("Symbol({})", crate::json::quote(&s.0)),
        Val::Obj(_) => String::new(),
    }
}
