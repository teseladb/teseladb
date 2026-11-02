//! `JSON.parse` and `JSON.stringify` (ECMA-262 §25.5).

use crate::builtins::method;
use crate::interp::{Interp, PROP_BYTES, R};
use crate::value::*;

fn arg(args: &[Val], i: usize) -> Val {
    args.get(i).cloned().unwrap_or(Val::Undef)
}

pub fn install(it: &mut Interp, g: &ObjRef) -> R<()> {
    let j = it.object()?;
    it.define(g, "JSON", Prop::hidden(Val::Obj(j.clone())))?;
    method(it, &j, "parse", parse, 2)?;
    method(it, &j, "stringify", stringify, 3)?;
    method(it, &j, "rawJSON", raw_json, 1)?;
    method(it, &j, "isRawJSON", is_raw_json, 1)?;
    Ok(())
}

/// The internal slot of the objects `JSON.rawJSON` makes.
const RAW_JSON: &str = "@@isRawJSON";

/// `JSON.rawJSON(text)`: a frozen object whose JSON form is `text`, a primitive JSON value.
fn raw_json(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let text = it.str_of(&arg(a, 0))?;
    if text.is_empty() {
        let m = "JSON.rawJSON: argument cannot be empty when converted to string";
        return Err(it.error("SyntaxError", m));
    }
    let edge = |c: Option<char>| matches!(c, Some('\t' | '\n' | '\r' | ' '));
    if edge(text.chars().next()) || edge(text.chars().last()) {
        let m = "JSON.rawJSON: first or last character cannot be any of tab, line feed, \
                 carriage return, or space";
        return Err(it.error("SyntaxError", m));
    }
    if matches!(text.chars().next(), Some('{' | '[')) {
        let m = "JSON.rawJSON: outermost value must not be an array or object";
        return Err(it.error("SyntaxError", m));
    }
    parse_text(it, &text)?;
    let o = it.alloc(None, Kind::Ordinary)?;
    it.define(&o, RAW_JSON, Prop::hidden(Val::Bool(true)))?;
    it.define(&o, "rawJSON", Prop::data(Val::Str(text)))?;
    crate::props::lock(&o, true);
    Ok(Val::Obj(o))
}

fn is_raw_json(_: &mut Interp, _: &Val, a: &[Val]) -> R {
    Ok(Val::Bool(matches!(arg(a, 0), Val::Obj(o) if o.borrow().get_own(RAW_JSON).is_some())))
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
    /// The source of each value read, innermost last (for the reviver's `context.source`).
    out: Vec<Src>,
}

/// Where a parsed value came from: a primitive's text and value, or the parts of an object or
/// array.
enum Src {
    Prim(String, Val),
    Obj(Vec<(String, Src)>),
    Arr(Vec<Src>),
}

fn parse(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let text = it.str_of(&arg(a, 0))?;
    let (v, src) = parse_text(it, &text)?;
    let reviver = arg(a, 1);
    if reviver.is_callable() {
        let holder = it.object()?;
        it.define(&holder, "", Prop::data(v))?;
        return revive(it, &Val::Obj(holder), "", &reviver, src.as_ref());
    }
    Ok(v)
}

/// A whole JSON text: its value and where each part came from.
fn parse_text(it: &mut Interp, text: &str) -> R<(Val, Option<Src>)> {
    let mut p = P { s: text.as_bytes(), i: 0, out: Vec::new() };
    let v = p.value(it, 0)?;
    p.ws();
    if p.i < p.s.len() {
        return Err(p.err(it, "unexpected non-whitespace character after JSON data"));
    }
    Ok((v, p.out.pop()))
}

/// InternalizeJSONProperty (ECMA-262 §25.5.1.1) with the source of what was parsed: the reviver
/// gets `{source}` for a primitive still equal to what the text gave.
fn revive(it: &mut Interp, holder: &Val, key: &str, f: &Val, src: Option<&Src>) -> R {
    let v = it.get(holder, key)?;
    if let Val::Obj(o) = &v {
        if crate::proxy::is_array(it, &v)? {
            let len = it.get(&v, "length")?;
            let len = it.number_of(&len)?;
            let len = if len.is_nan() || len <= 0.0 {
                0
            } else {
                len.min(9.007_199_254_740_991e15) as u64
            };
            for i in 0..len {
                let k = i.to_string();
                let child = match src {
                    Some(Src::Arr(items)) => items.get(i as usize),
                    _ => None,
                };
                let nv = revive(it, &v, &k, f, child)?;
                put(it, o, &k, nv)?;
            }
        } else {
            for k in it.keys(o, true)? {
                let child = match src {
                    Some(Src::Obj(entries)) => {
                        entries.iter().rev().find(|(n, _)| *n == *k).map(|(_, s)| s)
                    }
                    _ => None,
                };
                let nv = revive(it, &v, &k, f, child)?;
                put(it, o, &k, nv)?;
            }
        }
    }
    let context = it.object()?;
    if let Some(Src::Prim(text, orig)) = src
        && !matches!(v, Val::Obj(_))
        && crate::value::same_value_zero(orig, &v)
    {
        let t = it.string(text.clone())?;
        it.define(&context, "source", Prop::data(t))?;
    }
    it.call(f, holder, &[Val::Str(key.into()), v, Val::Obj(context)])
}

/// What the reviver gave for `o[k]`: deleted when undefined, else set as a new data property.
/// Neither throws when the property refuses (non-configurable).
fn put(it: &mut Interp, o: &ObjRef, k: &str, nv: Val) -> R<()> {
    if matches!(nv, Val::Undef) {
        let outer = std::mem::replace(&mut it.strict, false);
        let r = it.delete_key(o, k);
        it.strict = outer;
        r?;
        return Ok(());
    }
    crate::props::create_data_property(it, o, k, nv)?;
    Ok(())
}

impl P<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn err(&self, it: &Interp, what: &str) -> crate::interp::Ctrl {
        // SpiderMonkey's form: "JSON.parse: <what> at line L column C of the JSON data".
        let before = &self.s[..self.i.min(self.s.len())];
        let line = before.iter().filter(|&&b| b == b'\n').count() + 1;
        let col = before.iter().rev().take_while(|&&b| b != b'\n').count() + 1;
        it.error(
            "SyntaxError",
            &format!("JSON.parse: {what} at line {line} column {col} of the JSON data"),
        )
    }

    fn value(&mut self, it: &mut Interp, depth: usize) -> R {
        it.step()?;
        if depth > 1000 {
            return Err(it.error("InternalError", "too much recursion"));
        }
        self.ws();
        let Some(&c) = self.s.get(self.i) else {
            return Err(self.err(it, "unexpected end of data"));
        };
        match c {
            b'{' => {
                self.i += 1;
                let o = it.object()?;
                let mut parts = Vec::new();
                self.ws();
                if self.s.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    self.out.push(Src::Obj(parts));
                    return Ok(Val::Obj(o));
                }
                loop {
                    self.ws();
                    if self.i >= self.s.len() {
                        return Err(self.err(it, "end of data while reading object contents"));
                    }
                    if self.s.get(self.i) != Some(&b'"') {
                        return Err(self.err(it, "expected double-quoted property name"));
                    }
                    let k = self.string(it)?;
                    self.ws();
                    if self.i >= self.s.len() {
                        let m = "end of data after property name when ':' was expected";
                        return Err(self.err(it, m));
                    }
                    if self.s.get(self.i) != Some(&b':') {
                        return Err(self.err(it, "expected ':' after property name in object"));
                    }
                    self.i += 1;
                    let v = self.value(it, depth + 1)?;
                    if let Some(src) = self.out.pop() {
                        parts.push((k.clone(), src));
                    }
                    it.define(&o, &k, Prop::data(v))?;
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            self.out.push(Src::Obj(parts));
                            return Ok(Val::Obj(o));
                        }
                        None => {
                            return Err(self.err(it, "end of data after property value in object"));
                        }
                        _ => {
                            return Err(
                                self.err(it, "expected ',' or '}' after property value in object")
                            );
                        }
                    }
                }
            }
            b'[' => {
                self.i += 1;
                let mut items = Vec::new();
                let mut parts = Vec::new();
                self.ws();
                if self.s.get(self.i) == Some(&b']') {
                    self.i += 1;
                    self.out.push(Src::Arr(parts));
                    return it.array(items);
                }
                loop {
                    it.charge(PROP_BYTES)?;
                    items.push(self.value(it, depth + 1)?);
                    if let Some(src) = self.out.pop() {
                        parts.push(src);
                    }
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            self.out.push(Src::Arr(parts));
                            return it.array(items);
                        }
                        None => {
                            return Err(self.err(it, "end of data when ',' or ']' was expected"));
                        }
                        _ => return Err(self.err(it, "expected ',' or ']' after array element")),
                    }
                }
            }
            b'"' => {
                let start = self.i;
                let s = self.string(it)?;
                self.primitive(start, Val::Str(s.into()))
            }
            b't' if self.s[self.i..].starts_with(b"true") => {
                self.i += 4;
                self.primitive(self.i - 4, Val::Bool(true))
            }
            b'f' if self.s[self.i..].starts_with(b"false") => {
                self.i += 5;
                self.primitive(self.i - 5, Val::Bool(false))
            }
            b'n' if self.s[self.i..].starts_with(b"null") => {
                self.i += 4;
                self.primitive(self.i - 4, Val::Null)
            }
            b'a'..=b'z' | b'A'..=b'Z' => Err(self.err(it, "unexpected keyword")),
            b'-' | b'0'..=b'9' => {
                let start = self.i;
                if c == b'-' {
                    self.i += 1;
                }
                let digits = |p: &mut Self| {
                    let s = p.i;
                    while p.i < p.s.len() && p.s[p.i].is_ascii_digit() {
                        p.i += 1;
                    }
                    p.i > s
                };
                if !digits(self) {
                    return Err(self.err(it, "no number after minus sign"));
                }
                if self.s.get(self.i) == Some(&b'.') {
                    self.i += 1;
                    if !digits(self) {
                        return Err(self.err(it, "missing digits after decimal point"));
                    }
                }
                if matches!(self.s.get(self.i), Some(b'e' | b'E')) {
                    self.i += 1;
                    if matches!(self.s.get(self.i), Some(b'+' | b'-')) {
                        self.i += 1;
                    }
                    if !digits(self) {
                        return Err(self.err(it, "missing digits after exponent indicator"));
                    }
                }
                let t = std::str::from_utf8(&self.s[start..self.i]).unwrap_or("0");
                self.primitive(start, Val::Num(t.parse().unwrap_or(f64::NAN)))
            }
            _ => Err(self.err(it, "unexpected character")),
        }
    }

    /// A primitive read from `start` to here, its source kept.
    fn primitive(&mut self, start: usize, v: Val) -> R {
        let text = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
        self.out.push(Src::Prim(text, v.clone()));
        Ok(v)
    }

    fn string(&mut self, it: &mut Interp) -> R<String> {
        self.i += 1;
        let mut units: Vec<u16> = Vec::new();
        loop {
            let Some(&c) = self.s.get(self.i) else {
                return Err(self.err(it, "unterminated string"));
            };
            match c {
                b'"' => {
                    self.i += 1;
                    it.charge(units.len())?;
                    return Ok(crate::strings::from_units(&units));
                }
                b'\\' => {
                    self.i += 1;
                    let e = self.s.get(self.i).copied();
                    self.i += 1;
                    match e {
                        Some(b'"') => units.push(u16::from(b'"')),
                        Some(b'\\') => units.push(u16::from(b'\\')),
                        Some(b'/') => units.push(u16::from(b'/')),
                        Some(b'b') => units.push(8),
                        Some(b'f') => units.push(12),
                        Some(b'n') => units.push(10),
                        Some(b'r') => units.push(13),
                        Some(b't') => units.push(9),
                        Some(b'u') => {
                            let hex = self
                                .s
                                .get(self.i..self.i + 4)
                                .and_then(|h| std::str::from_utf8(h).ok());
                            let Some(n) = hex.and_then(|h| u16::from_str_radix(h, 16).ok()) else {
                                return Err(self.err(it, "bad Unicode escape"));
                            };
                            self.i += 4;
                            units.push(n);
                        }
                        _ => return Err(self.err(it, "bad escaped character")),
                    }
                }
                0..=0x1f => return Err(self.err(it, "bad control character in string literal")),
                _ => {
                    // A run of plain characters, as the engine keeps them.
                    let end = self.s[self.i..]
                        .iter()
                        .position(|&b| b == b'"' || b == b'\\' || b < 0x20)
                        .map_or(self.s.len(), |n| self.i + n);
                    let run = std::str::from_utf8(&self.s[self.i..end]).unwrap_or("\u{fffd}");
                    units.extend(crate::text::units(run));
                    self.i = end;
                }
            }
        }
    }
}

/// A string as JSON writes it.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for cp in crate::text::cps(s) {
        match char::from_u32(cp) {
            Some('"') => out.push_str("\\\""),
            Some('\\') => out.push_str("\\\\"),
            Some('\n') => out.push_str("\\n"),
            Some('\r') => out.push_str("\\r"),
            Some('\t') => out.push_str("\\t"),
            Some('\u{8}') => out.push_str("\\b"),
            Some('\u{c}') => out.push_str("\\f"),
            Some(c) if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            Some(c) => crate::text::push_char(&mut out, c),
            None => out.push_str(&format!("\\u{cp:04x}")),
        }
    }
    out.push('"');
    out
}

struct S {
    indent: String,
    replacer: Val,
    allow: Option<Vec<Name>>,
    stack: Vec<ObjRef>,
}

fn stringify(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let replacer = arg(a, 1);
    let allow = if matches!(replacer, Val::Obj(_)) && crate::proxy::is_array(it, &replacer)? {
        Some(property_list(it, &replacer)?)
    } else {
        None
    };
    // A Number or String object counts as its value (ECMA-262 §25.5.2.1 step 5).
    let space = match arg(a, 2) {
        Val::Obj(o) => match &o.borrow().kind {
            Kind::Num(n) => Val::Num(*n),
            Kind::Str(s) => Val::Str(s.clone()),
            _ => Val::Undef,
        },
        v => v,
    };
    let indent = match space {
        Val::Num(n) => " ".repeat(n.trunc().clamp(0.0, 10.0) as usize),
        Val::Str(s) => {
            crate::text::from_units(&crate::text::units(&s)[..crate::text::utf16_len(&s).min(10)])
        }
        _ => String::new(),
    };
    let mut st = S {
        indent,
        replacer: if replacer.is_callable() { replacer } else { Val::Undef },
        allow,
        stack: Vec::new(),
    };
    let holder = it.object()?;
    it.define(&holder, "", Prop::data(arg(a, 0)))?;
    let mut out = String::new();
    if write(it, &mut st, &Val::Obj(holder), "", &mut out, "")? {
        it.string(out)
    } else {
        Ok(Val::Undef)
    }
}

/// The keys a replacer array allows: its strings and numbers (and their objects), in order and
/// once each.
fn property_list(it: &mut Interp, replacer: &Val) -> R<Vec<Name>> {
    let len = it.get(replacer, "length")?;
    let len = it.number_of(&len)?;
    let len = if len.is_nan() || len <= 0.0 { 0 } else { len as u64 };
    let mut keys: Vec<Name> = Vec::new();
    for i in 0..len {
        let v = it.get(replacer, &i.to_string())?;
        let item = match &v {
            Val::Str(s) => Some(s.clone()),
            Val::Num(_) => Some(it.str_of(&v)?),
            Val::Obj(o) if matches!(o.borrow().kind, Kind::Num(_) | Kind::Str(_)) => {
                Some(it.str_of(&v)?)
            }
            _ => None,
        };
        if let Some(k) = item
            && !keys.contains(&k)
        {
            keys.push(k);
        }
    }
    Ok(keys)
}

/// Writes property `key` of `holder`; `false` when it has no JSON form.
fn write(
    it: &mut Interp,
    st: &mut S,
    holder: &Val,
    key: &str,
    out: &mut String,
    gap: &str,
) -> R<bool> {
    it.step()?;
    let mut v = it.get(holder, key)?;
    if let Val::Obj(_) = &v {
        let f = it.get(&v, "toJSON")?;
        if f.is_callable() {
            v = it.call(&f, &v, &[Val::Str(key.into())])?;
        }
    }
    if st.replacer.is_callable() {
        let r = st.replacer.clone();
        v = it.call(&r, holder, &[Val::Str(key.into()), v])?;
    }
    if let Val::Obj(o) = &v {
        let prim = match &o.borrow().kind {
            Kind::Num(n) => Some(Val::Num(*n)),
            Kind::Str(s) => Some(Val::Str(s.clone())),
            Kind::Bool(b) => Some(Val::Bool(*b)),
            Kind::BigInt(b) => Some(Val::BigInt(b.clone())),
            _ => None,
        };
        if let Some(p) = prim {
            v = p;
        }
    }
    if let Val::Obj(o) = &v
        && o.borrow().get_own(RAW_JSON).is_some()
    {
        let raw = it.get(&v, "rawJSON")?;
        out.push_str(&it.str_of(&raw)?);
        return Ok(true);
    }
    match &v {
        Val::Null => out.push_str("null"),
        Val::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Val::Num(n) => {
            out.push_str(&if n.is_finite() { crate::number::to_string(*n) } else { "null".into() })
        }
        Val::Str(s) => out.push_str(&quote(s)),
        Val::Undef | Val::Hole | Val::Sym(_) => return Ok(false),
        Val::BigInt(_) => return it.type_error("BigInt value can't be serialized in JSON"),
        Val::Obj(o) => {
            if o.borrow().is_callable() {
                return Ok(false);
            }
            if st.stack.iter().any(|x| x.ptr_eq(o)) {
                return it.type_error("cyclic object value");
            }
            st.stack.push(o.clone());
            let inner = format!("{gap}{}", st.indent);
            let is_array = crate::proxy::is_array(it, &v)?;
            let mut parts = Vec::new();
            if is_array {
                let len = it.get(&v, "length")?;
                let len = it.number_of(&len)? as usize;
                for i in 0..len {
                    let mut s = String::new();
                    if !write(it, st, &v, &i.to_string(), &mut s, &inner)? {
                        s = "null".into();
                    }
                    parts.push(s);
                }
            } else {
                let keys = match &st.allow {
                    Some(k) => k.clone(),
                    None => it.keys(o, true)?,
                };
                for k in keys {
                    let mut s = String::new();
                    if write(it, st, &v, &k, &mut s, &inner)? {
                        let sep = if st.indent.is_empty() { ":" } else { ": " };
                        parts.push(format!("{}{sep}{s}", quote(&k)));
                    }
                }
            }
            st.stack.pop();
            let (open, close) = if is_array { ('[', ']') } else { ('{', '}') };
            out.push(open);
            if !parts.is_empty() {
                if st.indent.is_empty() {
                    out.push_str(&parts.join(","));
                } else {
                    out.push('\n');
                    out.push_str(&inner);
                    out.push_str(&parts.join(&format!(",\n{inner}")));
                    out.push('\n');
                    out.push_str(gap);
                }
            }
            out.push(close);
            it.charge(out.len().min(1 << 16))?;
        }
    }
    Ok(true)
}
