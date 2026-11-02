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
        ("isWellFormed", is_well_formed, 0),
        ("toWellFormed", to_well_formed, 0),
        ("matchAll", crate::regex::match_all, 1),
    ] {
        method(it, &proto, name, f, n)?;
    }
    // `trimLeft` and `trimRight` are the same functions as `trimStart` and `trimEnd`.
    for (alias, name) in [("trimLeft", "trimStart"), ("trimRight", "trimEnd")] {
        let f = proto.borrow().get_own(name).map(|p| p.value.clone()).unwrap_or(Val::Undef);
        it.define(&proto, alias, Prop::hidden(f))?;
    }
    for (name, f, n) in HTML_METHODS {
        method(it, &proto, name, f, n)?;
    }
    method(it, &c, "raw", raw, 1)?;
    Ok(())
}

/// The HTML methods of Annex B (measured on a `refrouter` 9.0.4 on 02-11).
const HTML_METHODS: [(&str, NativeFn, usize); 13] = [
    ("anchor", |it, t, a| html(it, t, "a", Some(("name", a))), 1),
    ("big", |it, t, _| html(it, t, "big", None), 0),
    ("blink", |it, t, _| html(it, t, "blink", None), 0),
    ("bold", |it, t, _| html(it, t, "b", None), 0),
    ("fixed", |it, t, _| html(it, t, "tt", None), 0),
    ("fontcolor", |it, t, a| html(it, t, "font", Some(("color", a))), 1),
    ("fontsize", |it, t, a| html(it, t, "font", Some(("size", a))), 1),
    ("italics", |it, t, _| html(it, t, "i", None), 0),
    ("link", |it, t, a| html(it, t, "a", Some(("href", a))), 1),
    ("small", |it, t, _| html(it, t, "small", None), 0),
    ("strike", |it, t, _| html(it, t, "strike", None), 0),
    ("sub", |it, t, _| html(it, t, "sub", None), 0),
    ("sup", |it, t, _| html(it, t, "sup", None), 0),
];

/// `this` as a string for a method that names itself when `this` is null or undefined.
fn this_string(it: &mut Interp, this: &Val) -> R<Name> {
    if this.is_nullish() {
        let name = it.native.clone();
        return it.type_error(&format!(
            "String.prototype.{name} called on incompatible {}",
            it.show(this)
        ));
    }
    it.str_of(this)
}

/// CreateHTML: `<tag attr="value">text</tag>`, with `"` in the value as `&quot;`.
fn html(it: &mut Interp, this: &Val, tag: &str, attr: Option<(&str, &[Val])>) -> R {
    let text = this_string(it, this)?;
    let open = match attr {
        Some((name, a)) => {
            let v = it.str_of(&arg(a, 0))?;
            format!("<{tag} {name}=\"{}\">", v.replace('"', "&quot;"))
        }
        None => format!("<{tag}>"),
    };
    it.string(format!("{open}{text}</{tag}>"))
}

fn is_well_formed(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let s = this_string(it, this)?;
    let u = units(&s);
    Ok(Val::Bool(well_formed(&u)))
}

/// Whether `u` has no lone surrogate.
fn well_formed(u: &[u16]) -> bool {
    char::decode_utf16(u.iter().copied()).all(|c| c.is_ok())
}

fn to_well_formed(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let s = this_string(it, this)?;
    let u = units(&s);
    if well_formed(&u) {
        return Ok(Val::Str(s));
    }
    let fixed: Vec<u16> = char::decode_utf16(u.iter().copied())
        .flat_map(|c| {
            let mut buf = [0u16; 2];
            c.unwrap_or('\u{fffd}').encode_utf16(&mut buf).to_vec()
        })
        .collect();
    it.string(from_units(&fixed))
}

/// `String.raw(template, ...substitutions)`.
fn raw(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let template = arg(a, 0);
    let cooked = it.object_of(&template)?;
    let strings = it.get(&Val::Obj(cooked), "raw")?;
    let strings = Val::Obj(it.object_of(&strings)?);
    let len = it.get(&strings, "length")?;
    let len = it.number_of(&len)?;
    let len = if len.is_finite() && len > 0.0 { len as usize } else { 0 };
    let mut out = String::new();
    for i in 0..len {
        let part = it.get(&strings, &i.to_string())?;
        out.push_str(&it.str_of(&part)?);
        if i + 1 < len
            && let Some(sub) = a.get(i + 1)
        {
            out.push_str(&it.str_of(sub)?);
        }
    }
    it.string(out)
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
        let m = format!("String.prototype.{} called on incompatible {}", it.native, it.show(this));
        return it.type_error(&m);
    }
    it.str_of(this)
}

pub use crate::text::{from_units, pair_surrogates, to_utf8, units, utf16_len};

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
        match (n.fract() == 0.0 && (0.0..=1_114_111.0).contains(&n)).then_some(n as u32) {
            Some(cp) => crate::text::push_cp(&mut s, cp),
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
    let pos = it.number_of(&arg(a, 1))?;
    if n.len() > u.len() {
        return Ok(Val::Num(-1.0));
    }
    let last = u.len() - n.len();
    let from = if pos.is_nan() { last } else { (pos.max(0.0) as usize).min(last) };
    let r = (0..=from).rev().find(|&i| u[i..i + n.len()] == n[..]);
    Ok(Val::Num(r.map_or(-1.0, |i| i as f64)))
}

fn no_regex(it: &mut Interp, v: &Val) -> R<()> {
    if matches!(v, Val::Obj(o) if matches!(o.borrow().kind, Kind::RegExp(..))) {
        return it.type_error(&format!("Invalid type: first can't be a Regular Expression"));
    }
    Ok(())
}

fn includes(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    no_regex(it, &arg(a, 0))?;
    index_of(it, this, a).map(|v| Val::Bool(!matches!(v, Val::Num(n) if n < 0.0)))
}

fn starts_with(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    no_regex(it, &arg(a, 0))?;
    let u = units(&this_str(it, this)?);
    let n = units(&it.str_of(&arg(a, 0))?);
    let from = clamp(int_arg(it, &arg(a, 1), 0.0)?, u.len());
    Ok(Val::Bool(u[from..].starts_with(&n)))
}

fn ends_with(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    no_regex(it, &arg(a, 0))?;
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
    // Each character on its own: no final sigma (measured on a `refrouter` 9.0.4 on 02-11).
    it.string(s.chars().flat_map(char::to_lowercase).collect())
}

fn is_ws(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

fn trim(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let s = this_str(it, this)?;
    it.string(s.trim_matches(is_ws).to_owned())
}

pub(crate) fn trim_start(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let s = this_str(it, this)?;
    it.string(s.trim_start_matches(is_ws).to_owned())
}

pub(crate) fn trim_end(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let s = this_str(it, this)?;
    it.string(s.trim_end_matches(is_ws).to_owned())
}

fn pad(it: &mut Interp, this: &Val, a: &[Val], start: bool) -> R {
    let s = this_str(it, this)?;
    let len = utf16_len(&s);
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
    if n < 0.0 {
        return Err(it.error("RangeError", "repeat count must be non-negative"));
    }
    if n.is_infinite() {
        let m = "repeat count must be less than infinity and not overflow maximum string size";
        return Err(it.error("RangeError", m));
    }
    let total = (s.len() as f64) * n;
    // Past the longest string (`Interp::max_string`).
    if total > it.max_string() as f64 {
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
    it.joined_string(s)
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
                units(&s).into_iter().map(|c| Val::Str(from_units(&[c]).into())).collect()
            } else {
                let (su, pu) = (units(&s), units(&sep));
                let mut out = Vec::new();
                let mut last = 0;
                let mut i = 0;
                while i + pu.len() <= su.len() {
                    if su[i..i + pu.len()] == pu[..] {
                        out.push(Val::Str(from_units(&su[last..i]).into()));
                        i += pu.len();
                        last = i;
                    } else {
                        i += 1;
                    }
                }
                out.push(Val::Str(from_units(&su[last..]).into()));
                out
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
    named: &[(String, Val)],
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
            Some('<') if !named.is_empty() => {
                chars.next();
                let name: String = chars.by_ref().take_while(|c| *c != '>').collect();
                if let Some((_, v)) = named.iter().find(|(n, _)| *n == name)
                    && !matches!(v, Val::Undef)
                {
                    out.push_str(&it.str_of(v)?);
                }
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
        let r = substitute(it, &arg(a, 1), &p, i, &s, &[], &[])?;
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
    // The engine of RefDb 9.0.4 has no ICU: the difference of the first UTF-16 units that
    // differ, else of the lengths (measured: `"Abc".localeCompare("abc")` is -32).
    let (x, y) = (units(&s), units(&t));
    let d = x
        .iter()
        .zip(&y)
        .find(|(p, q)| p != q)
        .map_or_else(|| x.len() as f64 - y.len() as f64, |(p, q)| f64::from(*p) - f64::from(*q));
    Ok(Val::Num(d))
}
