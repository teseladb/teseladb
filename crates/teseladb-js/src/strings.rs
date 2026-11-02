//! `String` and `String.prototype` (ECMA-262 §22.1). Positions count UTF-16 units, as in
//! ECMAScript.

use crate::builtins::method;
use crate::interp::{Interp, R};
use crate::value::*;

fn arg(args: &[Val], i: usize) -> Val {
    args.get(i).cloned().unwrap_or(Val::Undef)
}

pub fn install(it: &mut Interp, g: &ObjRef) -> R<()> {
    let proto = it.realm.string.clone();
    let c = it.native("String", string_ctor, 1)?;
    it.define(&c, "prototype", Prop { writable: false, ..Prop::hidden(Val::Obj(proto.clone())) })?;
    it.define(&proto, "constructor", Prop::hidden(Val::Obj(c.clone())))?;
    it.define(g, "String", Prop::hidden(Val::Obj(c.clone())))?;
    method(it, &c, "fromCharCode", from_char_code, 1)?;
    method(it, &c, "fromCodePoint", from_code_point, 1)?;
    for (name, f, n) in [
        ("charAt", char_at as NativeFn, 1),
        ("charCodeAt", char_code_at, 1),
        ("codePointAt", code_point_at, 1),
        ("indexOf", index_of, 1),
        ("lastIndexOf", last_index_of, 1),
        ("includes", includes, 1),
        ("startsWith", starts_with, 1),
        ("endsWith", ends_with, 1),
        ("slice", slice, 2),
        ("substring", substring, 2),
        ("substr", substr, 2),
        ("toUpperCase", upper, 0),
        ("toLowerCase", lower, 0),
        ("toLocaleUpperCase", upper, 0),
        ("toLocaleLowerCase", lower, 0),
        ("trim", trim, 0),
        ("trimStart", trim_start, 0),
        ("trimEnd", trim_end, 0),
        ("padStart", pad_start, 2),
        ("padEnd", pad_end, 2),
        ("repeat", repeat, 1),
        ("concat", concat, 1),
        ("at", at, 1),
        ("split", split, 2),
        ("replace", replace, 2),
        ("replaceAll", replace_all, 2),
        ("match", crate::regex::string_match, 1),
        ("search", crate::regex::search, 1),
        ("toString", value_of, 0),
        ("valueOf", value_of, 0),
        ("localeCompare", locale_compare, 1),
        ("normalize", value_of, 0),
    ] {
        method(it, &proto, name, f, n)?;
    }
    Ok(())
}

fn string_ctor(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let s = match arg(a, 0) {
        Val::Undef if a.is_empty() => "".into(),
        Val::Sym(s) => format!("Symbol({})", s.0).into(),
        v => it.str_of(&v)?,
    };
    let called_with_new = matches!(this, Val::Obj(o) if o.borrow().get_own("@@new").is_some());
    if called_with_new {
        return Ok(Val::Obj(it.alloc(Some(it.realm.string.clone()), Kind::Str(s))?));
    }
    Ok(Val::Str(s))
}

/// `this` as a string.
pub fn this_str(it: &mut Interp, this: &Val) -> R<Name> {
    if this.is_nullish() {
        return it.type_error(&format!("String.prototype method called on {}", it.show(this)));
    }
    it.str_of(this)
}

pub fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

pub fn from_units(u: &[u16]) -> String {
    String::from_utf16_lossy(u)
}

fn int_arg(it: &mut Interp, v: &Val, default: f64) -> R<f64> {
    if matches!(v, Val::Undef) {
        return Ok(default);
    }
    let n = it.number_of(v)?;
    Ok(if n.is_nan() { 0.0 } else { n.trunc() })
}

fn clamp(n: f64, len: usize) -> usize {
    if n < 0.0 { 0 } else { (n as usize).min(len) }
}

fn rel(n: f64, len: usize) -> usize {
    if n < 0.0 { (len as f64 + n).max(0.0) as usize } else { (n as usize).min(len) }
}

fn from_char_code(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let mut u = Vec::new();
    for x in a {
        u.push(crate::number::to_uint32(it.number_of(x)?) as u16);
    }
    it.string(from_units(&u))
}

fn from_code_point(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let mut s = String::new();
    for x in a {
        let n = it.number_of(x)?;
        match char::from_u32(n as u32).filter(|_| n.fract() == 0.0 && n >= 0.0) {
            Some(c) => s.push(c),
            None => {
                return Err(it.error(
                    "RangeError",
                    &format!("{} is not a valid code point", crate::number::to_string(n)),
                ));
            }
        }
    }
    it.string(s)
}

fn char_at(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let u = units(&this_str(it, this)?);
    let i = int_arg(it, &arg(a, 0), 0.0)?;
    if i < 0.0 || i >= u.len() as f64 {
        return Ok(Val::str(""));
    }
    it.string(from_units(&u[i as usize..=i as usize]))
}

fn char_code_at(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let u = units(&this_str(it, this)?);
    let i = int_arg(it, &arg(a, 0), 0.0)?;
    if i < 0.0 || i >= u.len() as f64 {
        return Ok(Val::Num(f64::NAN));
    }
    Ok(Val::Num(f64::from(u[i as usize])))
}

fn code_point_at(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let u = units(&this_str(it, this)?);
    let i = int_arg(it, &arg(a, 0), 0.0)?;
    if i < 0.0 || i >= u.len() as f64 {
        return Ok(Val::Undef);
    }
    let i = i as usize;
    let c = char::decode_utf16(u[i..].iter().copied()).next();
    Ok(Val::Num(match c {
        Some(Ok(c)) => f64::from(c as u32),
        _ => f64::from(u[i]),
    }))
}

fn find_units(hay: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(hay.len()));
    }
    (from..=hay.len().saturating_sub(needle.len()))
        .find(|&i| hay.get(i..i + needle.len()) == Some(needle))
}

fn index_of(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let u = units(&this_str(it, this)?);
    let n = units(&it.str_of(&arg(a, 0))?);
    let from = clamp(int_arg(it, &arg(a, 1), 0.0)?, u.len());
    Ok(Val::Num(find_units(&u, &n, from).map_or(-1.0, |i| i as f64)))
}

fn last_index_of(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let u = units(&this_str(it, this)?);
    let n = units(&it.str_of(&arg(a, 0))?);
    if n.len() > u.len() {
        return Ok(Val::Num(-1.0));
    }
    let r = (0..=u.len() - n.len()).rev().find(|&i| u[i..i + n.len()] == n[..]);
    Ok(Val::Num(r.map_or(-1.0, |i| i as f64)))
}

fn no_regex(it: &mut Interp, v: &Val, what: &str) -> R<()> {
    if matches!(v, Val::Obj(o) if matches!(o.borrow().kind, Kind::RegExp(..))) {
        return it.type_error(&format!(
            "first argument to String.prototype.{what} must not be a regular expression"
        ));
    }
    Ok(())
}

fn includes(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    no_regex(it, &arg(a, 0), "includes")?;
    index_of(it, this, a).map(|v| Val::Bool(!matches!(v, Val::Num(n) if n < 0.0)))
}

fn starts_with(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    no_regex(it, &arg(a, 0), "startsWith")?;
    let u = units(&this_str(it, this)?);
    let n = units(&it.str_of(&arg(a, 0))?);
    let from = clamp(int_arg(it, &arg(a, 1), 0.0)?, u.len());
    Ok(Val::Bool(u[from..].starts_with(&n)))
}

fn ends_with(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    no_regex(it, &arg(a, 0), "endsWith")?;
    let u = units(&this_str(it, this)?);
    let n = units(&it.str_of(&arg(a, 0))?);
    let end = clamp(int_arg(it, &arg(a, 1), u.len() as f64)?, u.len());
    Ok(Val::Bool(u[..end].ends_with(&n)))
}

fn slice(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let u = units(&this_str(it, this)?);
    let s = rel(int_arg(it, &arg(a, 0), 0.0)?, u.len());
    let e = rel(int_arg(it, &arg(a, 1), u.len() as f64)?, u.len());
    it.string(if s < e { from_units(&u[s..e]) } else { String::new() })
}

fn substring(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let u = units(&this_str(it, this)?);
    let s = clamp(int_arg(it, &arg(a, 0), 0.0)?, u.len());
    let e = clamp(int_arg(it, &arg(a, 1), u.len() as f64)?, u.len());
    let (s, e) = if s > e { (e, s) } else { (s, e) };
    it.string(from_units(&u[s..e]))
}

fn substr(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let u = units(&this_str(it, this)?);
    let s = rel(int_arg(it, &arg(a, 0), 0.0)?, u.len());
    let n = int_arg(it, &arg(a, 1), (u.len() - s) as f64)?;
    let e = (s + clamp(n, u.len())).min(u.len());
    it.string(from_units(&u[s..e]))
}

fn upper(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let s = this_str(it, this)?;
    it.string(s.to_uppercase())
}

fn lower(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let s = this_str(it, this)?;
    it.string(s.to_lowercase())
}

fn is_ws(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

fn trim(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let s = this_str(it, this)?;
    it.string(s.trim_matches(is_ws).to_owned())
}

fn trim_start(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let s = this_str(it, this)?;
    it.string(s.trim_start_matches(is_ws).to_owned())
}

fn trim_end(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let s = this_str(it, this)?;
    it.string(s.trim_end_matches(is_ws).to_owned())
}

fn pad(it: &mut Interp, this: &Val, a: &[Val], start: bool) -> R {
    let s = this_str(it, this)?;
    let len = s.encode_utf16().count();
    let target = int_arg(it, &arg(a, 0), 0.0)?;
    let fill = if matches!(arg(a, 1), Val::Undef) { " ".into() } else { it.str_of(&arg(a, 1))? };
    if target <= len as f64 || fill.is_empty() {
        return Ok(Val::Str(s));
    }
    let need = target as usize - len;
    it.charge(need * 2)?;
    let f = units(&fill);
    let padding: Vec<u16> = f.iter().copied().cycle().take(need).collect();
    let padding = from_units(&padding);
    it.string(if start { format!("{padding}{s}") } else { format!("{s}{padding}") })
}

fn pad_start(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    pad(it, this, a, true)
}

fn pad_end(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    pad(it, this, a, false)
}

fn repeat(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let s = this_str(it, this)?;
    let n = int_arg(it, &arg(a, 0), 0.0)?;
    if n < 0.0 || n.is_infinite() {
        return Err(it.error("RangeError", "repeat count must be non-negative"));
    }
    let total = (s.len() as f64) * n;
    if total > (1u64 << 30) as f64 {
        return Err(it.error(
            "RangeError",
            "repeat count must be less than infinity and not overflow maximum string size",
        ));
    }
    it.charge(total as usize)?;
    Ok(Val::Str(s.repeat(n as usize).into()))
}

fn concat(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let mut s = this_str(it, this)?.to_string();
    for x in a {
        s.push_str(&it.str_of(x)?);
    }
    it.string(s)
}

fn at(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let u = units(&this_str(it, this)?);
    let n = int_arg(it, &arg(a, 0), 0.0)?;
    let i = if n < 0.0 { u.len() as f64 + n } else { n };
    if i < 0.0 || i >= u.len() as f64 {
        return Ok(Val::Undef);
    }
    it.string(from_units(&u[i as usize..=i as usize]))
}

fn split(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let s = this_str(it, this)?;
    let limit = if matches!(arg(a, 1), Val::Undef) {
        u32::MAX as usize
    } else {
        crate::number::to_uint32(it.number_of(&arg(a, 1))?) as usize
    };
    let sep = arg(a, 0);
    if matches!(&sep, Val::Obj(o) if matches!(o.borrow().kind, Kind::RegExp(..))) {
        return crate::regex::split(it, &s, &sep, limit);
    }
    let parts: Vec<Val> = match sep {
        Val::Undef => vec![Val::Str(s.clone())],
        sep => {
            let sep = it.str_of(&sep)?;
            if sep.is_empty() {
                s.encode_utf16().map(|c| Val::Str(from_units(&[c]).into())).collect()
            } else {
                s.split(&*sep).map(|p| Val::Str(p.into())).collect()
            }
        }
    };
    let parts: Vec<Val> = parts.into_iter().take(limit).collect();
    it.charge(parts.len() * crate::interp::PROP_BYTES + s.len())?;
    it.array(parts)
}

/// The replacement text for a match: `$&`, `` $` ``, `$'`, `$1`…, `$$`, or the function's result.
pub fn substitute(
    it: &mut Interp,
    replacement: &Val,
    matched: &str,
    position: usize,
    subject: &str,
    groups: &[Val],
) -> R<String> {
    if replacement.is_callable() {
        let mut args = vec![Val::Str(matched.into())];
        args.extend(groups.iter().cloned());
        args.push(Val::Num(position as f64));
        args.push(Val::Str(subject.into()));
        let r = it.call(replacement, &Val::Undef, &args)?;
        return Ok(it.str_of(&r)?.to_string());
    }
    let tpl = it.str_of(replacement)?;
    let su = units(subject);
    let m = units(matched).len();
    let mut out = String::new();
    let mut chars = tpl.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('$') => {
                chars.next();
                out.push('$');
            }
            Some('&') => {
                chars.next();
                out.push_str(matched);
            }
            Some('`') => {
                chars.next();
                out.push_str(&from_units(&su[..position.min(su.len())]));
            }
            Some('\'') => {
                chars.next();
                out.push_str(&from_units(&su[(position + m).min(su.len())..]));
            }
            Some(d) if d.is_ascii_digit() => {
                chars.next();
                let mut n = d.to_digit(10).unwrap_or(0) as usize;
                if let Some(e) = chars.peek().and_then(|c| c.to_digit(10)) {
                    let two = n * 10 + e as usize;
                    if two >= 1 && two <= groups.len() {
                        chars.next();
                        n = two;
                    }
                }
                match groups.get(n.wrapping_sub(1)) {
                    Some(g) if n >= 1 => {
                        if !matches!(g, Val::Undef) {
                            out.push_str(&it.str_of(g)?);
                        }
                    }
                    _ => {
                        out.push('$');
                        out.push(d);
                    }
                }
            }
            _ => out.push('$'),
        }
    }
    Ok(out)
}

fn replace_in(it: &mut Interp, this: &Val, a: &[Val], all: bool) -> R {
    let s = this_str(it, this)?;
    let pat = arg(a, 0);
    if matches!(&pat, Val::Obj(o) if matches!(o.borrow().kind, Kind::RegExp(..))) {
        return crate::regex::replace(it, &s, &pat, &arg(a, 1), all);
    }
    let p = it.str_of(&pat)?;
    let su = units(&s);
    let pu = units(&p);
    let mut out = String::new();
    let mut last = 0;
    let mut from = 0;
    while let Some(i) = find_units(&su, &pu, from) {
        out.push_str(&from_units(&su[last..i]));
        let r = substitute(it, &arg(a, 1), &p, i, &s, &[])?;
        out.push_str(&r);
        last = i + pu.len();
        from = if pu.is_empty() { i + 1 } else { last };
        if !all || from > su.len() {
            break;
        }
    }
    out.push_str(&from_units(&su[last.min(su.len())..]));
    it.string(out)
}

fn replace(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    replace_in(it, this, a, false)
}

fn replace_all(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    if let Val::Obj(o) = &arg(a, 0)
        && let Kind::RegExp(_, f) = &o.borrow().kind
        && !f.contains('g')
    {
        return Err(it.error("TypeError", "replaceAll must be called with a global RegExp"));
    }
    replace_in(it, this, a, true)
}

fn value_of(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    match this {
        Val::Str(s) => Ok(Val::Str(s.clone())),
        Val::Obj(o) => match &o.borrow().kind {
            Kind::Str(s) => Ok(Val::Str(s.clone())),
            _ => Err(crate::builtins::incompatible(it, "String.prototype.valueOf")),
        },
        _ => Err(crate::builtins::incompatible(it, "String.prototype.valueOf")),
    }
}

fn locale_compare(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let s = this_str(it, this)?;
    let t = it.str_of(&arg(a, 0))?;
    Ok(Val::Num(match s.cmp(&t) {
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Equal => 0.0,
        std::cmp::Ordering::Greater => 1.0,
    }))
}
