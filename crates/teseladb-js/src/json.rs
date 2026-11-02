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
    Ok(())
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
}

fn parse(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let text = it.str_of(&arg(a, 0))?;
    let mut p = P { s: text.as_bytes(), i: 0 };
    let v = p.value(it, 0)?;
    p.ws();
    if p.i < p.s.len() {
        return Err(p.err(it, "unexpected non-whitespace character after JSON data"));
    }
    let reviver = arg(a, 1);
    if reviver.is_callable() {
        let holder = it.object()?;
        it.define(&holder, "", Prop::data(v))?;
        return revive(it, &Val::Obj(holder), "", &reviver);
    }
    Ok(v)
}

fn revive(it: &mut Interp, holder: &Val, key: &str, f: &Val) -> R {
    let v = it.get(holder, key)?;
    if let Val::Obj(o) = &v {
        for k in it.keys(o, true)? {
            let nv = revive(it, &v, &k, f)?;
            if matches!(nv, Val::Undef) {
                o.borrow_mut().remove(&k);
            } else {
                it.set(&v, &k, nv)?;
            }
        }
    }
    it.call(f, holder, &[Val::Str(key.into()), v])
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
                self.ws();
                if self.s.get(self.i) == Some(&b'}') {
                    self.i += 1;
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
                    it.define(&o, &k, Prop::data(v))?;
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
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
                self.ws();
                if self.s.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return it.array(items);
                }
                loop {
                    it.charge(PROP_BYTES)?;
                    items.push(self.value(it, depth + 1)?);
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
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
                let s = self.string(it)?;
                Ok(Val::Str(s.into()))
            }
            b't' if self.s[self.i..].starts_with(b"true") => {
                self.i += 4;
                Ok(Val::Bool(true))
            }
            b'f' if self.s[self.i..].starts_with(b"false") => {
                self.i += 5;
                Ok(Val::Bool(false))
            }
            b'n' if self.s[self.i..].starts_with(b"null") => {
                self.i += 4;
                Ok(Val::Null)
            }
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
                Ok(Val::Num(t.parse().unwrap_or(f64::NAN)))
            }
            _ => Err(self.err(it, "unexpected character")),
        }
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
                    return Ok(String::from_utf16_lossy(&units));
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
                    // One UTF-8 character.
                    let len = match c {
                        0..=0x7f => 1,
                        0xc0..=0xdf => 2,
                        0xe0..=0xef => 3,
                        _ => 4,
                    };
                    let ch = std::str::from_utf8(&self.s[self.i..(self.i + len).min(self.s.len())])
                        .unwrap_or("\u{fffd}");
                    let mut buf = [0u16; 2];
                    for ch in ch.chars() {
                        units.extend_from_slice(ch.encode_utf16(&mut buf));
                    }
                    self.i += len;
                }
            }
        }
    }
}

/// A string as JSON writes it.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
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
    let allow = match &replacer {
        Val::Obj(o) if matches!(o.borrow().kind, Kind::Array(_)) => {
            let mut keys = Vec::new();
            for x in it.iterate(&replacer)? {
                keys.push(it.str_of(&x)?);
            }
            Some(keys)
        }
        _ => None,
    };
    let indent = match arg(a, 2) {
        Val::Num(n) => " ".repeat(n.clamp(0.0, 10.0) as usize),
        Val::Str(s) => s.chars().take(10).collect(),
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
    match &v {
        Val::Null => out.push_str("null"),
        Val::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Val::Num(n) => {
            out.push_str(&if n.is_finite() { crate::number::to_string(*n) } else { "null".into() })
        }
        Val::Str(s) => out.push_str(&quote(s)),
        Val::Undef | Val::Sym(_) => return Ok(false),
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
