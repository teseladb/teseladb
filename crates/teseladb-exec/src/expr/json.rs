//! Plain JSON for `$toString` of arrays and documents and for `$toArray` and `$toObject` of a
//! string (measured on a `refrouter` 9.0.4). Reading is strict RFC 8259 JSON: keys that start with
//! `$` stay plain fields, an integer is an `int`, a `long` when it does not fit or a double when
//! neither fits, a number with a fraction or exponent is a double, and a repeated key keeps its
//! first place with the last value. Writing gives numbers, booleans and `null` as they are,
//! strings escaped, NaN and infinities as strings, and any other value as the string `$toString`
//! gives it.

use teseladb_bson::{Document, Value};

use super::convert::scalar_text;
use super::string::double_text;

/// The value of JSON `text`, `None` when it is not valid JSON.
pub(super) fn parse(text: &str) -> Option<Value> {
    let mut p = Parser { s: text.as_bytes(), i: 0 };
    p.ws();
    let v = p.value()?;
    p.ws();
    (p.i == p.s.len()).then_some(v)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, word: &[u8]) -> bool {
        let found = self.s[self.i..].starts_with(word);
        if found {
            self.i += word.len();
        }
        found
    }

    fn value(&mut self) -> Option<Value> {
        match self.peek()? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => self.string().map(Value::String),
            b't' => self.eat(b"true").then_some(Value::Boolean(true)),
            b'f' => self.eat(b"false").then_some(Value::Boolean(false)),
            b'n' => self.eat(b"null").then_some(Value::Null),
            _ => self.number(),
        }
    }

    fn object(&mut self) -> Option<Value> {
        self.i += 1;
        let mut d = Document::new();
        self.ws();
        if self.eat(b"}") {
            return Some(Value::Document(d));
        }
        loop {
            self.ws();
            let k = self.string()?;
            self.ws();
            if !self.eat(b":") {
                return None;
            }
            self.ws();
            let v = self.value()?;
            d.insert(k, v);
            self.ws();
            if self.eat(b"}") {
                return Some(Value::Document(d));
            }
            if !self.eat(b",") {
                return None;
            }
        }
    }

    fn array(&mut self) -> Option<Value> {
        self.i += 1;
        let mut out = Vec::new();
        self.ws();
        if self.eat(b"]") {
            return Some(Value::Array(out));
        }
        loop {
            self.ws();
            out.push(self.value()?);
            self.ws();
            if self.eat(b"]") {
                return Some(Value::Array(out));
            }
            if !self.eat(b",") {
                return None;
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = self.s.get(self.i..self.i + 4)?;
        let text = std::str::from_utf8(digits).ok()?;
        let v = u32::from_str_radix(text, 16).ok()?;
        self.i += 4;
        Some(v)
    }

    fn string(&mut self) -> Option<String> {
        if !self.eat(b"\"") {
            return None;
        }
        let mut out = Vec::new();
        loop {
            let c = self.peek()?;
            self.i += 1;
            match c {
                b'"' => return String::from_utf8(out).ok(),
                b'\\' => {
                    let e = self.peek()?;
                    self.i += 1;
                    let ch = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => self.unicode()?,
                        _ => return None,
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                c if c < 0x20 => return None,
                c => out.push(c),
            }
        }
    }

    /// The character of `\uXXXX`, joining a surrogate pair.
    fn unicode(&mut self) -> Option<char> {
        let hi = self.hex4()?;
        if (0xD800..0xDC00).contains(&hi) {
            if !self.eat(b"\\u") {
                return None;
            }
            let lo = self.hex4()?;
            if !(0xDC00..0xE000).contains(&lo) {
                return None;
            }
            return char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00));
        }
        char::from_u32(hi)
    }

    fn digits(&mut self) -> usize {
        let start = self.i;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        self.i - start
    }

    fn number(&mut self) -> Option<Value> {
        let start = self.i;
        self.eat(b"-");
        let int_start = self.i;
        let n = self.digits();
        if n == 0 || (n > 1 && self.s[int_start] == b'0') {
            return None;
        }
        let mut real = false;
        if self.eat(b".") {
            real = true;
            if self.digits() == 0 {
                return None;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            real = true;
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if self.digits() == 0 {
                return None;
            }
        }
        let text = std::str::from_utf8(&self.s[start..self.i]).ok()?;
        if !real {
            if let Ok(n) = text.parse::<i32>() {
                return Some(Value::Int32(n));
            }
            if let Ok(n) = text.parse::<i64>() {
                return Some(Value::Int64(n));
            }
        }
        let x: f64 = text.parse().ok()?;
        x.is_finite().then_some(Value::Double(x))
    }
}

/// `v` as `$toString` writes an array or a document: JSON.
pub(super) fn write(v: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, v);
    out
}

fn write_string(out: &mut String, s: &str) {
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
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_value(out: &mut String, v: &Value) {
    match v {
        Value::Null | Value::Undefined => out.push_str("null"),
        Value::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int32(n) => out.push_str(&n.to_string()),
        Value::Int64(n) => out.push_str(&n.to_string()),
        Value::Double(x) if x.is_finite() => out.push_str(&double_text(*x)),
        Value::Decimal128(d) if d.to_string().bytes().any(|b| b.is_ascii_digit()) => {
            out.push_str(&d.to_string());
        }
        Value::Array(items) => {
            out.push('[');
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, x);
            }
            out.push(']');
        }
        Value::Document(d) => {
            out.push('{');
            for (i, (k, x)) in d.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, k);
                out.push(':');
                write_value(out, x);
            }
            out.push('}');
        }
        other => write_string(out, &scalar_text(other).unwrap_or_default()),
    }
}
